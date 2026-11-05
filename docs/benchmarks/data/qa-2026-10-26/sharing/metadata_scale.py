#!/usr/bin/env python3
"""Probe dedupe indexing with Cargo-style uplifted hard links.

Files are sparse, 64 KiB logical; this measures metadata, not compilation or
real disk savings. --max-index controls candidate metadata; max-read=1 stops
after indexing the first batch. Linux reports child peak RSS in KiB.
"""
import argparse
import json
import os
import resource
from pathlib import Path
import subprocess
import time


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", type=Path, required=True)
    ap.add_argument("--buildq", type=Path, required=True)
    ap.add_argument("--files-per-member", type=int, default=10000)
    ap.add_argument("--members", type=int, default=4)
    ap.add_argument("--max-index", type=int, default=1000)
    ap.add_argument("--artifact-bytes", type=int, default=65536)
    ap.add_argument("--max-read", default="1")
    ap.add_argument("--hardlinks", action="store_true")
    args = ap.parse_args()
    root = args.root.resolve()
    root.mkdir(parents=True, exist_ok=True)
    (root / "q").mkdir(exist_ok=True)
    (root / "q/config.toml").write_text(
        "[cache]\nbudget='3G'\nmin_free='0'\naccounting='logical'\n"
        f"dedupe_min_size='{args.artifact_bytes}'\ndedupe_max_read='{args.max_read}'\n"
        f"dedupe_max_index={args.max_index}\n")
    pool = root / "cache/pools/metadata-probe"
    pool.mkdir(parents=True, exist_ok=True)
    (pool / "pool.json").write_text(json.dumps({"toolchain_id": "qa-synthetic-1.89"}))
    start = time.monotonic()
    for member in range(args.members):
        deps = pool / f"m{member}/debug/deps"
        deps.mkdir(parents=True, exist_ok=True)
        for n in range(args.files_per_member):
            file = deps / f"libprobe{n:06}-{n:016x}.rlib"
            if not file.exists():
                with file.open("wb") as out:
                    out.truncate(args.artifact_bytes)
            if args.hardlinks:
                alias = deps.parent / file.name
                if not alias.exists():
                    os.link(file, alias)
    setup = time.monotonic() - start
    env = os.environ.copy()
    for key in ["BUILDQ_CONFIG", "BUILDQ_HELD"]:
        env.pop(key, None)
    env.update(BUILDQ_DIR=str(root / "q"), BUILDQ_CACHE=str(root / "cache"))
    argv = [str(args.buildq), "cache", "dedupe", "--dry-run", "--json"]
    start = time.monotonic()
    p = subprocess.run(argv, env=env, capture_output=True, text=True)
    elapsed = time.monotonic()-start
    (root / "report.json").write_text(p.stdout)
    (root / "stderr.txt").write_text(p.stderr)
    report = json.loads(p.stdout)
    summary = dict(files_per_member=args.files_per_member, members=args.members,
                   hardlinks=args.hardlinks, max_index=args.max_index,
                   artifact_bytes=args.artifact_bytes, max_read=args.max_read,
                   exit=p.returncode, setup_seconds=setup, seconds=elapsed,
                   max_rss_kib=resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss,
                   report=report)
    (root / "summary.json").write_text(json.dumps(summary, indent=2)+"\n")
    print(json.dumps(summary), flush=True)


if __name__ == "__main__":
    main()
