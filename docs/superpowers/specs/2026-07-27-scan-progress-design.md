# Scan Progress Design

Date: 2026-07-27
Status: Approved

## Purpose

The apply phase reports progress; the scan phase does not. Pressing **Scan** in
the GUI changes a button label to "Scanning…" and nothing else happens until the
plan appears. `filesyncer sync` prints nothing until the plan summary. On a large
tree or a slow share, both look indistinguishable from a hang, and neither offers
a way out short of killing the process.

This adds progress reporting to scanning, and makes a scan cancellable.

### Behavior summary

- Both frontends show which phase is running, how many files it has seen, and
  the path it is on.
- Walking a tree is indeterminate — the file count is not known until the walk
  finishes — so it reports a count-up, not a percentage.
- Hashing is determinate. The number of paths needing a hash is computed before
  any hashing starts, so it reports `n / total`.
- A scan can be cancelled. Cancelling is not an error: a scan writes nothing.

## Progress vocabulary

`Progress` is currently apply-specific (`Started { total } | Item { .. } |
Finished`). It becomes a shared vocabulary for all three phases, in a new
`filesyncer-core/src/progress.rs`:

```rust
pub enum Phase { ScanLocal, ScanRemote, Compare, Apply }

pub enum Progress {
    Started  { phase: Phase, total: Option<usize> },
    Item     { phase: Phase, index: usize, rel_path: String },
    Finished { phase: Phase, count: usize },
}
```

`total: None` means the size of the phase is not known ahead of time. It is the
signal a frontend keys on to show an indeterminate indicator; every walk sets it,
and no other phase does.

A `Reporter` bundles the progress sink with the cancellation check, following the
existing `ApplyCtx` idiom:

```rust
pub struct Reporter<'a> {
    progress: &'a dyn Fn(Progress),
    cancel:   &'a dyn Fn() -> bool,
}

impl<'a> Reporter<'a> {
    pub fn new(progress: &'a dyn Fn(Progress), cancel: &'a dyn Fn() -> bool) -> Self;
    pub fn silent() -> Reporter<'static>;   // no-op sink, never cancels
    pub fn cancelled(&self) -> bool;
}
```

`Reporter::silent()` keeps existing test call sites to a one-word change, and
collapses `apply`'s two trailing closure parameters into a single argument.

## Where events come from

### `scan_root`

```rust
pub fn scan_root(
    root: &Path,
    extra_ignore_files: &[PathBuf],
    phase: Phase,
    reporter: &Reporter,
) -> Result<Option<Scan>>
```

Emits `Started { total: None }`, one `Item` per accepted entry, and
`Finished { count }` with the number of files found. The phase is a parameter
rather than something `scan_root` decides: only the caller knows whether a given
root is the local or the remote side. `Session::prepare` passes `ScanLocal` and
then `ScanRemote`.

Cancellation is checked once per entry. `Ok(None)` means cancelled.

### `reconcile`

```rust
pub fn reconcile(
    local: &Scan, remote: &Scan,
    state: &BTreeMap<String, StateRow>,
    hashes: &dyn HashProvider,
    reporter: &Reporter,
) -> Result<Option<Plan>>
```

Before the main loop, a pre-pass over the union of the key set counts the paths
whose recorded size or mtime differs on either side. That comparison is pure —
it reads the scans and the state map, never the filesystem — and yields the
`Compare` total. A path is counted once even when both sides need hashing.

The main loop then emits one `Item` per compared path, indexed against that
total, and checks cancellation per path. Paths that need no hash emit nothing:
an unchanged tree produces `Started { total: Some(0) }` immediately followed by
`Finished { count: 0 }`.

### `apply`

```rust
pub fn apply(plan: &Plan, ctx: &ApplyCtx, reporter: &Reporter) -> Result<RunSummary>
```

Same events under `Phase::Apply`. Behavior is unchanged; only the signature and
the enum shape move.

### `Session`

```rust
pub fn prepare(&self, force: bool, reporter: &Reporter) -> Result<Option<Plan>>
pub fn run(&self, plan: &Plan, reporter: &Reporter) -> Result<RunSummary>
```

`prepare` returns `Ok(None)` when the scan was cancelled. Cancellation is
modelled as an absent plan rather than an `Error` variant because nothing failed
and nothing was written — an error would force every caller to distinguish a
cancelled scan from a broken one.

The deletion-threshold check still runs after reconciliation, unchanged.

## GUI

A new `crates/filesyncer-gui/src/scan_view.rs`, mirroring `run_view.rs`:

- A heading naming the phase ("Scanning local files…", "Scanning remote files…",
  "Comparing…").
- A `gtk::ProgressBar` that pulses while `total` is `None` and shows a fraction
  once a total is known.
- A count label ("1,284 files") and an ellipsized path label.
- A **Cancel** button, driving the same `AtomicBool` pattern `run_view` uses.

The worker thread structure is unchanged from today's `profile_page`: it opens
its own `Session` (rusqlite connections are `Send` but not `Sync`) and reports
through an `async_channel`. Its progress closure throttles `Item` events to at
most one per 100 ms, measured against an `Instant`, so a fast local walk cannot
flood the channel. `Started` and `Finished` are never throttled.

On completion the view hands off to `plan_view::show`. A cancelled scan returns
to the profile page. An error uses the existing dialog.

`profile_page` loses its inline "Scanning…" button label — the scan page is now
the feedback — but keeps its worker-spawn responsibility.

## CLI

`render.rs` gains a scan reporter written against an injected writer and an
explicit TTY flag, so both output shapes are unit-testable:

```rust
pub struct ScanReporter<W: Write> { out: W, tty: bool, /* … */ }
```

`main` constructs it with `std::io::stdout()` and `std::io::stdout().is_terminal()`
(`std::io::IsTerminal`; no new dependency).

**Interactive (TTY).** A status line rewritten in place with `\r`, at most ten
updates per second, cleared when the phase finishes:

```
Scanning remote… 1,284 files  photos/2024/img_09.jpg
Comparing… 12/40  notes/todo.md
```

**Non-interactive.** One plain line per phase, printed when that phase finishes:

```
Scanning local… 1,284 files
Scanning remote… 1,290 files
Comparing… 40 files hashed
```

This adds three lines to output that scheduled jobs do not see today. The plan
summary format and all exit codes are untouched, so a job parsing the summary or
branching on exit status is unaffected.

The CLI passes a cancel closure that always returns false; Ctrl-C remains the way
to abort a command line scan, which is safe because a scan writes nothing.

## Testing

Core:

- `scan_root` emits `Started`/`Finished` with a count matching `scan.files.len()`,
  and one `Item` per file.
- Cancelling mid-walk returns `Ok(None)`, and `prepare` propagates that as `None`.
- A cancelled scan leaves both roots and the state database untouched.
- The `Compare` total equals the number of suspect paths for a fixture mixing
  unchanged, locally modified, and remotely modified files.
- An unchanged tree emits zero `Compare` items.
- A path suspect on both sides is counted once.

CLI: `ScanReporter` is driven over a `Vec<u8>` with `tty` true and false, and the
two output shapes are asserted directly, including that the non-TTY path emits
nothing until a phase finishes.

GUI: manual verification, as with the existing run view.

## Out of scope

- Byte-level progress within a single large file.
- Estimated time remaining.
- A `--quiet` flag. If the three non-TTY lines prove unwelcome in logs, that is a
  follow-up.
