"""Small release helper; run from the repository root with Python 3.11+."""

import hashlib
import re
import sys
import tarfile
import tomllib
import zipfile
from pathlib import Path

TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
)
VERSION = tomllib.loads(Path("Cargo.toml").read_text())["package"]["version"]


def archive_name(target):
    extension = "zip" if "windows" in target else "tar.gz"
    return f"buildq-v{VERSION}-{target}.{extension}"


def validate(tag):
    # Restrict tags before passing them to gh, and reject accidental version drift.
    if not re.fullmatch(
        r"v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?", tag
    ):
        raise SystemExit(f"Invalid version tag: {tag!r}")
    if tag != f"v{VERSION}":
        raise SystemExit(f"Tag must match Cargo.toml: v{VERSION}")


def package(target):
    if target not in TARGETS:
        raise SystemExit(f"Unsupported target: {target}")
    binary = "buildq.exe" if "windows" in target else "buildq"
    files = [
        (Path("target") / target / "release" / binary, binary),
        (Path("LICENSE-MIT"), "LICENSE-MIT"),
        (Path("LICENSE-APACHE"), "LICENSE-APACHE"),
    ]
    Path("dist").mkdir(exist_ok=True)
    output = Path("dist") / archive_name(target)
    if "windows" in target:
        with zipfile.ZipFile(output, "w", zipfile.ZIP_DEFLATED) as archive:
            for source, name in files:
                archive.write(source, name)
    else:
        with tarfile.open(output, "w:gz") as archive:
            for source, name in files:
                archive.add(source, arcname=name)
    print(output)


def checksums():
    expected = {archive_name(target) for target in TARGETS}
    actual = {path.name for path in Path("dist").iterdir()}
    if actual != expected:
        raise SystemExit(f"Unexpected release artifacts: {actual ^ expected}")
    lines = []
    for name in sorted(expected):
        path = Path("dist") / name
        with path.open("rb") as artifact:
            digest = hashlib.file_digest(artifact, "sha256").hexdigest()
        lines.append(f"{digest}  {name}\n")
    Path("dist/SHA256SUMS").write_text("".join(lines))


if __name__ == "__main__":
    match sys.argv[1:]:
        case ["validate", tag]:
            validate(tag)
        case ["package", target]:
            package(target)
        case ["checksums"]:
            checksums()
        case _:
            raise SystemExit(
                "Usage: release.py validate TAG | package TARGET | checksums"
            )
