#!/usr/bin/env python3
"""Develop two divergent Tokio worktrees with equal source mtimes, switch repeatedly."""

import argparse
import json
import os
import subprocess
import time
from pathlib import Path

p = argparse.ArgumentParser()
p.add_argument("--root", type=Path, required=True)
p.add_argument("--buildq", required=True)
a = p.parse_args()
root = a.root.resolve()
logs = root / "logs" / "real-edit"
logs.mkdir(parents=True, exist_ok=True)
env = {
    k: v
    for k, v in os.environ.items()
    if not k.startswith("BUILDQ_")
    and k not in ("CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR")
}
env.update(
    BUILDQ_DIR=str(root / "state"),
    BUILDQ_CACHE=str(root / "cache"),
    BUILDQ_CONFIG=str(root / "state/config.toml"),
    CARGO_HOME=str(root / "cargo-home"),
    CARGO_BUILD_JOBS="1",
    TMPDIR=str(root / "tmp"),
    RUSTC_WRAPPER="",
)
paths = []
for idx in (3, 4):
    path = root / "worktrees" / f"tokio-edit-{idx}"
    paths.append(path)
    if not path.exists():
        subprocess.run(
            [
                "git",
                "-C",
                str(root / "repos" / "tokio"),
                "worktree",
                "add",
                "--detach",
                str(path),
            ],
            check=True,
        )
    lib = path / "tokio/src/lib.rs"
    lib.write_text(
        subprocess.check_output(
            ["git", "-C", str(path), "show", "HEAD:tokio/src/lib.rs"], text=True
        )
        + f"\n#[doc(hidden)]\npub const BUILDQ_QA_MARKER: u32 = {idx};\n"
    )
    os.utime(lib, (1700000000, 1700000000))
    (path / "tokio/tests/buildq_qa_marker.rs").write_text(
        f"#[test]\nfn divergent_worktree_marker() {{ assert_eq!(tokio::BUILDQ_QA_MARKER, {idx}); }}\n"
    )
results = []
for i in range(8):
    path = paths[i % 2]
    features = ["--features", "full"] if i // 2 % 2 else []
    cmd = [
        a.buildq,
        "cargo",
        "test",
        "-p",
        "tokio@1.45.1",
        "--test",
        "buildq_qa_marker",
        *features,
        "--",
        "--nocapture",
    ]
    start = time.monotonic()
    with (
        (logs / f"{i:02d}.out").open("wb") as out,
        (logs / f"{i:02d}.err").open("wb") as err,
    ):
        r = subprocess.run(
            cmd,
            cwd=path,
            env={**env, "BUILDQ_LABEL": f"qa-real-edit-{i}"},
            stdout=out,
            stderr=err,
            timeout=900,
        )
    results.append(
        {
            "index": i,
            "cwd": str(path),
            "command": cmd,
            "exit": r.returncode,
            "duration_s": round(time.monotonic() - start, 3),
        }
    )
    (logs / "results.json").write_text(json.dumps(results, indent=2) + "\n")
print(json.dumps(results, indent=2))
