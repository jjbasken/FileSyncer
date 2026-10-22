use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Side {
    Local,
    Remote,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    pub size: u64,
    pub mtime: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Carries no hash: `apply` hashes the bytes as it copies them, which
    /// both saves a second read and records what was actually transferred.
    Copy {
        rel_path: String,
        from: Side,
        size: u64,
    },
    Trash {
        rel_path: String,
        side: Side,
    },
    RecordState {
        rel_path: String,
        /// `None` when the pair was trusted on size and mtime unread.
        hash: Option<String>,
        local: Stat,
        remote: Stat,
    },
    DropState {
        rel_path: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    BothEdited,
    BothNew,
    EditedLocalDeletedRemote,
    EditedRemoteDeletedLocal,
    TypeMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub rel_path: String,
    pub kind: ConflictKind,
    pub local: Option<Stat>,
    pub remote: Option<Stat>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    Symlink,
    Unreadable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip {
    pub rel_path: String,
    pub reason: SkipReason,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub actions: Vec<Action>,
    pub conflicts: Vec<Conflict>,
    pub skips: Vec<Skip>,
    /// Every path the plan copies, trashes or flags, as it was on each side
    /// when scanned. Kept when a path is skipped, so a frontend can still show
    /// it and its choice can be changed back.
    pub listed: BTreeMap<String, Listed>,
}

/// A path as the scan found it on each side.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listed {
    pub local: Option<Stat>,
    pub remote: Option<Stat>,
    /// Set when reconciliation could not decide the path itself.
    pub conflict: Option<ConflictKind>,
}

impl Listed {
    pub fn stat(&self, side: Side) -> Option<&Stat> {
        match side {
            Side::Local => self.local.as_ref(),
            Side::Remote => self.remote.as_ref(),
        }
    }
}

/// What to do with one listed path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Copy from this side over the other.
    Copy(Side),
    /// Move this side's file to the trash.
    Trash(Side),
    /// Leave both sides alone this run; the path is planned again next scan.
    Skip,
}

impl Side {
    pub fn other(self) -> Side {
        match self {
            Side::Local => Side::Remote,
            Side::Remote => Side::Local,
        }
    }
}

impl Plan {
    /// True when applying this plan would not touch either filesystem.
    pub fn is_noop(&self) -> bool {
        !self
            .actions
            .iter()
            .any(|a| matches!(a, Action::Copy { .. } | Action::Trash { .. }))
    }

    pub fn copy_count(&self, to: Side) -> usize {
        self.actions
            .iter()
            .filter(|a| matches!(a, Action::Copy { from, .. } if from.other() == to))
            .count()
    }

    pub fn copy_bytes(&self, to: Side) -> u64 {
        self.actions
            .iter()
            .filter_map(|a| match a {
                Action::Copy { from, size, .. } if from.other() == to => Some(*size),
                _ => None,
            })
            .sum()
    }

    pub fn trash_count(&self) -> usize {
        self.actions
            .iter()
            .filter(|a| matches!(a, Action::Trash { .. }))
            .count()
    }
}

/// The path an action concerns.
pub fn action_path(action: &Action) -> &str {
    match action {
        Action::Copy { rel_path, .. }
        | Action::Trash { rel_path, .. }
        | Action::RecordState { rel_path, .. }
        | Action::DropState { rel_path } => rel_path,
    }
}

/// A plan deleting more than this share of tracked paths is refused.
pub const DELETION_THRESHOLD_PCT: usize = 25;

impl Plan {
    /// Refuse plans that would delete an implausible share of tracked paths.
    /// The usual cause is an unmounted share scanning as an empty directory.
    pub fn check_deletion_threshold(&self, tracked: usize) -> crate::Result<()> {
        let would_delete = self.trash_count();
        if would_delete == 0 || tracked == 0 {
            return Ok(());
        }
        if would_delete * 100 > tracked * DELETION_THRESHOLD_PCT {
            return Err(crate::Error::DeletionThreshold {
                would_delete,
                tracked,
            });
        }
        Ok(())
    }

