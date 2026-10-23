use crate::plan::{Action, Conflict, ConflictKind, Plan, Side, Skip, SkipReason, Stat};
use crate::progress::{CANCEL_POLL, Phase, Reporter};
use crate::scan::{Entry, Scan};
use crate::state::StateRow;
use crate::{Error, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;

/// Upper bound on hashing threads. Past this, a spinning disk spends its time
/// seeking between files and a network share gains little more overlap.
const MAX_HASH_THREADS: usize = 8;

/// Whether content comparisons may trust recorded metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compare {
    /// Equal size and mtime are taken as equal content, as rsync does by
    /// default. Only the local copy is read, for the hash the state records;
    /// on a first sync against a slow share that skips reading the share.
    #[default]
    Quick,
    /// Read every existing copy of paired or tracked files, even when its
    /// metadata matches the saved state.
    Verify,
}

/// Supplies content hashes on demand. Injected so the decision logic itself
/// performs no I/O and every branch is testable without a filesystem.
///
/// `Sync` because hashes are requested from several threads at once.
pub trait HashProvider: Sync {
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

/// Which sides of a path must be hashed before it can be decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Needs {
    local: bool,
    remote: bool,
}

impl Needs {
    const NONE: Needs = Needs {
        local: false,
        remote: false,
    };
    fn any(self) -> bool {
        self.local || self.remote
    }

    fn sides(self) -> impl Iterator<Item = Side> {
        [(self.local, Side::Local), (self.remote, Side::Remote)]
            .into_iter()
            .filter_map(|(needed, side)| needed.then_some(side))
    }
}

/// Hashes read ahead of the decisions, keyed by side and path.
type Hashes<'k> = HashMap<(Side, &'k str), String>;

/// Returns `Ok(None)` when the comparison was cancelled.
pub fn reconcile(
    local: &Scan,
    remote: &Scan,
    state: &BTreeMap<String, StateRow>,
    hashes: &dyn HashProvider,
    compare: Compare,
    reporter: &Reporter,
) -> Result<Option<Plan>> {
    let mut plan = Plan::default();

    collect_skips(local, &mut plan);
    collect_skips(remote, &mut plan);

    let mut keys: BTreeSet<&str> = BTreeSet::new();
    keys.extend(local.files.keys().map(String::as_str));
    keys.extend(remote.files.keys().map(String::as_str));
    keys.extend(state.keys().map(String::as_str));

    // Hashing is the only expensive part of reconciliation, and which hashes
    // are needed is decided by size and mtime alone — a pure read of the scans
    // and the state map. So a pre-pass lists them, which gives this phase a
    // real n/total and lets every file be read in parallel before any
    // decision is made. The decisions themselves then perform no I/O.
    let jobs: Vec<(&str, Needs)> = keys
        .iter()
        .filter_map(|&key| {
            let needs = needs(key, local, remote, state, compare);
            needs.any().then_some((key, needs))
        })
        .collect();

    let Some(mut known) = hash_all(&jobs, hashes, reporter)? else {
        return Ok(None);
    };

    for key in keys {
        decide(key, local, remote, state, &mut known, &mut plan);
    }
    Ok(Some(plan))
}

/// Hash every (side, path) in `jobs` on a pool of threads.
///
/// Workers only read files; the calling thread owns the `Reporter`, which is
/// not thread-safe, and reports each path as its hashes arrive. Results arrive
/// in completion order, so item order is not path order.
fn hash_all<'k>(
    jobs: &[(&'k str, Needs)],
    hashes: &dyn HashProvider,
    reporter: &Reporter,
) -> Result<Option<Hashes<'k>>> {
    reporter.started(Phase::Compare, Some(jobs.len()));
    if reporter.cancelled() {
        return Ok(None);
    }

    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel::<(usize, Result<Vec<(Side, String)>>)>();
    let workers = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .clamp(2, MAX_HASH_THREADS)
        .min(jobs.len());

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let tx = tx.clone();
            let (next, stop) = (&next, &stop);
            scope.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(key, needs)) = jobs.get(index) else {
                        break;
                    };
                    let result = needs
                        .sides()
                        .map(|side| hashes.hash(side, key).map(|h| (side, h)))
                        .collect();
                    if tx.send((index, result)).is_err() {
                        break;
                    }
                }
            });
        }
        // Only the workers' clones remain, so the channel closes once they
        // have all run out of jobs.
        drop(tx);

        // Returning early sets `stop`; the scope then waits for each worker to
        // finish the file it is on, which no longer delays anything visible.
        let mut known = Hashes::new();
        let mut done = 0usize;
        loop {
            match rx.recv_timeout(CANCEL_POLL) {
                Ok((index, result)) => {
                    let key = jobs[index].0;
                    match result {
                        Ok(pairs) => known.extend(pairs.into_iter().map(|(s, h)| ((s, key), h))),
                        Err(e) => {
                            stop.store(true, Ordering::Relaxed);
                            return Err(e);
                        }
                    }
                    reporter.item(Phase::Compare, done, key);
                    done += 1;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if reporter.cancelled() {
                stop.store(true, Ordering::Relaxed);
                return Ok(None);
            }
        }
        reporter.finished(Phase::Compare, done);
        Ok(Some(known))
    })
}

