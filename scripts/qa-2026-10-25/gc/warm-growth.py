#!/usr/bin/env python3
"""Reproduce missed post-job GC after a warm member grows within size TTL.

Usage: python3 warm-growth.py /absolute/path/to/buildq
Requires cargo/rustc and git on PATH; uses only a fresh temporary directory.
"""

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

with tempfile.TemporaryDirectory(prefix="buildq-warm-growth-") as directory:
    root = Path(directory)
    repo = root / "repo"
    (repo / "src").mkdir(parents=True)
    (repo / "Cargo.toml").write_text(
        '[package]\nname="warm-growth"\nversion="0.1.0"\nedition="2021"\n'
    )
    (repo / "src/main.rs").write_text("fn main() {}\n")
    (repo / "build.rs").write_text("""use std::{env,fs,io::Write,path::Path};
fn main() {
    println!("cargo:rerun-if-env-changed=PAYLOAD_MIB");
    let n: usize=env::var("PAYLOAD_MIB").unwrap().parse().unwrap();
    let path=Path::new(&env::var("OUT_DIR").unwrap()).join("payload");
    let mut f=fs::File::create(path).unwrap();
    for _ in 0..n { f.write_all(&vec![13u8;1024*1024]).unwrap(); }
    f.sync_all().unwrap();
}
""")
    for arguments in [
        ["init", "-q"],
        ["add", "."],
        [
            "-c",
            "user.name=QA",
            "-c",
            "user.email=qa@example.invalid",
            "commit",
            "-qm",
            "QA",
        ],
    ]:
        subprocess.run(["git", *arguments], cwd=repo, check=True, capture_output=True)
    state = root / "state"
    state.mkdir()
    (state / "config.toml").write_text("""slots=1
[cache]
budget="32MiB"
min_free="0"
gc_after_job=true
gc_mode="inline"
dedupe="off"
[cargo]
jobs=1
mirror_bins=false
""")
    env = {
        k: v
        for k, v in os.environ.items()
        if not k.startswith("BUILDQ_")
        and k not in {"CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"}
    }
    env.update(BUILDQ_DIR=str(state), BUILDQ_CACHE=str(root / "cache"))
    binary = str(Path(sys.argv[1]).resolve())

    def run(arguments, extra=None):
        return subprocess.run(
            [binary, *arguments],
            cwd=repo,
            env=env | (extra or {}),
            capture_output=True,
            text=True,
            check=True,
        )

    run(["cargo", "build", "--offline"], {"PAYLOAD_MIB": "1"})
    large = run(["cargo", "build", "--offline"], {"PAYLOAD_MIB": "64"})
    cached = json.loads(run(["cache", "ls", "--json"]).stdout)
    refreshed = json.loads(run(["cache", "ls", "--refresh", "--json"]).stdout)
    gc = json.loads(run(["gc", "--json"]).stdout)
    result = dict(
        budget=cached["budget"],
        cached_total=cached["total"],
        refreshed_total=refreshed["total"],
        warning="exceeds the entire cache budget" in large.stderr,
        manual_gc_freed=gc["freed"],
        manual_gc_after=gc["after"],
    )
    print(json.dumps(result, indent=2))
    assert cached["total"] < cached["budget"] < refreshed["total"], (
        "Behavior changed; reassess finding"
    )
    assert not result["warning"] and gc["after"] == 0
