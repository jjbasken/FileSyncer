# FileSyncer Design

Date: 2026-07-26
Status: Approved

## Purpose

FileSyncer keeps a local directory and a mounted network or cloud share in sync,
in both directions. It is a real tool for a recurring workflow, not a general
rsync replacement.

Because the remote side is a mount, everything is ordinary filesystem paths.
There is no network protocol to implement.

### Behavior summary

- Bidirectional. Either side may be edited.
- Conflicts are reported and skipped, never resolved silently.
- Deletions propagate, but deleted files move to a trash directory first.
- Every run scans and presents a plan for confirmation before writing anything.
- Multiple named profiles, each a local path paired with a remote path.
- Symlinks are skipped and reported.
- Scale target: thousands of files, a few GB. Content hashing is affordable.

## Architecture

A Cargo workspace of three crates.

### `filesyncer-core`

The engine. Contains no GTK types and writes nothing to stdout.

| Module | Responsibility |
|---|---|
| `profile` | Load, validate, and save profile definitions |
| `scan` | Walk one root, applying ignore rules, producing entries |
| `state` | SQLite store of the last-sync snapshot |
| `reconcile` | Pure function: two scans plus state produce a `Plan` |
| `apply` | Execute a `Plan`; the only module that mutates the filesystem |
| `hash` | BLAKE3 of file contents |

`reconcile` performs no I/O. All inputs are passed in and a `Plan` comes out,
which makes the logic that decides what to delete exhaustively testable without
touching a disk.

### `filesyncer-cli`

A `clap` front end.

| Command | Behavior |
|---|---|
| `sync <profile>` | Scan, print the plan, prompt, apply |
| `sync <profile> --yes` | Skip the prompt |
| `sync <profile> --dry-run` | Print the plan and stop |
| `sync <profile> --force` | Override the deletion threshold |
| `list` | List configured profiles |
| `add` | Create a profile and write its root markers |
| `status <profile>` | Show configured paths, last sync time, tracked path count |

`status` reads only the state database and does not scan, so it is instant and
never touches the remote mount. It cannot report a conflict count, because
conflicts are a property of a scan rather than stored state; use `--dry-run` for
that.

Exit codes: `0` clean, `1` error, `2` conflicts present. `--dry-run` uses the
same codes, so `2` means "a scan found conflicts" whether or not anything was
applied. A future systemd timer can alert on `2` without parsing output.

### `filesyncer-gui`

GTK4 with libadwaita. Contains no sync logic; it calls the same core API the CLI
does.

### Crossing the boundary

`apply` accepts a progress callback (`&dyn Fn(Progress)`). The CLI renders a
progress bar from it. The GUI runs `apply` on a worker thread and forwards each
event through an `async_channel` into the GTK main context, the standard
gtk4-rs pattern. Core knows nothing about threads or widgets.

### Dependencies

`ignore`, `rusqlite` (bundled), `blake3`, `serde`, `toml`, `clap`, `gtk4`,
`libadwaita`, `async-channel`, `anyhow`, `thiserror`, `tempfile` (dev).

## Reconciliation

### State

One row per relative path, recording what was true at the last successful sync:

- `rel_path`
- `hash` — content hash at last sync; both sides were identical then, so one
  hash suffices
- `local_size`, `local_mtime`
- `remote_size`, `remote_mtime`

### Two-stage change detection

A side whose current size or mtime differs from the stored values is *suspect*,
not changed. FileSyncer then hashes the file to decide.

This matters specifically for mounted shares. SMB and NFS routinely report
shifted or coarsened mtimes after a remount or clock skew. A naive mtime
comparison would schedule a full re-copy of the entire tree. When the hash
matches the stored hash, FileSyncer refreshes the stored stat values and
transfers nothing.

### Decision table

For every path in the union of the local scan, the remote scan, and the state
table. `L` and `R` mean present on that side; `S` means a state row exists.

