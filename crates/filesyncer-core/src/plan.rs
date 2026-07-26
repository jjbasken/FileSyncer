#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}