/// Which hashes deciding `key` will read.
///
/// This function alone chooses between reading a file and settling the path
/// from its size: `hash_all` reads exactly what it names, and `decide` reads
/// a missing hash as "resized, so changed", or for a pair with nothing
/// recorded, as "trusted on size and mtime". A size that differs from the recorded one proves the content
/// changed (the recorded hash was taken of a file of the recorded size); a
/// changed mtime alone does not.
fn needs(
    key: &str,
    local: &Scan,
    remote: &Scan,
    state: &BTreeMap<String, StateRow>,
    compare: Compare,
) -> Needs {
    // A type mismatch is settled without reading either file.
    if local.dirs.contains(key) || remote.dirs.contains(key) {
        return Needs::NONE;
    }
    if compare == Compare::Verify
        && (state.contains_key(key)
            || (local.files.contains_key(key) && remote.files.contains_key(key)))
    {
        return Needs {
            local: local.files.contains_key(key),
            remote: remote.files.contains_key(key),
        };
    }
    match (local.files.get(key), remote.files.get(key), state.get(key)) {
        (Some(l), Some(r), Some(s)) => match (local_suspect(l, s), remote_suspect(r, s)) {
            (false, false) => Needs::NONE,
            // A resized side has certainly changed and the other has not, so
            // it is copied; the copy hashes it.
            (true, false) => Needs {
                local: !local_resized(l, s),
                remote: false,
            },
            (false, true) => Needs {
                local: false,
                remote: !remote_resized(r, s),
            },
            // Both touched. A resized side has changed, and its hash only
            // matters if the other side changed too and might hold the same
            // bytes, which takes equal sizes.
            (true, true) => Needs {
                local: !local_resized(l, s) || l.size == r.size,
                remote: !remote_resized(r, s) || l.size == r.size,
            },
        },
        // Nothing recorded: only equal sizes can be equal content. A pair whose
        // mtimes match too is trusted unless verifying, and only the local
        // copy is read, for the hash to record.
        (Some(l), Some(r), None) => {
            let same_size = l.size == r.size;
            let trusted = compare == Compare::Quick && same_size && l.mtime == r.mtime;
            Needs {
                local: same_size,
                remote: same_size && !trusted,
            }
        }
        (Some(l), None, Some(s)) => Needs {
            local: local_suspect(l, s) && !local_resized(l, s),
            remote: false,
        },
        (None, Some(r), Some(s)) => Needs {
            local: false,
            remote: remote_suspect(r, s) && !remote_resized(r, s),
        },
        // New on one side: copied, and hashed by the copy.
        (Some(_), None, None) | (None, Some(_), None) => Needs::NONE,
        // Gone from both sides: the row is dropped without any I/O.
        (None, None, _) => Needs::NONE,
    }
}

/// Add the action or conflict for `key` to `plan`, using the hashes `needs`
/// asked for. `known` holds exactly those, so a missing one was not needed.
fn decide<'k>(
    key: &'k str,
    local: &Scan,
    remote: &Scan,
    state: &BTreeMap<String, StateRow>,
    known: &mut Hashes<'k>,
    plan: &mut Plan,
) {
    // A path that is a file on one side and a directory on the other can
    // never be reconciled by copying; refuse to touch it.
    if local.dirs.contains(key) || remote.dirs.contains(key) {
        plan.conflicts.push(Conflict {
            rel_path: key.to_string(),
            kind: ConflictKind::TypeMismatch,
            local: local.files.get(key).map(stat_of),
            remote: remote.files.get(key).map(stat_of),
        });
        return;
    }

    let lh = known.remove(&(Side::Local, key));
    let rh = known.remove(&(Side::Remote, key));

    let copy = |from: Side, e: &Entry| Action::Copy {
        rel_path: key.to_string(),
        from,
        size: e.size,
    };
    let conflict = |kind, l: Option<&Entry>, r: Option<&Entry>| Conflict {
        rel_path: key.to_string(),
        kind,
        local: l.map(stat_of),
        remote: r.map(stat_of),
    };
    let record = |hash: String, l: &Entry, r: &Entry| Action::RecordState {
        rel_path: key.to_string(),
        hash,
        local: stat_of(l),
        remote: stat_of(r),
    };
    // A suspect side with no hash was resized, which settles it as changed.
    let changed = |suspect: bool, hash: &Option<String>, s: &StateRow| {
        (suspect || hash.is_some()) && hash.as_ref().is_none_or(|h| *h != s.hash)
    };

    match (local.files.get(key), remote.files.get(key), state.get(key)) {
        (Some(l), Some(r), Some(s)) => {
            let (l_suspect, r_suspect) = (local_suspect(l, s), remote_suspect(r, s));
            match (changed(l_suspect, &lh, s), changed(r_suspect, &rh, s)) {
                // Only metadata drifted: remember the new stats so the next
                // scan does not hash again.
                (false, false) if l_suspect || r_suspect => {
                    plan.actions.push(record(s.hash.clone(), l, r))
                }
                (false, false) => {}
                (true, false) => plan.actions.push(copy(Side::Local, l)),
                (false, true) => plan.actions.push(copy(Side::Remote, r)),
                (true, true) => match (lh, rh) {
                    (Some(lh), Some(rh)) if lh == rh => plan.actions.push(record(lh, l, r)),
                    _ => plan
                        .conflicts
                        .push(conflict(ConflictKind::BothEdited, Some(l), Some(r))),
                },
            }
        }

        (Some(l), Some(r), None) => match (lh, rh) {
            (Some(lh), Some(rh)) if lh == rh => plan.actions.push(record(lh, l, r)),
            // Only a trusted pair is hashed on the local side alone.
            (Some(lh), None) => plan.actions.push(record(lh, l, r)),
            _ => plan
                .conflicts
                .push(conflict(ConflictKind::BothNew, Some(l), Some(r))),
        },

        (Some(l), None, Some(s)) => {
            if changed(local_suspect(l, s), &lh, s) {
                plan.conflicts.push(conflict(
                    ConflictKind::EditedLocalDeletedRemote,
                    Some(l),
                    None,
                ));
            } else {
                plan.actions.push(Action::Trash {
                    rel_path: key.to_string(),
                    side: Side::Local,
                });
            }
        }

        (None, Some(r), Some(s)) => {
            if changed(remote_suspect(r, s), &rh, s) {
                plan.conflicts.push(conflict(
                    ConflictKind::EditedRemoteDeletedLocal,
                    None,
                    Some(r),
                ));
            } else {
                plan.actions.push(Action::Trash {
                    rel_path: key.to_string(),
                    side: Side::Remote,
                });
            }
        }

        (Some(l), None, None) => plan.actions.push(copy(Side::Local, l)),

        (None, Some(r), None) => plan.actions.push(copy(Side::Remote, r)),

        (None, None, Some(_)) => plan.actions.push(Action::DropState {
            rel_path: key.to_string(),
        }),

        (None, None, None) => unreachable!("key came from one of the three sources"),
    }
}