    /// What the plan will do with a listed path.
    pub fn choice(&self, rel_path: &str) -> Choice {
        self.actions
            .iter()
            .find_map(|a| match a {
                Action::Copy {
                    rel_path: p, from, ..
                } if p == rel_path => Some(Choice::Copy(*from)),
                Action::Trash { rel_path: p, side } if p == rel_path => Some(Choice::Trash(*side)),
                _ => None,
            })
            .unwrap_or(Choice::Skip)
    }

    /// The choices open for a listed path, in display order.
    ///
    /// A side's file can be copied over the other whenever it exists. It can be
    /// trashed only when the other side has none: the state records the path
    /// as on both sides, so trashing one of two copies would read as a
    /// deletion next scan and trash the other too.
    pub fn choices(&self, rel_path: &str) -> Vec<Choice> {
        let Some(listed) = self.listed.get(rel_path) else {
            return Vec::new();
        };
        // A file cannot be copied over a directory, nor its absence trusted.
        if listed.conflict == Some(ConflictKind::TypeMismatch) {
            return vec![Choice::Skip];
        }
        let mut out = Vec::new();
        for side in [Side::Local, Side::Remote] {
            if listed.stat(side).is_some() {
                out.push(Choice::Copy(side));
            }
        }
        for side in [Side::Local, Side::Remote] {
            if listed.stat(side).is_some() && listed.stat(side.other()).is_none() {
                out.push(Choice::Trash(side));
            }
        }
        out.push(Choice::Skip);
        out
    }

    /// Replace what the plan does with a listed path.
    ///
    /// Skipping a conflict leaves it flagged; choosing anything else for one
    /// resolves it.
    pub fn set_choice(&mut self, rel_path: &str, choice: Choice) -> crate::Result<()> {
        if !self.choices(rel_path).contains(&choice) {
            return Err(crate::Error::ChoiceUnavailable(rel_path.to_string()));
        }
        let listed = self.listed[rel_path].clone();
        self.actions.retain(|a| {
            !(action_path(a) == rel_path && matches!(a, Action::Copy { .. } | Action::Trash { .. }))
        });
        self.conflicts.retain(|c| c.rel_path != rel_path);

        match choice {
            Choice::Copy(from) => self.actions.push(Action::Copy {
                rel_path: rel_path.to_string(),
                from,
                size: listed.stat(from).map_or(0, |s| s.size),
            }),
            Choice::Trash(side) => self.actions.push(Action::Trash {
                rel_path: rel_path.to_string(),
                side,
            }),
            Choice::Skip => {
                if let Some(kind) = listed.conflict {
                    self.conflicts.push(Conflict {
                        rel_path: rel_path.to_string(),
                        kind,
                        local: listed.local,
                        remote: listed.remote,
                    });
                }
            }
        }
        Ok(())
    }

