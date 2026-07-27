#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
    Copy {
        rel_path: String,
        from: Side,
        size: u64,
        hash: String,
    },
    Trash {
        rel_path: String,
        side: Side,
    },
    RecordState {
        rel_path: String,
        hash: String,
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
                // Not precomputed: `apply` hashes the source as it copies.
                hash: String::new(),
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
            hash: "h".into(),
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
                    hash: "h".into(),
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
                hash: String::new(),
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
