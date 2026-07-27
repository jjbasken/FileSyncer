# FileSyncer

Bidirectional file synchroniser between a local directory and a mounted network
share. Changes are reviewed before anything is written, conflicts are flagged
rather than auto-merged, and deletions are routed through a recoverable trash
directory. It ships as a GTK4 desktop app (`filesyncer-gui`) and a matching
command line tool (`filesyncer`), both driving the same engine.

## How it works

Each run follows a **scan → review → apply** cycle:

1. **Scan** both roots and compare against the last-known state. Change
   detection is two-stage — a file is only hashed (BLAKE3) when its size or
   mtime differs from the recorded state, so an unchanged tree costs no reads.
2. **Review** the resulting plan: what will be copied in each direction, what
   will be trashed, and which paths are in conflict. Nothing has happened yet.
3. **Apply** the plan. Copies are atomic (temp file + rename), each file's state
   is committed as it completes, and per-file errors are reported without
   aborting the run — so an interrupted sync resumes exactly where it stopped.

Highlights:

- **Bidirectional.** New and edited files propagate whichever way they changed.
- **Conflicts are flagged, never merged.** If a file changed on both sides (or
  was edited on one and deleted on the other), it is reported and left untouched.
  Conflicts resurface on every run until you resolve them by keeping the local
  or the remote copy.
- **Deletions are recoverable.** A deleted file is moved to
  `.filesyncer-trash/<timestamp>/` on the far side rather than removed.
- **Safe against unmounted shares.** Each root carries a `.filesyncer-root`
  marker; if it is missing (as an unmounted mount point would be) the run aborts
  instead of reading the empty directory as a mass deletion. A separate guard
  refuses any plan that would delete more than 25% of tracked paths unless you
  pass `--force`.
- **Symlinks** are skipped and reported, never followed or copied.

## Building

Requires a Rust toolchain (edition 2024, ≥ 1.96) and, for the GUI, the GTK 4 and
libadwaita development libraries:

- Debian/Ubuntu: `sudo apt install libgtk-4-dev libadwaita-1-dev`
- Fedora: `sudo dnf install gtk4-devel libadwaita-devel`

```sh
cargo build --release --workspace
cargo test --workspace
```

## Installing

### System / per-user (Makefile)

Installs both binaries, a desktop launcher entry, an icon, and AppStream
metadata following the GNU directory conventions.

```sh
sudo make install                  # system-wide, into /usr/local
make install PREFIX="$HOME/.local" # per-user, no root needed
make uninstall                     # same PREFIX/DESTDIR to remove
```

`PREFIX` and `DESTDIR` are honoured, so `make DESTDIR=/tmp/stage install`
stages a packaging root without touching the live system.

### cargo install

```sh
cargo install --path crates/filesyncer-cli   # installs `filesyncer`
cargo install --path crates/filesyncer-gui   # installs `filesyncer-gui`
```

### Flatpak

The manifest and vendored crate list live in `build-aux/`. The build is
network-isolated, so crates are vendored via `build-aux/cargo-sources.json`;
regenerate it with `python3 build-aux/gen-cargo-sources.py` after `Cargo.lock`
changes.

```sh
flatpak install flathub org.gnome.Platform//49 org.gnome.Sdk//49 \
    org.freedesktop.Sdk.Extension.rust-stable//25.08
flatpak-builder --user --install --force-clean \
    build build-aux/net.jjbasken.FileSyncer.yml
flatpak run net.jjbasken.FileSyncer
```

## Usage

A **profile** pairs a local directory with a remote one under a short id.
Creating a profile validates both directories (they must exist, be distinct, and
not nest) and writes a `.filesyncer-root` marker into each.

### Command line

```sh
filesyncer add <id> <local-dir> <remote-dir>   # create a profile, stamp markers
filesyncer list                                 # show configured profiles
filesyncer sync <id> --dry-run                  # review the plan, change nothing
filesyncer sync <id>                            # review, confirm, apply
filesyncer sync <id> --yes                      # apply without the prompt
filesyncer sync <id> --force                    # override the deletion guard
filesyncer status <id>                          # paths, last sync, tracked count
```

Exit codes: `0` clean, `1` error, `2` conflicts present — so a scheduled job
(systemd timer, cron) can tell "nothing to do" from "needs attention".

### Desktop app

Launch `filesyncer-gui` (or start **FileSyncer** from your app menu). Pick a
profile in the sidebar — or add one with the **+** button — then **Scan** to see
the plan. Copies, trashings, conflicts, and skips are grouped; each conflict row
offers **Keep local** / **Keep remote**. **Apply** runs the sync with a live
progress bar you can **Cancel** at any point.

### Ignoring files

Drop a `.syncignore` file in either root to exclude paths, using gitignore-style
patterns. Rules from both roots are combined and applied to both sides.
`.gitignore` files are **not** honoured — only `.syncignore`. FileSyncer's own
`.filesyncer-root` and `.filesyncer-trash` entries are always excluded.

### Data locations

- Profiles: `$XDG_CONFIG_HOME/filesyncer/profiles.toml`
- Per-profile sync state: `$XDG_DATA_HOME/filesyncer/state/<id>.db`

Deleting a state database is safe: the next run re-baselines from the current
contents of both trees and never deletes as part of doing so.

## Layout

| Crate | Purpose |
| --- | --- |
| `filesyncer-core` | Scanning, reconciliation, state store, and apply — no UI, no stdout I/O. |
| `filesyncer-cli` | Thin command line frontend (`filesyncer`). |
| `filesyncer-gui` | GTK4 + libadwaita frontend (`filesyncer-gui`). |

## License

GPL-3.0-or-later.
