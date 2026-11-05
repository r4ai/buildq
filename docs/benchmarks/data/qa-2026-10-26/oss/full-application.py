#!/usr/bin/env python3
"""Measure complete rust-analyzer application builds, then run its staged binary."""

import argparse
import os
from pathlib import Path
from buildq_bench.measurement import measure

p = argparse.ArgumentParser()
p.add_argument("--root", type=Path, required=True)
p.add_argument("--buildq", required=True)
p.add_argument(
    "--skip-initial",
    action="store_true",
    help="Measure warm repeats after a separately recorded initial build",
)
a = p.parse_args()
root = a.root.resolve()
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
    BUILDQ_TRACE="1",
)
for name in ([] if a.skip_initial else ["full-rust-analyzer"]) + [
    "full-rust-analyzer-warm-1",
    "full-rust-analyzer-warm-2",
    "full-rust-analyzer-warm-3",
    "full-rust-analyzer-version",
]:
    command = (
        [a.buildq, "cargo", "build", "-p", "rust-analyzer"]
        if not name.endswith("-version")
        else [a.buildq, "cargo", "run", "-p", "rust-analyzer", "--", "--version"]
    )
    r = measure(
        name,
        root / "worktrees/rust-analyzer-0",
        root / "logs/full",
        command,
        {**env, "BUILDQ_LABEL": name},
    )
    if r.exit:
        raise SystemExit(r.exit)
