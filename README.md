# FileSyncer

Bidirectional file synchroniser between a local directory and a mounted network
share. Changes are reviewed before anything is written, conflicts are flagged
rather than auto-merged, and deletions are routed through a recoverable trash
directory. It ships as a GTK4 desktop app (`filesyncer-gui`) and a matching
command line tool (`filesyncer`), both driving the same engine.

## Building

Requires a Rust toolchain (edition 2024, ≥ 1.96) and, for the GUI, GTK 4 and
libadwaita development libraries.

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

```sh
filesyncer add <id> <local-dir> <remote-dir>   # create a profile, stamp markers
filesyncer list                                 # show configured profiles
filesyncer sync <id> --dry-run                  # review the plan, change nothing
filesyncer sync <id>                            # review, confirm, apply
filesyncer status <id>                          # paths, last sync, tracked count
```

Exit codes: `0` clean, `1` error, `2` conflicts present (usable from a timer).

Profiles are stored in `$XDG_CONFIG_HOME/filesyncer/profiles.toml`; per-profile
sync state in `$XDG_DATA_HOME/filesyncer/state/<id>.db`.

## Layout

| Crate | Purpose |
| --- | --- |
| `filesyncer-core` | Scanning, reconciliation, state store, and apply — no UI, no stdout I/O. |
| `filesyncer-cli` | Thin command line frontend (`filesyncer`). |
| `filesyncer-gui` | GTK4 + libadwaita frontend (`filesyncer-gui`). |

## License

GPL-3.0-or-later.
