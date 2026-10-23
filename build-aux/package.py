#!/usr/bin/env python3
"""Package already-built release binaries as a Debian or Fedora installer."""

import argparse
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parent.parent
APP_ID = "net.jjbasken.FileSyncer"


def run(*args, **kwargs):
    return subprocess.check_output(args, text=True, **kwargs).strip()


def stage(destination):
    files = {
        "target/release/filesyncer": "usr/bin/filesyncer",
        "target/release/filesyncer-gui": "usr/bin/filesyncer-gui",
        f"data/{APP_ID}.desktop": f"usr/share/applications/{APP_ID}.desktop",
        f"data/icons/{APP_ID}.svg": f"usr/share/icons/hicolor/scalable/apps/{APP_ID}.svg",
        f"data/{APP_ID}.metainfo.xml": f"usr/share/metainfo/{APP_ID}.metainfo.xml",
        "LICENSE": "usr/share/doc/filesyncer/LICENSE",
        "README.md": "usr/share/doc/filesyncer/README.md",
    }
    for source, target in files.items():
        target = destination / target
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / source, target)
        target.chmod(0o755 if "/bin/" in str(target) else 0o644)


def deb(work, output, version, revision):
    tree = work / "filesyncer"
    stage(tree)
    # dpkg-shlibdeps needs source-package context, and determines dependencies
    # from the actual ELF binaries and the build distribution's library data.
    (work / "debian").mkdir()
    (work / "debian/control").write_text("Source: filesyncer\n\nPackage: filesyncer\nArchitecture: any\n")
    deps = run("dpkg-shlibdeps", "-O", "-e" + str(tree / "usr/bin/filesyncer"),
               "-e" + str(tree / "usr/bin/filesyncer-gui"), cwd=work)
    deps = next(line.removeprefix("shlibs:Depends=") for line in deps.splitlines()
                if line.startswith("shlibs:Depends="))
    arch = run("dpkg", "--print-architecture")
    control = tree / "DEBIAN"
    control.mkdir()
    (control / "control").write_text(
        f"Package: filesyncer\nVersion: {version}+git.{revision}\n"
        f"Architecture: {arch}\nMaintainer: Jeremy Basken <jjbasken@proton.me>\n"
        f"Installed-Size: {sum(p.stat().st_size for p in tree.rglob('*') if p.is_file()) // 1024 + 1}\n"
        f"Depends: {deps}\nSection: utils\nPriority: optional\n"
        "Homepage: https://github.com/jjbasken/FileSyncer\n"
        "Description: Bidirectional file synchroniser\n"
        " Review changes and resolve conflicts before syncing local directories\n"
        " and mounted network shares, using the desktop app or command line.\n"
    )
    path = output / f"filesyncer_{version}+git.{revision}_{arch}.deb"
    subprocess.run(["dpkg-deb", "--root-owner-group", "--build", str(tree), str(path)], check=True)
    subprocess.run(["dpkg-deb", "--info", str(path)], check=True)
    return [path]


def rpm(work, output, version, revision):
    for name in ("BUILD", "BUILDROOT", "RPMS", "SOURCES", "SPECS", "SRPMS"):
        (work / name).mkdir()
    source = work / f"filesyncer-{version}"
    stage(source)
    archive = work / "SOURCES" / f"filesyncer-{version}.tar.gz"
    subprocess.run(["tar", "-czf", str(archive), "-C", str(work), source.name], check=True)
    spec = work / "SPECS/filesyncer.spec"
    spec.write_text(f"""Name: filesyncer
Version: {version}
Release: 0.git.{revision}%{{?dist}}
Summary: Bidirectional file synchroniser
License: GPL-3.0-or-later
URL: https://github.com/jjbasken/FileSyncer
Source0: filesyncer-{version}.tar.gz

%description
Review changes and resolve conflicts before syncing local directories and
mounted network shares, using the desktop app or command line.

%prep
%setup -q

%install
mkdir -p "%{{buildroot}}"
cp -a usr "%{{buildroot}}/"

%files
/usr/bin/filesyncer
/usr/bin/filesyncer-gui
/usr/share/applications/{APP_ID}.desktop
/usr/share/icons/hicolor/scalable/apps/{APP_ID}.svg
/usr/share/metainfo/{APP_ID}.metainfo.xml
%license /usr/share/doc/filesyncer/LICENSE
%doc /usr/share/doc/filesyncer/README.md
""")
    # rpmbuild automatically generates ELF library and symbol requirements.
    subprocess.run(["rpmbuild", "--define", f"_topdir {work}",
                    "--define", "debug_package %{nil}", "-bb", str(spec)], check=True)
    results = []
    for path in (work / "RPMS").rglob("*.rpm"):
        result = output / path.name
        shutil.copyfile(path, result)
        subprocess.run(["rpm", "-qp", "--requires", str(result)], check=True)
        results.append(result)
    if not results:
        raise RuntimeError("rpmbuild produced no RPM")
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("format", choices=("deb", "rpm"))
    parser.add_argument("--output-dir", type=Path, default=ROOT / "dist")
    args = parser.parse_args()
    args.output_dir = args.output_dir.resolve()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    with (ROOT / "Cargo.toml").open("rb") as file:
        version = tomllib.load(file)["workspace"]["package"]["version"]
    timestamp = run("git", "log", "-1", "--format=%ct", cwd=ROOT)
    commit = run("git", "rev-parse", "--short=12", "HEAD", cwd=ROOT)
    os.environ.setdefault("SOURCE_DATE_EPOCH", timestamp)
    revision = f"{timestamp}.{commit}"
    with tempfile.TemporaryDirectory(prefix="filesyncer-package-") as directory:
        results = {"deb": deb, "rpm": rpm}[args.format](
            Path(directory), args.output_dir, version, revision)
    for path in results:
        print(path)


if __name__ == "__main__":
    main()
