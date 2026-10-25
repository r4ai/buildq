#!/usr/bin/env python3
"""Verify embedded checkout paths and include_str! inputs across warm worktrees.

Uses two local worktrees, one shared pool member per backend, equal-sized marker
variants, and a fixed backdated nanosecond mtime. Incremental is disabled to make
compiler outputs eligible for mbx action caching. No external dependencies.
"""

import argparse
import json
import os
import re
import time
from pathlib import Path

from qa_support import fresh_paths, run

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--root", type=Path, required=True)
parser.add_argument("--out", type=Path, required=True)
parser.add_argument("--binary", default=os.environ.get("BUILDQ_BIN"))
parser.add_argument("--mbx", default=os.environ.get("MBX_BIN"))
parser.add_argument(
    "--same-marker-only",
    action="store_true",
    help="use identical included bytes in both roots to isolate checkout-path keys",
)
args = parser.parse_args()
if not args.binary or not args.mbx:
    parser.error("--binary/--mbx or BUILDQ_BIN/MBX_BIN are required")
root, out = fresh_paths(args.root, args.out)
repo = root / "wt0"
(repo / "src").mkdir(parents=True)
(repo / "Cargo.toml").write_text(
    '[package]\nname="qa-embedded-paths"\nversion="0.1.0"\nedition="2024"\n'
)
(repo / "marker.txt").write_text("ALPHA")
(repo / "src/absolute.rs").write_text(
    "fn absolute_source_file() -> &'static str { file!() }\n"
)
(
    repo / "src/main.rs"
).write_text("""include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/absolute.rs"));
fn main() {
 println!("manifest={}",env!("CARGO_MANIFEST_DIR"));
 println!("file={}",file!());
 println!("absolute_file={}",absolute_source_file());
 println!("manifest_marker={}",include_str!(concat!(env!("CARGO_MANIFEST_DIR"),"/marker.txt")));
 println!("relative_marker={}",include_str!("../marker.txt"));
 println!("cwd={}",std::env::current_dir().unwrap().display());
}
""")
for command in [
    ["git", "init", "-q", str(repo)],
    ["git", "-C", str(repo), "add", "."],
    [
        "git",
        "-C",
        str(repo),
        "-c",
        "user.name=QA",
        "-c",
        "user.email=qa@example.invalid",
        "commit",
        "-qm",
        "embedded-path fixture",
    ],
    ["git", "-C", str(repo), "worktree", "add", "--detach", str(root / "wt1"), "HEAD"],
]:
    run(command, check=True, capture_output=True, text=True, timeout=10)
fixed_ns = 946684800000000000
for index, marker in [(0, "ALPHA"), (1, "BRAVO")]:
    path = root / f"wt{index}" / "marker.txt"
    path.write_text(marker)
    os.utime(path, ns=(fixed_ns, fixed_ns))
base = {
    key: value
    for key, value in os.environ.items()
    if not key.startswith(("BUILDQ_", "MBX_"))
    and key not in {"CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR", "RUSTC_WRAPPER"}
}
base.update(
    CARGO_INCREMENTAL="0",
    CARGO_PROFILE_DEV_DEBUG="0",
    CARGO_BUILD_JOBS="1",
    MBX_DISPLAY="plain",
    MBX_SUMMARY="full",
    MBX_GC_AUTO="0",
    MBX_CACHE_DIR=str(root / "mbx-cache"),
)
for backend in ["cargo", "mbx"]:
    q = root / (backend + "-queue")
    q.mkdir()
    (q / "config.toml").write_text(f"""slots=1
run_slots=1
poll_ms=10
[cache]
dir={json.dumps(str(root / (backend + "-pool")))}
max_members=1
budget="512MiB"
min_free="0"
gc_after_job=false
[cargo]
backend={json.dumps(backend)}
mbx={json.dumps(str(Path(args.mbx).resolve()))}
""")
rows = []
sequence = [
    ("initial-0", 0, "ALPHA"),
    ("initial-1", 1, "BRAVO"),
    ("return-0", 0, "ALPHA"),
    ("same-root-edit-0", 0, "DELTA"),
    ("return-1", 1, "BRAVO"),
    ("return-edited-0", 0, "DELTA"),
]
if args.same_marker_only:
    sequence = [
        ("identical-initial-0", 0, "ALPHA"),
        ("identical-other-root-1", 1, "ALPHA"),
        ("identical-return-0", 0, "ALPHA"),
    ]
for phase, index, marker in sequence:
    wt = root / f"wt{index}"
    path = wt / "marker.txt"
    path.write_text(marker)
    os.utime(path, ns=(fixed_ns, fixed_ns))
    for backend in ["cargo", "mbx"]:
        env = base | {
            "BUILDQ_DIR": str(root / (backend + "-queue")),
            "BUILDQ_CACHE": str(root / (backend + "-pool")),
        }
        started = time.monotonic()
        result = run(
            [args.binary, "cargo", "run", "--offline"],
            cwd=wt,
            env=env,
            capture_output=True,
            text=True,
            timeout=30,
        )
        values = dict(
            line.split("=", 1) for line in result.stdout.splitlines() if "=" in line
        )
        expected = {
            "manifest": str(wt),
            "absolute_file": str(wt / "src/absolute.rs"),
            "manifest_marker": marker,
            "relative_marker": marker,
            "cwd": str(wt),
        }
        ordinary_file = values.get("file", "")
        file_correct = ordinary_file == "src/main.rs" or ordinary_file == str(
            wt / "src/main.rs"
        )
        passed = (
            result.returncode == 0
            and all(values.get(key) == value for key, value in expected.items())
            and file_correct
        )
        name = f"{backend}-{phase}"
        (out / (name + ".log")).write_text(result.stdout + result.stderr)
        row = dict(
            name=name,
            backend=backend,
            phase=phase,
            worktree=index,
            exit=result.returncode,
            expected=expected,
            actual=values,
            file_macro_correct=file_correct,
            marker_mtime_ns=path.stat().st_mtime_ns,
            marker_bytes=path.stat().st_size,
            cache_hits=sum(
                map(int, re.findall(r"mbx\[cache\]: (\d+) hits", result.stderr))
            ),
            seconds=round(time.monotonic() - started, 6),
            passed=passed,
        )
        rows.append(row)
        print(json.dumps(row), flush=True)
for name, command in [
    ("mbx-stats", [args.mbx, "stats", "--json"]),
    ("mbx-cache", [args.mbx, "cache", "stats", "--json"]),
]:
    result = run(
        command,
        cwd=repo,
        env=base,
        capture_output=True,
        text=True,
        check=True,
        timeout=20,
    )
    (out / (name + ".json")).write_text(result.stdout)
provenance = {
    "rustc": run(
        ["rustc", "-Vv"], env=base, capture_output=True, text=True, check=True
    ).stdout,
    "mbx": run(
        [args.mbx, "--version"], capture_output=True, text=True, check=True
    ).stdout.strip(),
    "cargo_incremental": "0",
    "fixed_marker_mtime_ns": fixed_ns,
    "same_marker_only": args.same_marker_only,
    "results": rows,
}
(out / "results.json").write_text(json.dumps(provenance, indent=2) + "\n")
print(
    f"{sum(row['passed'] for row in rows)}/{len(rows)} passed; "
    f"real mbx hits={sum(row['cache_hits'] for row in rows)}"
)