    /// Turn a flagged conflict into a concrete action by choosing a winner.
    /// Keeping a side that holds no file means keeping a deletion.
    pub fn resolve_conflict(&mut self, rel_path: &str, keep: Side) -> crate::Result<()> {
        let index = self
            .conflicts
            .iter()
            .position(|c| c.rel_path == rel_path)
            .ok_or_else(|| crate::Error::NoSuchConflict(rel_path.to_string()))?;
        let conflict = self.conflicts.remove(index);

        let winner = match keep {
            Side::Local => conflict.local.as_ref(),
            Side::Remote => conflict.remote.as_ref(),
        };

        match winner {
            Some(stat) => self.actions.push(Action::Copy {
                rel_path: conflict.rel_path,
                from: keep,
                size: stat.size,
            }),
            None => self.actions.push(Action::Trash {
                rel_path: conflict.rel_path,
                side: keep.other(),
            }),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copy(rel: &str, from: Side, size: u64) -> Action {
        Action::Copy {
            rel_path: rel.into(),
            from,
            size,
        }
    }

    #[test]
    fn side_other_flips() {
        assert_eq!(Side::Local.other(), Side::Remote);
        assert_eq!(Side::Remote.other(), Side::Local);
    }

    #[test]
    fn empty_plan_is_a_noop() {
        assert!(Plan::default().is_noop());
    }

    #[test]
    fn state_only_actions_are_a_noop() {
        let plan = Plan {
            actions: vec![
                Action::DropState {
                    rel_path: "a".into(),
                },
                Action::RecordState {
                    rel_path: "b".into(),
                    hash: Some("h".into()),
                    local: Stat { size: 1, mtime: 2 },
                    remote: Stat { size: 1, mtime: 3 },
                },
            ],
            ..Default::default()
        };
        assert!(
            plan.is_noop(),
            "state bookkeeping does not touch either tree"
        );
    }

    #[test]
    fn a_copy_makes_the_plan_non_noop() {
        let plan = Plan {
            actions: vec![copy("a", Side::Local, 5)],
            ..Default::default()
        };
        assert!(!plan.is_noop());
    }

    #[test]
    fn conflicts_alone_are_a_noop() {
        let plan = Plan {
            conflicts: vec![Conflict {
                rel_path: "a".into(),
                kind: ConflictKind::BothEdited,
                local: Some(Stat { size: 1, mtime: 1 }),
                remote: Some(Stat { size: 2, mtime: 2 }),
            }],
            ..Default::default()
        };
        assert!(plan.is_noop(), "unresolved conflicts are never acted on");
    }

    #[test]
    fn copy_counts_are_reported_by_destination_not_source() {
        let plan = Plan {
            actions: vec![
                copy("a", Side::Local, 10),
                copy("b", Side::Local, 20),
                copy("c", Side::Remote, 100),
            ],
            ..Default::default()
        };
        assert_eq!(plan.copy_count(Side::Remote), 2);
        assert_eq!(plan.copy_bytes(Side::Remote), 30);
        assert_eq!(plan.copy_count(Side::Local), 1);
        assert_eq!(plan.copy_bytes(Side::Local), 100);
    }

    #[test]
    fn trash_count_totals_both_sides() {
        let plan = Plan {
            actions: vec![
                Action::Trash {
                    rel_path: "a".into(),
                    side: Side::Local,
                },
                Action::Trash {
                    rel_path: "b".into(),
                    side: Side::Remote,
                },
            ],
            ..Default::default()
        };
        assert_eq!(plan.trash_count(), 2);
    }

    fn conflict(
        rel: &str,
        kind: ConflictKind,
        local: Option<Stat>,
        remote: Option<Stat>,
    ) -> Conflict {
        Conflict {
            rel_path: rel.into(),
            kind,
            local,
            remote,
        }
    }

    #[test]
    fn a_plan_with_no_deletions_always_passes_the_threshold() {
        let plan = Plan {
            actions: vec![copy("a", Side::Local, 1)],
            ..Default::default()
        };
        plan.check_deletion_threshold(4).unwrap();
    }

    #[test]
    fn deleting_a_quarter_or_less_passes() {
        let plan = Plan {
            actions: vec![Action::Trash {
                rel_path: "a".into(),
                side: Side::Local,
            }],
            ..Default::default()
        };
        plan.check_deletion_threshold(4).unwrap();
    }

    #[test]
    fn deleting_more_than_a_quarter_is_refused() {
        let plan = Plan {
            actions: vec![
                Action::Trash {
                    rel_path: "a".into(),
                    side: Side::Local,
                },
                Action::Trash {
                    rel_path: "b".into(),
                    side: Side::Local,
                },
            ],
            ..Default::default()
        };
        let err = plan.check_deletion_threshold(4).unwrap_err();
        assert!(
            matches!(err, crate::Error::DeletionThreshold { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn an_unmounted_share_wiping_everything_is_refused() {
        let actions = (0..100)
            .map(|i| Action::Trash {
                rel_path: format!("f{i}"),
                side: Side::Local,
            })
            .collect();
        let plan = Plan {
            actions,
            ..Default::default()
        };
        assert!(plan.check_deletion_threshold(100).is_err());
    }

    #[test]
    fn an_empty_state_table_cannot_trip_the_threshold() {
        // First-ever run: nothing is tracked, so nothing can be a deletion.
        Plan::default().check_deletion_threshold(0).unwrap();
    }

    fn stat(size: u64) -> Option<Stat> {
        Some(Stat { size, mtime: 1 })
    }

    /// A plan listing one path, with the given action or conflict for it.
    fn listing(local: Option<Stat>, remote: Option<Stat>, action: Option<Action>) -> Plan {
        let conflict = action.is_none().then_some(ConflictKind::BothEdited);
        Plan {
            actions: action.into_iter().collect(),
            conflicts: conflict
                .map(|kind| self::conflict("a.txt", kind, local.clone(), remote.clone()))
                .into_iter()
                .collect(),
            listed: [(
                "a.txt".to_string(),
                Listed {
                    local,
                    remote,
                    conflict,
                },
            )]
            .into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_file_on_one_side_can_be_copied_trashed_or_skipped() {
        let plan = listing(stat(5), None, Some(copy("a.txt", Side::Local, 5)));
        assert_eq!(plan.choice("a.txt"), Choice::Copy(Side::Local));
        assert_eq!(
            plan.choices("a.txt"),
            vec![
                Choice::Copy(Side::Local),
                Choice::Trash(Side::Local),
                Choice::Skip
            ]
        );
    }

    #[test]
    fn a_file_on_both_sides_cannot_have_one_copy_trashed() {
        let plan = listing(stat(5), stat(6), Some(copy("a.txt", Side::Local, 5)));
        assert_eq!(
            plan.choices("a.txt"),
            vec![
                Choice::Copy(Side::Local),
                Choice::Copy(Side::Remote),
                Choice::Skip
            ]
        );
        let mut plan = plan;
        assert!(
            plan.set_choice("a.txt", Choice::Trash(Side::Local))
                .is_err()
        );
    }

    #[test]
    fn reversing_a_copy_sizes_it_from_the_other_side() {
        let mut plan = listing(stat(5), stat(6), Some(copy("a.txt", Side::Local, 5)));
        plan.set_choice("a.txt", Choice::Copy(Side::Remote))
            .unwrap();
        assert_eq!(plan.actions, vec![copy("a.txt", Side::Remote, 6)]);
    }

    #[test]
    fn trashing_instead_of_copying_replaces_the_action() {
        let mut plan = listing(stat(5), None, Some(copy("a.txt", Side::Local, 5)));
        plan.set_choice("a.txt", Choice::Trash(Side::Local))
            .unwrap();
        assert_eq!(
            plan.actions,
            vec![Action::Trash {
                rel_path: "a.txt".into(),
                side: Side::Local
            }]
        );
    }

    #[test]
    fn skipping_removes_the_action_but_keeps_the_path_listed() {
        let mut plan = listing(stat(5), None, Some(copy("a.txt", Side::Local, 5)));
        plan.set_choice("a.txt", Choice::Skip).unwrap();
        assert!(plan.actions.is_empty());
        assert_eq!(plan.choice("a.txt"), Choice::Skip);
        plan.set_choice("a.txt", Choice::Copy(Side::Local)).unwrap();
        assert_eq!(plan.actions, vec![copy("a.txt", Side::Local, 5)]);
    }

    #[test]
    fn choosing_for_a_conflict_resolves_it_and_skipping_flags_it_again() {
        let mut plan = listing(stat(5), stat(6), None);
        assert_eq!(plan.choice("a.txt"), Choice::Skip);
        plan.set_choice("a.txt", Choice::Copy(Side::Remote))
            .unwrap();
        assert!(plan.conflicts.is_empty());
        plan.set_choice("a.txt", Choice::Skip).unwrap();
        assert!(plan.actions.is_empty());
        assert_eq!(plan.conflicts[0].kind, ConflictKind::BothEdited);
    }

    #[test]
    fn state_rows_survive_a_change_of_choice() {
        let mut plan = listing(stat(5), None, Some(copy("a.txt", Side::Local, 5)));
        plan.actions.push(Action::DropState {
            rel_path: "a.txt".into(),
        });
        plan.set_choice("a.txt", Choice::Skip).unwrap();
        assert_eq!(plan.actions.len(), 1);
    }

    #[test]
    fn a_type_mismatch_can_only_be_skipped() {
        let mut plan = listing(stat(5), None, None);
        plan.listed.get_mut("a.txt").unwrap().conflict = Some(ConflictKind::TypeMismatch);
        assert_eq!(plan.choices("a.txt"), vec![Choice::Skip]);
    }

    #[test]
    fn an_unlisted_path_has_no_choices() {
        let mut plan = Plan::default();
        assert!(plan.choices("ghost").is_empty());
        assert!(plan.set_choice("ghost", Choice::Skip).is_err());
    }

    #[test]
    fn keeping_local_turns_a_both_edited_conflict_into_a_copy() {
        let mut plan = Plan {
            conflicts: vec![conflict(
                "a.txt",
                ConflictKind::BothEdited,
                Some(Stat {
                    size: 20,
                    mtime: 900,
                }),
                Some(Stat {
                    size: 30,
                    mtime: 901,
                }),
            )],
            ..Default::default()
        };
        plan.resolve_conflict("a.txt", Side::Local).unwrap();
        assert!(plan.conflicts.is_empty());
        assert_eq!(
            plan.actions,
            vec![Action::Copy {
                rel_path: "a.txt".into(),
                from: Side::Local,
                size: 20,
            }]
        );
    }

    #[test]
    fn keeping_remote_copies_the_other_way() {
        let mut plan = Plan {
            conflicts: vec![conflict(
                "a.txt",
                ConflictKind::BothEdited,
                Some(Stat {
                    size: 20,
                    mtime: 900,
                }),
                Some(Stat {
                    size: 30,
                    mtime: 901,
                }),
            )],
            ..Default::default()
        };
        plan.resolve_conflict("a.txt", Side::Remote).unwrap();
        assert_eq!(plan.copy_count(Side::Local), 1);
    }

    #[test]
    fn keeping_the_deleted_side_trashes_the_survivor() {
        // Local edited it, remote deleted it, user sides with the deletion.
        let mut plan = Plan {
            conflicts: vec![conflict(
                "a.txt",
                ConflictKind::EditedLocalDeletedRemote,
                Some(Stat {
                    size: 20,
                    mtime: 900,
                }),
                None,
            )],
            ..Default::default()
        };
        plan.resolve_conflict("a.txt", Side::Remote).unwrap();
        assert_eq!(
            plan.actions,
            vec![Action::Trash {
                rel_path: "a.txt".into(),
                side: Side::Local
            }]
        );
    }

    #[test]
    fn keeping_the_surviving_side_of_a_delete_conflict_copies_it_back() {
        let mut plan = Plan {
            conflicts: vec![conflict(
                "a.txt",
                ConflictKind::EditedLocalDeletedRemote,
                Some(Stat {
                    size: 20,
                    mtime: 900,
                }),
                None,
            )],
            ..Default::default()
        };
        plan.resolve_conflict("a.txt", Side::Local).unwrap();
        assert_eq!(plan.copy_count(Side::Remote), 1);
    }

    #[test]
    fn resolving_an_unknown_path_is_an_error() {
        let mut plan = Plan::default();
        let err = plan.resolve_conflict("ghost.txt", Side::Local).unwrap_err();
        assert!(
            matches!(err, crate::Error::NoSuchConflict(_)),
            "got: {err:?}"
        );
    }

    #[test]
    fn resolving_one_conflict_leaves_the_others_alone() {
        let mut plan = Plan {
            conflicts: vec![
                conflict(
                    "a.txt",
                    ConflictKind::BothEdited,
                    Some(Stat { size: 1, mtime: 1 }),
                    Some(Stat { size: 2, mtime: 2 }),
                ),
                conflict(
                    "b.txt",
                    ConflictKind::BothEdited,
                    Some(Stat { size: 1, mtime: 1 }),
                    Some(Stat { size: 2, mtime: 2 }),
                ),
            ],
            ..Default::default()
        };
        plan.resolve_conflict("a.txt", Side::Local).unwrap();
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.conflicts[0].rel_path, "b.txt");
    }
}
