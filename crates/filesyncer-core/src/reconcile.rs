use crate::plan::{Action, Conflict, ConflictKind, Plan, Side, Skip, SkipReason, Stat};
use crate::scan::Scan;
use crate::state::StateRow;
use crate::{Error, Result};
use std::collections::{BTreeMap, BTreeSet};

/// Supplies content hashes on demand. Injected so the decision logic itself
/// performs no I/O and every branch is testable without a filesystem.
pub trait HashProvider {
    fn hash(&self, side: Side, rel_path: &str) -> Result<String>;
}

/// A `HashProvider` backed by a lookup table, for tests.
#[doc(hidden)]
pub struct MapHashProvider(pub BTreeMap<(Side, String), String>);

impl HashProvider for MapHashProvider {
    fn hash(&self, side: Side, rel_path: &str) -> Result<String> {
        self.0
            .get(&(side, rel_path.to_string()))
            .cloned()
            .ok_or_else(|| Error::Config(format!("test hash missing for {side:?} {rel_path}")))
    }
}

pub fn reconcile(
    local: &Scan,
    remote: &Scan,
    state: &BTreeMap<String, StateRow>,
    hashes: &dyn HashProvider,
) -> Result<Plan> {
    let mut plan = Plan::default();

    collect_skips(local, &mut plan);
    collect_skips(remote, &mut plan);

    let mut keys: BTreeSet<&str> = BTreeSet::new();
    keys.extend(local.files.keys().map(String::as_str));
    keys.extend(remote.files.keys().map(String::as_str));
    keys.extend(state.keys().map(String::as_str));

    for key in keys {
        // A path that is a file on one side and a directory on the other can
        // never be reconciled by copying; refuse to touch it.
        if local.dirs.contains(key) || remote.dirs.contains(key) {
            plan.conflicts.push(Conflict {
                rel_path: key.to_string(),
                kind: ConflictKind::TypeMismatch,
                local: local.files.get(key).map(stat_of),
                remote: remote.files.get(key).map(stat_of),
            });
            continue;
        }

        let l = local.files.get(key);
        let r = remote.files.get(key);
        let s = state.get(key);

        match (l, r, s) {
            (Some(l), Some(r), Some(s)) => {
                let l_suspect = l.size != s.local_size || l.mtime != s.local_mtime;
                let r_suspect = r.size != s.remote_size || r.mtime != s.remote_mtime;
                if !l_suspect && !r_suspect {
                    continue;
                }

                let lh = if l_suspect {
                    hashes.hash(Side::Local, key)?
                } else {
                    s.hash.clone()
                };
                let rh = if r_suspect {
                    hashes.hash(Side::Remote, key)?
                } else {
                    s.hash.clone()
                };

                match (lh != s.hash, rh != s.hash) {
                    (false, false) => plan.actions.push(Action::RecordState {
                        rel_path: key.to_string(),
                        hash: s.hash.clone(),
                        local: stat_of(l),
                        remote: stat_of(r),
                    }),
                    (true, false) => plan.actions.push(Action::Copy {
                        rel_path: key.to_string(),
                        from: Side::Local,
                        size: l.size,
                        hash: lh,
                    }),
                    (false, true) => plan.actions.push(Action::Copy {
                        rel_path: key.to_string(),
                        from: Side::Remote,
                        size: r.size,
                        hash: rh,
                    }),
                    (true, true) => {
                        if lh == rh {
                            plan.actions.push(Action::RecordState {
                                rel_path: key.to_string(),
                                hash: lh,
                                local: stat_of(l),
                                remote: stat_of(r),
                            });
                        } else {
                            plan.conflicts.push(Conflict {
                                rel_path: key.to_string(),
                                kind: ConflictKind::BothEdited,
                                local: Some(stat_of(l)),
                                remote: Some(stat_of(r)),
                            });
                        }
                    }
                }
            }

            (Some(l), Some(r), None) => {
                let lh = hashes.hash(Side::Local, key)?;
                let rh = hashes.hash(Side::Remote, key)?;
                if lh == rh {
                    plan.actions.push(Action::RecordState {
                        rel_path: key.to_string(),
                        hash: lh,
                        local: stat_of(l),
                        remote: stat_of(r),
                    });
                } else {
                    plan.conflicts.push(Conflict {
                        rel_path: key.to_string(),
                        kind: ConflictKind::BothNew,
                        local: Some(stat_of(l)),
                        remote: Some(stat_of(r)),
                    });
                }
            }

            // Task 10 fills in the remaining arms.
            _ => {}
        }
    }

    Ok(plan)
}