fn local_suspect(l: &Entry, s: &StateRow) -> bool {
    l.size != s.local_size || l.mtime != s.local_mtime
}

fn remote_suspect(r: &Entry, s: &StateRow) -> bool {
    r.size != s.remote_size || r.mtime != s.remote_mtime
}

fn local_resized(l: &Entry, s: &StateRow) -> bool {
    l.size != s.local_size
}

fn remote_resized(r: &Entry, s: &StateRow) -> bool {
    r.size != s.remote_size
}

/// Hashes files on the real filesystem.
pub struct FsHashProvider {
    pub local_root: std::path::PathBuf,
    pub remote_root: std::path::PathBuf,
}

impl HashProvider for FsHashProvider {
    fn hash(&self, side: Side, rel_path: &str) -> Result<String> {
        let root = match side {
            Side::Local => &self.local_root,
            Side::Remote => &self.remote_root,
        };
        crate::hash::hash_file(&root.join(rel_path))
    }
}

fn stat_of(e: &Entry) -> Stat {
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

    /// Reconcile with no progress reporting, for the tests that only care about
    /// the resulting plan.
    fn plan_of(
        local: &Scan,
        remote: &Scan,
        state: &BTreeMap<String, StateRow>,
        hashes: &dyn HashProvider,
    ) -> Plan {
        reconcile(
            local,
            remote,
            state,
            hashes,
            Compare::Quick,
            &Reporter::silent(),
        )
        .unwrap()
        .expect("a silent reporter never cancels")
    }

    /// The events a reconciliation emits, for the progress tests.
    fn events_of(
        local: &Scan,
        remote: &Scan,
        state: &BTreeMap<String, StateRow>,
        hashes: &dyn HashProvider,
    ) -> Vec<crate::progress::Progress> {
        let events = std::cell::RefCell::new(Vec::new());
        let sink = |p| events.borrow_mut().push(p);
        reconcile(
            local,
            remote,
            state,
            hashes,
            Compare::Quick,
            &Reporter::new(&sink, &|| false),
        )
        .unwrap();
        events.into_inner()
    }

    fn compare_total(events: &[crate::progress::Progress]) -> Option<usize> {
        events.iter().find_map(|e| match e {
            crate::progress::Progress::Started { total, .. } => Some(*total),
            _ => None,
        })?
    }

    /// Sorted, since paths are reported in the order their hashes finish.
    fn compared_paths(events: &[crate::progress::Progress]) -> Vec<String> {
        let mut paths: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                crate::progress::Progress::Item { rel_path, .. } => Some(rel_path.clone()),
                _ => None,
            })
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn unchanged_on_both_sides_produces_nothing() {
        let local = scan_of(&[("a.txt", 10, 100)]);
        let remote = scan_of(&[("a.txt", 10, 200)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let plan = plan_of(&local, &remote, &state, &no_hashes());
        assert!(plan.actions.is_empty(), "got: {:?}", plan.actions);
        assert!(plan.conflicts.is_empty());
    }

    #[test]
    fn local_edit_copies_to_remote() {
        // Same size, so only the hash can tell the edit apart from drift.
        let local = scan_of(&[("a.txt", 10, 150)]);
        let remote = scan_of(&[("a.txt", 10, 200)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Local, "a.txt", "h2")]);
        let plan = plan_of(&local, &remote, &state, &h);
        assert_eq!(
            plan.actions,
            vec![Action::Copy {
                rel_path: "a.txt".into(),
                from: Side::Local,
                size: 10,
            }]
        );
    }

    #[test]
    fn remote_edit_copies_to_local() {
        let local = scan_of(&[("a.txt", 10, 100)]);
        let remote = scan_of(&[("a.txt", 10, 250)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Remote, "a.txt", "h3")]);
        let plan = plan_of(&local, &remote, &state, &h);
        assert_eq!(
            plan.actions,
            vec![Action::Copy {
                rel_path: "a.txt".into(),
                from: Side::Remote,
                size: 10,
            }]
        );
    }

    #[test]
    fn a_resized_local_file_is_copied_without_hashing() {
        let local = scan_of(&[("a.txt", 12, 150)]);
        let remote = scan_of(&[("a.txt", 10, 200)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let plan = plan_of(&local, &remote, &state, &no_hashes());
        assert_eq!(
            plan.actions,
            vec![Action::Copy {
                rel_path: "a.txt".into(),
                from: Side::Local,
                size: 12,
            }]
        );
    }

    #[test]
    fn a_resized_remote_file_is_copied_without_hashing() {
        let local = scan_of(&[("a.txt", 10, 100)]);
        let remote = scan_of(&[("a.txt", 14, 250)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let plan = plan_of(&local, &remote, &state, &no_hashes());
        assert_eq!(
            plan.actions,
            vec![Action::Copy {
                rel_path: "a.txt".into(),
                from: Side::Remote,
                size: 14,
            }]
        );
    }

    #[test]
    fn a_touched_but_unchanged_file_beside_a_resized_peer_still_copies() {
        // Both suspect, local resized: only the remote is read, and its
        // untouched content makes this a plain local edit. The provider has no
        // local hash, so reading the local file would fail the comparison.
        let local = scan_of(&[("a.txt", 12, 150)]);
        let remote = scan_of(&[("a.txt", 10, 999)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Remote, "a.txt", "h1")]);
        let plan = plan_of(&local, &remote, &state, &h);
        assert_eq!(plan.copy_count(Side::Remote), 1, "got: {:?}", plan.actions);
        assert!(plan.conflicts.is_empty());
    }

    #[test]
    fn a_resized_file_beside_an_edited_peer_of_another_size_is_a_conflict() {
        let local = scan_of(&[("a.txt", 12, 150)]);
        let remote = scan_of(&[("a.txt", 10, 999)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Remote, "a.txt", "hR")]);
        let plan = plan_of(&local, &remote, &state, &h);
        assert_eq!(plan.conflicts[0].kind, ConflictKind::BothEdited);
    }

    #[test]
    fn a_resized_file_beside_a_peer_of_the_same_size_is_hashed_to_converge() {
        // Both now 12 bytes: they may have been edited to the same content.
        let local = scan_of(&[("a.txt", 12, 150)]);
        let remote = scan_of(&[("a.txt", 12, 999)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 12, 200)]);
        let h = hashes(&[(Side::Local, "a.txt", "hX"), (Side::Remote, "a.txt", "hX")]);
        let plan = plan_of(&local, &remote, &state, &h);
        assert!(plan.conflicts.is_empty(), "got: {:?}", plan.conflicts);
        assert!(plan.is_noop(), "got: {:?}", plan.actions);
    }

    #[test]
    fn mtime_drift_without_content_change_records_state_and_copies_nothing() {
        // The remount case: mtime shifted on both sides, contents identical.
        let local = scan_of(&[("a.txt", 10, 900)]);
        let remote = scan_of(&[("a.txt", 10, 901)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Local, "a.txt", "h1"), (Side::Remote, "a.txt", "h1")]);
        let plan = plan_of(&local, &remote, &state, &h);
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
        let plan = plan_of(&local, &remote, &state, &h);
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
    fn both_edited_to_the_same_size_but_different_content_is_a_conflict() {
        let local = scan_of(&[("a.txt", 20, 900)]);
        let remote = scan_of(&[("a.txt", 20, 901)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Local, "a.txt", "hL"), (Side::Remote, "a.txt", "hR")]);
        let plan = plan_of(&local, &remote, &state, &h);
        assert!(plan.actions.is_empty());
        assert_eq!(plan.conflicts[0].kind, ConflictKind::BothEdited);
    }

    #[test]
    fn both_edited_to_different_sizes_is_a_conflict_without_hashing() {
        let local = scan_of(&[("a.txt", 20, 900)]);
        let remote = scan_of(&[("a.txt", 30, 901)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let plan = plan_of(&local, &remote, &state, &no_hashes());
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
        let plan = plan_of(&local, &remote, &BTreeMap::new(), &h);
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
        let remote = scan_of(&[("a.txt", 10, 200)]);
        let h = hashes(&[(Side::Local, "a.txt", "hL"), (Side::Remote, "a.txt", "hR")]);
        let plan = plan_of(&local, &remote, &BTreeMap::new(), &h);
        assert_eq!(plan.conflicts[0].kind, ConflictKind::BothNew);
    }

    #[test]
    fn new_on_both_sides_with_different_sizes_is_a_conflict_without_hashing() {
        let local = scan_of(&[("a.txt", 10, 100)]);
        let remote = scan_of(&[("a.txt", 11, 200)]);
        let plan = plan_of(&local, &remote, &BTreeMap::new(), &no_hashes());
        assert_eq!(plan.conflicts[0].kind, ConflictKind::BothNew);
    }

    #[test]
    fn new_on_both_sides_with_matching_size_and_mtime_is_adopted_from_the_local_hash() {
        // The provider has no remote hash, so reading the remote would fail.
        let local = scan_of(&[("a.txt", 10, 100)]);
        let remote = scan_of(&[("a.txt", 10, 100)]);
        let h = hashes(&[(Side::Local, "a.txt", "hL")]);
        let plan = plan_of(&local, &remote, &BTreeMap::new(), &h);
        assert!(plan.conflicts.is_empty(), "got: {:?}", plan.conflicts);
        assert_eq!(
            plan.actions,
            vec![Action::RecordState {
                rel_path: "a.txt".into(),
                hash: "hL".into(),
                local: Stat {
                    size: 10,
                    mtime: 100
                },
                remote: Stat {
                    size: 10,
                    mtime: 100
                },
            }]
        );
    }

    #[test]
    fn a_trusted_pair_counts_as_one_comparison() {
        let local = scan_of(&[("a.txt", 10, 100)]);
        let remote = scan_of(&[("a.txt", 10, 100)]);
        let h = hashes(&[(Side::Local, "a.txt", "hL")]);
        let events = events_of(&local, &remote, &BTreeMap::new(), &h);
        assert_eq!(compare_total(&events), Some(1));
    }

    #[test]
    fn verifying_reads_both_copies_despite_matching_size_and_mtime() {
        let local = scan_of(&[("a.txt", 10, 100)]);
        let remote = scan_of(&[("a.txt", 10, 100)]);
        let h = hashes(&[(Side::Local, "a.txt", "hL"), (Side::Remote, "a.txt", "hR")]);
        let plan = reconcile(
            &local,
            &remote,
            &BTreeMap::new(),
            &h,
            Compare::Verify,
            &Reporter::silent(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(plan.conflicts[0].kind, ConflictKind::BothNew);
    }

    #[test]
    fn verify_checks_tracked_files_even_when_metadata_is_unchanged() {
        let local = scan_of(&[("a.txt", 10, 100)]);
        let remote = scan_of(&[("a.txt", 10, 100)]);
        let state = state_of(&[("a.txt", "hL", 10, 100, 10, 100)]);
        for (lh, rh, expected) in [
            ("hL", "hR", Some(Side::Remote)),
            ("hR", "hL", Some(Side::Local)),
            ("hX", "hY", None),
        ] {
            let h = hashes(&[(Side::Local, "a.txt", lh), (Side::Remote, "a.txt", rh)]);
            let plan = reconcile(
                &local,
                &remote,
                &state,
                &h,
                Compare::Verify,
                &Reporter::silent(),
            )
            .unwrap()
            .unwrap();
            if let Some(from) = expected {
                assert_eq!(
                    plan.actions,
                    vec![Action::Copy {
                        rel_path: "a.txt".into(),
                        from,
                        size: 10,
                    }]
                );
            } else {
                assert_eq!(plan.conflicts[0].kind, ConflictKind::BothEdited);
            }
        }
    }

    #[test]
    fn verify_records_a_new_shared_hash_with_unchanged_metadata() {
        let scan = scan_of(&[("a.txt", 10, 100)]);
        let state = state_of(&[("a.txt", "old", 10, 100, 10, 100)]);
        let h = hashes(&[
            (Side::Local, "a.txt", "new"),
            (Side::Remote, "a.txt", "new"),
        ]);
        let plan = reconcile(
            &scan,
            &scan,
            &state,
            &h,
            Compare::Verify,
            &Reporter::silent(),
        )
        .unwrap()
        .unwrap();
        assert!(matches!(&plan.actions[0], Action::RecordState { hash, .. } if hash == "new"));
    }

    #[test]
    fn verify_prevents_deleting_a_survivor_with_an_unrecorded_edit() {
        let scan = scan_of(&[("a.txt", 10, 100)]);
        let state = state_of(&[("a.txt", "old", 10, 100, 10, 100)]);
        for side in [Side::Local, Side::Remote] {
            let empty = Scan::default();
            let (local, remote) = match side {
                Side::Local => (&scan, &empty),
                Side::Remote => (&empty, &scan),
            };
            let h = hashes(&[(side, "a.txt", "edited")]);
            let plan = reconcile(
                local,
                remote,
                &state,
                &h,
                Compare::Verify,
                &Reporter::silent(),
            )
            .unwrap()
            .unwrap();
            assert!(plan.actions.is_empty());
            assert_eq!(plan.conflicts.len(), 1);
        }
    }

    #[test]
    fn matching_mtimes_are_not_trusted_once_state_is_recorded() {
        // Both sides edited to the same new mtime: the recorded state, not the
        // pair's agreement, decides, so both are still read.
        let local = scan_of(&[("a.txt", 10, 500)]);
        let remote = scan_of(&[("a.txt", 10, 500)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Local, "a.txt", "hL"), (Side::Remote, "a.txt", "hR")]);
        let plan = plan_of(&local, &remote, &state, &h);
        assert_eq!(plan.conflicts[0].kind, ConflictKind::BothEdited);
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
        // The child `thing/inner.txt` is a genuine new-remote file (local has no
        // such path), so the completed decision table schedules it for copy.
        let plan = plan_of(&local, &remote, &BTreeMap::new(), &no_hashes());
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
        let plan = plan_of(&local, &Scan::default(), &BTreeMap::new(), &no_hashes());
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
        let plan = plan_of(&local, &remote, &BTreeMap::new(), &no_hashes());
        assert_eq!(plan.skips.len(), 2, "got: {:?}", plan.skips);
    }

    #[test]
    fn remote_deleted_and_local_unchanged_trashes_local() {
        let local = scan_of(&[("a.txt", 10, 100)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let plan = plan_of(&local, &Scan::default(), &state, &no_hashes());
        assert_eq!(
            plan.actions,
            vec![Action::Trash {
                rel_path: "a.txt".into(),
                side: Side::Local
            }]
        );
    }

    #[test]
    fn local_deleted_and_remote_unchanged_trashes_remote() {
        let remote = scan_of(&[("a.txt", 10, 200)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let plan = plan_of(&Scan::default(), &remote, &state, &no_hashes());
        assert_eq!(
            plan.actions,
            vec![Action::Trash {
                rel_path: "a.txt".into(),
                side: Side::Remote
            }]
        );
    }

    #[test]
    fn remote_deleted_but_local_edited_in_place_is_a_conflict() {
        let local = scan_of(&[("a.txt", 10, 900)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Local, "a.txt", "hNEW")]);
        let plan = plan_of(&local, &Scan::default(), &state, &h);
        assert_eq!(
            plan.conflicts[0].kind,
            ConflictKind::EditedLocalDeletedRemote
        );
    }

    #[test]
    fn remote_deleted_but_local_resized_is_a_conflict_without_hashing() {
        let local = scan_of(&[("a.txt", 99, 900)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let plan = plan_of(&local, &Scan::default(), &state, &no_hashes());
        assert!(plan.actions.is_empty(), "got: {:?}", plan.actions);
        assert_eq!(
            plan.conflicts,
            vec![Conflict {
                rel_path: "a.txt".into(),
                kind: ConflictKind::EditedLocalDeletedRemote,
                local: Some(Stat {
                    size: 99,
                    mtime: 900
                }),
                remote: None,
            }]
        );
    }

    #[test]
    fn local_deleted_but_remote_edited_is_a_conflict() {
        let remote = scan_of(&[("a.txt", 10, 900)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Remote, "a.txt", "hNEW")]);
        let plan = plan_of(&Scan::default(), &remote, &state, &h);
        assert_eq!(
            plan.conflicts[0].kind,
            ConflictKind::EditedRemoteDeletedLocal
        );
        assert_eq!(plan.conflicts[0].local, None);
    }

    #[test]
    fn a_suspect_but_unchanged_file_whose_peer_vanished_is_still_trashed() {
        // mtime drifted locally, but content matches state: the remote deletion
        // is the only real change, so propagate it rather than calling it a conflict.
        let local = scan_of(&[("a.txt", 10, 777)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Local, "a.txt", "h1")]);
        let plan = plan_of(&local, &Scan::default(), &state, &h);
        assert_eq!(
            plan.actions,
            vec![Action::Trash {
                rel_path: "a.txt".into(),
                side: Side::Local
            }]
        );
    }

    #[test]
    fn a_new_local_file_copies_to_remote_without_hashing() {
        let local = scan_of(&[("new.txt", 42, 100)]);
        let plan = plan_of(&local, &Scan::default(), &BTreeMap::new(), &no_hashes());
        assert_eq!(
            plan.actions,
            vec![Action::Copy {
                rel_path: "new.txt".into(),
                from: Side::Local,
                size: 42,
            }]
        );
    }

    #[test]
    fn a_new_remote_file_copies_to_local_without_hashing() {
        let remote = scan_of(&[("new.txt", 42, 100)]);
        let plan = plan_of(&Scan::default(), &remote, &BTreeMap::new(), &no_hashes());
        assert_eq!(
            plan.actions,
            vec![Action::Copy {
                rel_path: "new.txt".into(),
                from: Side::Remote,
                size: 42,
            }]
        );
    }

    #[test]
    fn gone_from_both_sides_drops_the_state_row() {
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let plan = plan_of(&Scan::default(), &Scan::default(), &state, &no_hashes());
        assert_eq!(
            plan.actions,
            vec![Action::DropState {
                rel_path: "a.txt".into()
            }]
        );
        assert!(plan.is_noop(), "dropping a row touches no files");
    }

    #[test]
    fn an_emptied_remote_schedules_deletion_of_every_tracked_path() {
        // reconcile itself does not refuse this; Task 11's threshold check does.
        let local = scan_of(&[("a.txt", 1, 1), ("b.txt", 1, 1), ("c.txt", 1, 1)]);
        let state = state_of(&[
            ("a.txt", "h", 1, 1, 1, 1),
            ("b.txt", "h", 1, 1, 1, 1),
            ("c.txt", "h", 1, 1, 1, 1),
        ]);
        let plan = plan_of(&local, &Scan::default(), &state, &no_hashes());
        assert_eq!(plan.trash_count(), 3);
    }

    #[test]
    fn a_mixed_tree_produces_every_action_kind_at_once() {
        let local = scan_of(&[
            ("same.txt", 10, 100),      // unchanged
            ("edited.txt", 20, 900),    // edited locally
            ("newlocal.txt", 5, 50),    // new local
            ("delremote.txt", 10, 100), // remote deleted this
        ]);
        let remote = scan_of(&[
            ("same.txt", 10, 200),
            ("edited.txt", 10, 200),
            ("newremote.txt", 7, 70),
            ("dellocal.txt", 10, 200),
        ]);
        let state = state_of(&[
            ("same.txt", "h1", 10, 100, 10, 200),
            ("edited.txt", "h2", 10, 100, 10, 200),
            ("delremote.txt", "h3", 10, 100, 10, 200),
            ("dellocal.txt", "h4", 10, 100, 10, 200),
        ]);
        let plan = plan_of(&local, &remote, &state, &no_hashes());

        assert_eq!(
            plan.copy_count(Side::Remote),
            2,
            "edited.txt + newlocal.txt"
        );
        assert_eq!(plan.copy_count(Side::Local), 1, "newremote.txt");
        assert_eq!(plan.trash_count(), 2, "delremote.txt + dellocal.txt");
        assert!(plan.conflicts.is_empty());
    }

    #[test]
    fn the_compare_total_counts_exactly_the_suspect_paths() {
        let local = scan_of(&[
            ("same.txt", 10, 100),    // unchanged on both sides
            ("edited.txt", 10, 900),  // touched locally
            ("theirs.txt", 10, 100),  // touched remotely
            ("resized.txt", 20, 900), // resized locally, so no hash is needed
        ]);
        let remote = scan_of(&[
            ("same.txt", 10, 200),
            ("edited.txt", 10, 200),
            ("theirs.txt", 10, 800),
            ("resized.txt", 10, 200),
        ]);
        let state = state_of(&[
            ("same.txt", "h1", 10, 100, 10, 200),
            ("edited.txt", "h2", 10, 100, 10, 200),
            ("theirs.txt", "h3", 10, 100, 10, 200),
            ("resized.txt", "h4", 10, 100, 10, 200),
        ]);
        let h = hashes(&[
            (Side::Local, "edited.txt", "hE"),
            (Side::Remote, "theirs.txt", "hT"),
        ]);
        let events = events_of(&local, &remote, &state, &h);

        assert_eq!(
            compare_total(&events),
            Some(2),
            "same.txt and resized.txt need no hash"
        );
        assert_eq!(
            compared_paths(&events),
            vec!["edited.txt".to_string(), "theirs.txt".to_string()]
        );
    }

    #[test]
    fn a_path_suspect_on_both_sides_is_counted_once() {
        let local = scan_of(&[("a.txt", 10, 900)]);
        let remote = scan_of(&[("a.txt", 10, 901)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Local, "a.txt", "hL"), (Side::Remote, "a.txt", "hR")]);
        let events = events_of(&local, &remote, &state, &h);

        assert_eq!(compare_total(&events), Some(1));
        assert_eq!(compared_paths(&events), vec!["a.txt".to_string()]);
    }

    #[test]
    fn an_unchanged_tree_compares_nothing() {
        let local = scan_of(&[("a.txt", 10, 100), ("b.txt", 10, 100)]);
        let remote = scan_of(&[("a.txt", 10, 200), ("b.txt", 10, 200)]);
        let state = state_of(&[
            ("a.txt", "h1", 10, 100, 10, 200),
            ("b.txt", "h2", 10, 100, 10, 200),
        ]);
        let events = events_of(&local, &remote, &state, &no_hashes());

        assert_eq!(compare_total(&events), Some(0));
        assert!(compared_paths(&events).is_empty(), "got: {events:?}");
        assert_eq!(
            events.last(),
            Some(&crate::progress::Progress::Finished {
                phase: Phase::Compare,
                count: 0
            })
        );
    }

    #[test]
    fn the_finished_count_matches_the_total() {
        let local = scan_of(&[("both.txt", 5, 100), ("edited.txt", 10, 900)]);
        let remote = scan_of(&[("both.txt", 5, 100), ("edited.txt", 10, 200)]);
        let state = state_of(&[("edited.txt", "h2", 10, 100, 10, 200)]);
        let h = hashes(&[
            (Side::Local, "both.txt", "hB"),
            (Side::Remote, "both.txt", "hB"),
            (Side::Local, "edited.txt", "hE"),
        ]);
        let events = events_of(&local, &remote, &state, &h);

        let total = compare_total(&events).unwrap();
        assert_eq!(
            events.last(),
            Some(&crate::progress::Progress::Finished {
                phase: Phase::Compare,
                count: total
            }),
            "a total the run cannot reach would leave the bar short"
        );
    }

    #[test]
    fn a_type_mismatch_needs_no_hash_and_is_not_counted() {
        let local = scan_of(&[("thing", 10, 100)]);
        let mut remote = Scan::default();
        remote.dirs.insert("thing".to_string());
        let events = events_of(&local, &remote, &BTreeMap::new(), &no_hashes());
        assert_eq!(compare_total(&events), Some(0));
    }

    #[test]
    fn cancelling_yields_no_plan() {
        let local = scan_of(&[("a.txt", 1, 1), ("b.txt", 1, 1)]);
        let sink = |_| {};
        let plan = reconcile(
            &local,
            &Scan::default(),
            &BTreeMap::new(),
            &no_hashes(),
            Compare::Quick,
            &Reporter::new(&sink, &|| true),
        )
        .unwrap();
        assert!(plan.is_none());
    }

    #[test]
    fn many_paths_hash_in_parallel_into_the_same_plan() {
        // Enough work for every worker; each path's hashes must land on that
        // path and no other.
        let n = 200;
        let names: Vec<String> = (0..n).map(|i| format!("f{i:03}.txt")).collect();
        let local = scan_of(
            &names
                .iter()
                .map(|p| (p.as_str(), 10, 100))
                .collect::<Vec<_>>(),
        );
        let remote = scan_of(
            &names
                .iter()
                .map(|p| (p.as_str(), 10, 200))
                .collect::<Vec<_>>(),
        );
        // Every third path differs, so a crossed wire changes the plan.
        let table = || {
            let mut table = BTreeMap::new();
            for (i, p) in names.iter().enumerate() {
                let r = if i % 3 == 0 {
                    format!("x{i}")
                } else {
                    format!("h{i}")
                };
                table.insert((Side::Local, p.clone()), format!("h{i}"));
                table.insert((Side::Remote, p.clone()), r);
            }
            MapHashProvider(table)
        };
        let events = events_of(&local, &remote, &BTreeMap::new(), &table());
        assert_eq!(compare_total(&events), Some(n));
        assert_eq!(compared_paths(&events), names);

        let indices: Vec<usize> = events
            .iter()
            .filter_map(|e| match e {
                crate::progress::Progress::Item { index, .. } => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(indices, (0..n).collect::<Vec<_>>(), "indices count up");

        let plan = plan_of(&local, &remote, &BTreeMap::new(), &table());
        let conflicted: Vec<_> = plan.conflicts.iter().map(|c| c.rel_path.clone()).collect();
        let expected: Vec<_> = names.iter().step_by(3).cloned().collect();
        assert_eq!(conflicted, expected);
        assert_eq!(plan.actions.len(), n - expected.len());
    }

    #[test]
    fn a_hash_failure_fails_the_comparison() {
        let local = scan_of(&[("a.txt", 10, 100), ("b.txt", 10, 100)]);
        let remote = scan_of(&[("a.txt", 10, 200), ("b.txt", 10, 200)]);
        // b.txt has no hashes, so the provider errors on it.
        let h = hashes(&[(Side::Local, "a.txt", "h"), (Side::Remote, "a.txt", "h")]);
        let result = reconcile(
            &local,
            &remote,
            &BTreeMap::new(),
            &h,
            Compare::Quick,
            &Reporter::silent(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn cancelling_mid_hash_yields_no_plan() {
        let names: Vec<String> = (0..50).map(|i| format!("f{i}.txt")).collect();
        let items: Vec<_> = names.iter().map(|p| (p.as_str(), 10, 100)).collect();
        let local = scan_of(&items);
        let remote = scan_of(&items);
        let mut table = BTreeMap::new();
        for p in &names {
            table.insert((Side::Local, p.clone()), "h".to_string());
            table.insert((Side::Remote, p.clone()), "h".to_string());
        }
        let checks = std::cell::Cell::new(0usize);
        let sink = |_| {};
        let plan = reconcile(
            &local,
            &remote,
            &BTreeMap::new(),
            &MapHashProvider(table),
            Compare::Verify,
            &Reporter::new(&sink, &|| {
                checks.set(checks.get() + 1);
                checks.get() > 3
            }),
        )
        .unwrap();
        assert!(plan.is_none());
    }

    #[test]
    fn fs_hash_provider_reads_from_the_right_root() {
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        std::fs::write(local.path().join("a.txt"), "local contents").unwrap();
        std::fs::write(remote.path().join("a.txt"), "remote contents").unwrap();
        let p = FsHashProvider {
            local_root: local.path().to_path_buf(),
            remote_root: remote.path().to_path_buf(),
        };
        assert_ne!(
            p.hash(Side::Local, "a.txt").unwrap(),
            p.hash(Side::Remote, "a.txt").unwrap()
        );
    }
}