| L | R | S | Situation | Action |
|---|---|---|---|---|
| ✓ | ✓ | ✓ | neither side changed | none |
| ✓ | ✓ | ✓ | local changed only | copy to remote |
| ✓ | ✓ | ✓ | remote changed only | copy to local |
| ✓ | ✓ | ✓ | both changed, hashes equal | update state only |
| ✓ | ✓ | ✓ | both changed, hashes differ | conflict |
| ✓ | ✓ | — | new on both sides, hashes equal | adopt into state |
| ✓ | ✓ | — | new on both sides, hashes differ | conflict |
| ✓ | — | ✓ | local unchanged, remote deleted | trash local, drop row |
| ✓ | — | ✓ | local changed, remote deleted | conflict |
| — | ✓ | ✓ | remote unchanged, local deleted | trash remote, drop row |
| — | ✓ | ✓ | remote changed, local deleted | conflict |
| ✓ | — | — | new local file | copy to remote |
| — | ✓ | — | new remote file | copy to local |
| — | — | ✓ | gone from both sides | drop row |

A path that is a file on one side and a directory on the other is always a
conflict.

Directories are not tracked as state rows. They are created on demand when
copying, and pruned when left empty after deletions.

Symlinks are never synced. Each appears in the run's skipped list so its
existence is visible.

### Safeguards

An unmounted mount point is an empty directory. An empty remote scan against a
populated state table reads as "the user deleted everything," and the resulting
plan would trash the entire local tree. Two independent defenses:

1. **Root markers.** Each root contains a `.filesyncer-root` file holding the
   profile `id`. If it is missing or holds the wrong `id` at scan time, the run
   aborts before any plan is built. An unmounted mount point cannot have one.
2. **Deletion threshold.** A plan that would delete more than 25% of tracked
   paths aborts with an explanation. `--force` (CLI) or an explicit confirmation
   (GUI) overrides it for legitimate mass deletions.

Markers are written when a profile is created. If one is later lost, the abort
message names the missing path and states how to restore it; recreating the file
by hand with the profile `id` as its contents is a supported repair. FileSyncer
never recreates a marker automatically, since doing so would defeat the check.

### Conflicts

Conflicts are reported and skipped. Their state rows are left untouched, so the
same conflict resurfaces on every subsequent run until it is resolved.

The GUI offers a per-conflict choice of keep-local or keep-remote. Choosing a
side rewrites that conflict into an ordinary copy within the plan before `apply`
runs, so the engine needs no additional capability. The CLI does not resolve
conflicts; it reports them and exits `2`.

### Trash

Deleted files move to `.filesyncer-trash/<run-timestamp>/<original/rel/path>` on
the side being deleted from, preserving the original directory structure. The
trash directory is always excluded from scans. FileSyncer never prunes it;
clearing it is a manual action.

## Configuration and on-disk layout

### Profiles

`$XDG_CONFIG_HOME/filesyncer/profiles.toml`:

```toml
[[profile]]
id = "documents"
name = "Documents ↔ NAS"
local = "/home/jeremy/Documents"
remote = "/mnt/nas/documents"
```

`id` is stable and machine-facing: it appears in the root markers and names the
state database. `name` is the editable display label. Keeping them distinct means
renaming a profile in the GUI does not orphan its state.

### State

One SQLite database per profile at
`$XDG_DATA_HOME/filesyncer/state/<id>.db`.

State lives apart from configuration so the state directory can be deleted to
force a clean re-baseline without losing profile definitions. Deleting a state
database is a supported recovery action. The next run treats every path as
new-on-both-sides: identical files are adopted silently, and genuine differences
are flagged as conflicts. This degrades correctly. It cannot lose data; it only
asks more questions.

### Ignore rules

A `.syncignore` file at each sync root, with gitignore semantics supplied by the
`ignore` crate via `add_custom_ignore_filename(".syncignore")`. Rules from both
roots are unioned, so a pattern added on either side takes effect on both.

`.filesyncer-trash/` and `.filesyncer-root` are always excluded regardless of
ignore file contents.

### Profile validation

On load, FileSyncer verifies that both roots exist, are directories, are not the
same path, and that neither contains the other. Nesting silently produces
infinite recursion, so it fails loudly.

## GUI

An `AdwNavigationSplitView`: profiles in the sidebar, the selected profile's
workspace in the content pane.

