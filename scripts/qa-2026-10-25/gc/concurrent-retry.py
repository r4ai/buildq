#!/usr/bin/env python3
"""Bounded repro: the last post-job GC request is lost on collector contention.

Usage: source the Rust toolchain environment, then:
  python3 concurrent-retry.py /absolute/path/to/buildq

The external flock models a collector holding gc.lock. It does not claim to
reproduce the complete timing of two real collectors. Only a new temporary
fixture, a 64 MiB payload and its disposable cache are modified.
"""

import fcntl
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

binary = str(Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix="buildq-gc-contention-") as directory:
    root = Path(directory)
    repo, state, cache = root / "repo", root / "state", root / "cache"
    (repo / "src").mkdir(parents=True)
    state.mkdir()
    cache.mkdir()
    (repo / "Cargo.toml").write_text(
        '[package]\nname="gc-contention"\nversion="0.1.0"\nedition="2021"\n'
    )
    (repo / "src/main.rs").write_text("fn main() {}\n")
    (repo / "build.rs").write_text(
        """use std::{env,fs,io::Write,path::Path};
fn main() {
    let mut out=fs::File::create(
        Path::new(&env::var("OUT_DIR").unwrap()).join("payload")
    ).unwrap();
    let chunk=vec![13u8;1024*1024];
    for _ in 0..64 { out.write_all(&chunk).unwrap(); }
    out.sync_all().unwrap();
}
"""
    )
    for args in [
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
        subprocess.run(["git", *args], cwd=repo, check=True, capture_output=True)
    (state / "config.toml").write_text(
        """slots=2
[cache]
budget="32MiB"
min_free="0"
gc_after_job=true
gc_mode="inline"
dedupe="off"
[cargo]
jobs=1
mirror_bins=false
"""
    )
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("BUILDQ_")
        and key not in {"CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"}
    }
    env.update(
        BUILDQ_DIR=str(state),
        BUILDQ_CACHE=str(cache),
        BUILDQ_REAL_CARGO=shutil.which("cargo"),
        RUSTUP_TOOLCHAIN="1.89.0",
    )

    def call(args):
        return subprocess.run(
            [binary, *args],
            cwd=repo,
            env=env,
            capture_output=True,
            text=True,
            check=True,
            timeout=60,
        )

    with (cache / "gc.lock").open("ab") as active_collector:
        fcntl.flock(active_collector, fcntl.LOCK_EX)
        started = time.monotonic()
        build = call(["cargo", "build", "--offline"])
        seconds = time.monotonic() - started
        post_job_metadata = [
            json.loads(path.read_text()) for path in cache.glob("pools/*/m*.json")
        ]
        while_locked = json.loads(call(["cache", "ls", "--refresh", "--json"]).stdout)
    # No later job/event is generated. Unlocking alone does not schedule a retry.
    time.sleep(0.1)
    after_unlock = json.loads(call(["cache", "ls", "--refresh", "--json"]).stdout)
    manual = json.loads(call(["gc", "--json"]).stdout)
    members = [m for p in after_unlock["pools"] for m in p["members"]]
    result = {
        "scenario": "external_collector_lock_blocks_last_post_job_gc",
        "build_seconds": seconds,
        "build_exit": build.returncode,
        "build_stderr": build.stderr,
        "budget": after_unlock["budget"],
        "bytes_while_collector_lock_held": while_locked["total"],
        "bytes_after_collector_unlock": after_unlock["total"],
        "all_members_idle": all(not m["busy"] for m in members),
        "post_job_metadata_before_refresh": post_job_metadata,
        "member_size_metadata": [m["meta"] for m in members],
        "over_budget": after_unlock["over_budget"],
        "over_budget_reasons": after_unlock["over_budget_reasons"],
        "trash_bytes": after_unlock["trash_bytes"],
        "manual_gc": manual,
        "scope": (
            "Deterministic lock-contention model; not a real concurrent-collector trace"
        ),
    }
    print(json.dumps(result, indent=2))
    assert build.returncode == 0
    assert result["all_members_idle"] and result["over_budget"]
    assert result["bytes_after_collector_unlock"] > result["budget"]
    assert manual["after"] == 0 and manual["freed"] > 0