fn stat_of(e: &crate::scan::Entry) -> Stat {
    Stat {
        size: e.size,
        mtime: e.mtime,
    }
}

fn collect_skips(scan: &Scan, plan: &mut Plan) {
    for path in &scan.symlinks {
        plan.skips.push(Skip {
            rel_path: path.clone(),
            reason: SkipReason::Symlink,
        });
    }
    for (path, message) in &scan.errors {
        plan.skips.push(Skip {
            rel_path: path.clone(),
            reason: SkipReason::Unreadable(message.clone()),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{Entry, EntryKind};

    /// Build a scan containing one file per (path, size, mtime) triple.
    fn scan_of(items: &[(&str, u64, i64)]) -> Scan {
        let mut s = Scan::default();
        for (path, size, mtime) in items {
            s.files.insert(
                path.to_string(),
                Entry {
                    rel_path: path.to_string(),
                    kind: EntryKind::File,
                    size: *size,
                    mtime: *mtime,
                },
            );
        }
        s
    }

    fn state_of(items: &[(&str, &str, u64, i64, u64, i64)]) -> BTreeMap<String, StateRow> {
        items
            .iter()
            .map(|(p, h, ls, lm, rs, rm)| {
                (
                    p.to_string(),
                    StateRow {
                        rel_path: p.to_string(),
                        hash: h.to_string(),
                        local_size: *ls,
                        local_mtime: *lm,
                        remote_size: *rs,
                        remote_mtime: *rm,
                    },
                )
            })
            .collect()
    }

    fn hashes(items: &[(Side, &str, &str)]) -> MapHashProvider {
        MapHashProvider(
            items
                .iter()
                .map(|(s, p, h)| ((*s, p.to_string()), h.to_string()))
                .collect(),
        )
    }

    fn no_hashes() -> MapHashProvider {
        MapHashProvider(BTreeMap::new())
    }

    #[test]
    fn unchanged_on_both_sides_produces_nothing() {
        let local = scan_of(&[("a.txt", 10, 100)]);
        let remote = scan_of(&[("a.txt", 10, 200)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let plan = reconcile(&local, &remote, &state, &no_hashes()).unwrap();
        assert!(plan.actions.is_empty(), "got: {:?}", plan.actions);
        assert!(plan.conflicts.is_empty());
    }

    #[test]
    fn local_edit_copies_to_remote() {
        let local = scan_of(&[("a.txt", 12, 150)]);
        let remote = scan_of(&[("a.txt", 10, 200)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Local, "a.txt", "h2")]);
        let plan = reconcile(&local, &remote, &state, &h).unwrap();
        assert_eq!(
            plan.actions,
            vec![Action::Copy {
                rel_path: "a.txt".into(),
                from: Side::Local,
                size: 12,
                hash: "h2".into(),
            }]
        );
    }

    #[test]
    fn remote_edit_copies_to_local() {
        let local = scan_of(&[("a.txt", 10, 100)]);
        let remote = scan_of(&[("a.txt", 14, 250)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Remote, "a.txt", "h3")]);
        let plan = reconcile(&local, &remote, &state, &h).unwrap();
        assert_eq!(
            plan.actions,
            vec![Action::Copy {
                rel_path: "a.txt".into(),
                from: Side::Remote,
                size: 14,
                hash: "h3".into(),
            }]
        );
    }

    #[test]
    fn mtime_drift_without_content_change_records_state_and_copies_nothing() {
        // The remount case: mtime shifted on both sides, contents identical.
        let local = scan_of(&[("a.txt", 10, 900)]);
        let remote = scan_of(&[("a.txt", 10, 901)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Local, "a.txt", "h1"), (Side::Remote, "a.txt", "h1")]);
        let plan = reconcile(&local, &remote, &state, &h).unwrap();
        assert_eq!(
            plan.actions,
            vec![Action::RecordState {
                rel_path: "a.txt".into(),
                hash: "h1".into(),
                local: Stat {
                    size: 10,
                    mtime: 900
                },
                remote: Stat {
                    size: 10,
                    mtime: 901
                },
            }]
        );
        assert!(plan.is_noop(), "drift must not schedule any transfer");
    }

    #[test]
    fn both_edited_to_the_same_content_converges_without_transfer() {
        let local = scan_of(&[("a.txt", 20, 900)]);
        let remote = scan_of(&[("a.txt", 20, 901)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Local, "a.txt", "h9"), (Side::Remote, "a.txt", "h9")]);
        let plan = reconcile(&local, &remote, &state, &h).unwrap();
        assert_eq!(
            plan.actions,
            vec![Action::RecordState {
                rel_path: "a.txt".into(),
                hash: "h9".into(),
                local: Stat {
                    size: 20,
                    mtime: 900
                },
                remote: Stat {
                    size: 20,
                    mtime: 901
                },
            }]
        );
    }

    #[test]
    fn both_edited_differently_is_a_conflict() {
        let local = scan_of(&[("a.txt", 20, 900)]);
        let remote = scan_of(&[("a.txt", 30, 901)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Local, "a.txt", "hL"), (Side::Remote, "a.txt", "hR")]);
        let plan = reconcile(&local, &remote, &state, &h).unwrap();
        assert!(plan.actions.is_empty());
        assert_eq!(
            plan.conflicts,
            vec![Conflict {
                rel_path: "a.txt".into(),
                kind: ConflictKind::BothEdited,
                local: Some(Stat {
                    size: 20,
                    mtime: 900
                }),
                remote: Some(Stat {
                    size: 30,
                    mtime: 901
                }),
            }]
        );
    }

    #[test]
    fn new_on_both_sides_with_identical_content_is_adopted() {
        let local = scan_of(&[("a.txt", 10, 100)]);
        let remote = scan_of(&[("a.txt", 10, 200)]);
        let h = hashes(&[(Side::Local, "a.txt", "hX"), (Side::Remote, "a.txt", "hX")]);
        let plan = reconcile(&local, &remote, &BTreeMap::new(), &h).unwrap();
        assert_eq!(
            plan.actions,
            vec![Action::RecordState {
                rel_path: "a.txt".into(),
                hash: "hX".into(),
                local: Stat {
                    size: 10,
                    mtime: 100
                },
                remote: Stat {
                    size: 10,
                    mtime: 200
                },
            }]
        );
    }

    #[test]
    fn new_on_both_sides_with_different_content_is_a_conflict() {
        let local = scan_of(&[("a.txt", 10, 100)]);
        let remote = scan_of(&[("a.txt", 11, 200)]);
        let h = hashes(&[(Side::Local, "a.txt", "hL"), (Side::Remote, "a.txt", "hR")]);
        let plan = reconcile(&local, &remote, &BTreeMap::new(), &h).unwrap();
        assert_eq!(plan.conflicts[0].kind, ConflictKind::BothNew);
    }

    #[test]
    fn file_on_one_side_directory_on_the_other_is_a_type_mismatch() {
        let local = scan_of(&[("thing", 10, 100)]);
        let mut remote = Scan::default();
        remote.dirs.insert("thing".to_string());
        remote.files.insert(
            "thing/inner.txt".to_string(),
            Entry {
                rel_path: "thing/inner.txt".into(),
                kind: EntryKind::File,
                size: 1,
                mtime: 5,
            },
        );
        let plan = reconcile(&local, &remote, &BTreeMap::new(), &no_hashes()).unwrap();
        assert!(
            plan.conflicts
                .iter()
                .any(|c| c.rel_path == "thing" && c.kind == ConflictKind::TypeMismatch),
            "got: {:?}",
            plan.conflicts
        );
    }

    #[test]
    fn symlinks_and_errors_become_skips() {
        let mut local = scan_of(&[]);
        local.symlinks.push("link.txt".into());
        local
            .errors
            .push(("bad.txt".into(), "permission denied".into()));
        let plan = reconcile(&local, &Scan::default(), &BTreeMap::new(), &no_hashes()).unwrap();
        assert!(
            plan.skips
                .iter()
                .any(|s| s.rel_path == "link.txt" && s.reason == SkipReason::Symlink)
        );
        assert!(
            plan.skips
                .iter()
                .any(|s| s.rel_path == "bad.txt" && matches!(s.reason, SkipReason::Unreadable(_)))
        );
    }

    #[test]
    fn a_skipped_symlink_is_reported_once_per_side() {
        let mut local = Scan::default();
        local.symlinks.push("link.txt".into());
        let mut remote = Scan::default();
        remote.symlinks.push("link.txt".into());
        let plan = reconcile(&local, &remote, &BTreeMap::new(), &no_hashes()).unwrap();
        assert_eq!(plan.skips.len(), 2, "got: {:?}", plan.skips);
    }
}