```
┌────────────────┬──────────────────────────────────────┐
│ Profiles     ＋ │  Documents ↔ NAS                     │
│ ───────────────│  /home/jeremy/Documents               │
│ ▸ Documents ↔ …│  /mnt/nas/documents                   │
│ ▸ Photos ↔ NAS │  Last synced: today 09:14             │
│ ▸ Projects ↔ … │                                       │
│                │            [ Scan ]                   │
└────────────────┴──────────────────────────────────────┘
```

Scanning replaces the content pane with the plan, rendered as collapsible
`AdwExpanderRow` groups over the same `Plan` struct the CLI prints:

```
  ▸ Copy to remote           142 files   1.2 GB
  ▸ Copy to local             18 files    340 MB
  ▸ Move to trash              3 files
  ▾ Conflicts                  2 files
      notes.md      both edited        [Keep local] [Keep remote]
      budget.ods    edited vs deleted  [Keep local] [Keep remote]
  ▸ Skipped (symlinks, unreadable)  4
                        [ Cancel ]  [ Apply ]
```

Conflict rows are the only interactive ones. Every other row is informational.
The skipped group covers what the scan could not handle: symlinks and paths that
could not be read. Errors that occur during `apply` are distinct and appear in
the run summary afterward.

Applying shows a progress bar, the current file, and a working Cancel. Cancel is
safe: `apply` commits state per file, so stopping midway leaves a consistent
database and the next scan resumes from exactly where it stopped. The run ends on
a summary showing counts, elapsed time, and any per-file errors listed
individually rather than collapsed into a single failure message.

Adding a profile opens a dialog with two `GtkFileDialog` folder pickers and a
name field. Saving writes the root markers into both roots, and is where nesting
and existence validation surface to the user.

There is no background activity, no tray icon, and no filesystem watching. The
window acts only when a button is pressed.

## Error handling

### Per-file, not per-run

An unreadable file, a permission error, or an out-of-space condition on a single
copy records an error against that path and the run continues. The failed path's
state row is left untouched so the next run retries it. Every error appears in
the run summary.

### Aborting conditions

These abort the entire run, because continuing would be unsafe:

- A root marker is missing or holds the wrong profile `id`.
- The deletion threshold is exceeded without an override.
- A root becomes inaccessible mid-run, which usually means the mount dropped and
  every subsequent operation would act on wrong information.

### Atomic copies

Each copy writes to `<name>.filesyncer-tmp-<pid>` in the destination directory,
fsyncs, renames into place, then sets the mtime. A crash or a dropped mount
leaves a temp file, never a partial file that appears complete. Stale temp files
are removed at the start of the next run.

## Testing

Effort concentrates where a bug would cost files.

- **`reconcile` unit tests.** Every row of the decision table, driven by
  hand-built structs. No filesystem, no fixtures, fast. This is the bulk of the
  suite.
- **`apply` integration tests** against `tempfile` directories: trash placement,
  atomic rename behavior, per-file error isolation, and that an interrupted
  apply leaves resumable state.
- **Round-trip tests** driving scan → reconcile → apply across two temp
  directories, asserting convergence and idempotence. A second run immediately
  following a first must produce an empty plan. That property alone catches a
  broad class of change-detection bugs, including mtime drift.
- **Safeguard tests.** An emptied remote root must abort rather than delete. A
  wrong-id root marker must abort. A plan crossing the deletion threshold must
  abort without `--force`.
- `ignore` and `rusqlite` are trusted. Tests cover FileSyncer's usage of them,
  not their internals.
- The GUI has no automated tests. Its logic is thin by construction, and headless
  GTK testing would cost more than it would catch here.

## Out of scope

Deliberately excluded from this design:

- Background or scheduled syncing. The CLI's exit codes make a systemd timer
  straightforward to add later without redesign.
- Filesystem watching. Network mounts generally do not support inotify.
- Syncing over a network protocol. The remote side is always a mounted path.
- Automatic conflict merging of file contents.
- Automatic trash pruning or retention policies.
- Preserving ownership, ACLs, or extended attributes. Mode bits follow whatever
  the destination filesystem supports.
