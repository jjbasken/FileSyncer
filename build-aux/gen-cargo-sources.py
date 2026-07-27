#!/usr/bin/env python3
"""Generate cargo-sources.json for the Flatpak build from Cargo.lock.

Flatpak builds have no network access, so every crate must be vendored ahead of
time. This emits the flatpak-builder source list that downloads each crate from
crates.io, drops in its ``.cargo-checksum.json``, and writes a cargo ``config``
that redirects crates-io to the vendored directory. The output format matches
the official ``flatpak-cargo-generator.py`` but uses only the standard library.

Run from the repo root after Cargo.lock changes:

    python3 build-aux/gen-cargo-sources.py

Only supports crates.io registry dependencies (this project has no git deps).
"""

import json
import sys
import tomllib
from pathlib import Path

CRATES_IO = "https://static.crates.io/crates"
CARGO_HOME = "cargo"
CARGO_CRATES = f"{CARGO_HOME}/vendor"


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    lock_path = root / "Cargo.lock"
    with lock_path.open("rb") as f:
        lock = tomllib.load(f)

    sources = []
    for pkg in lock.get("package", []):
        source = pkg.get("source")
        # Workspace-local crates have no source; skip them.
        if source is None:
            continue
        if not source.startswith("registry+"):
            sys.exit(f"unsupported non-registry source for {pkg['name']}: {source}")
        checksum = pkg.get("checksum")
        if checksum is None:
            sys.exit(f"{pkg['name']} {pkg['version']} has no checksum in Cargo.lock")

        name = pkg["name"]
        version = pkg["version"]
        dest = f"{CARGO_CRATES}/{name}-{version}"
        sources.append(
            {
                "type": "archive",
                "archive-type": "tar-gzip",
                "url": f"{CRATES_IO}/{name}/{name}-{version}.crate",
                "sha256": checksum,
                "dest": dest,
            }
        )
        sources.append(
            {
                "type": "inline",
                "contents": json.dumps({"package": checksum, "files": {}}),
                "dest": dest,
                "dest-filename": ".cargo-checksum.json",
            }
        )

    # cargo config redirecting crates-io to the vendored directory.
    config = (
        "[source.vendored-sources]\n"
        f'directory = "{CARGO_CRATES}"\n'
        "\n"
        "[source.crates-io]\n"
        'replace-with = "vendored-sources"\n'
    )
    sources.append(
        {
            "type": "inline",
            "contents": config,
            "dest": CARGO_HOME,
            "dest-filename": "config",
        }
    )

    out_path = root / "build-aux" / "cargo-sources.json"
    with out_path.open("w", encoding="utf-8") as f:
        json.dump(sources, f, indent=4)
        f.write("\n")

    crates = sum(1 for s in sources if s["type"] == "archive")
    print(f"wrote {out_path.relative_to(root)} with {crates} crates")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
