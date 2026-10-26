#!/usr/bin/env python3
"""Fault injection: uncollected hidden partial seed after process death.

This constructs the interrupted on-disk state; it does NOT claim to kill an
actual reflink clone on this unsupported filesystem. 64 MiB allocated data
models an abandoned clone after its original source no longer holds the data.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import time

ap = argparse.ArgumentParser()
ap.add_argument("--root", type=Path, required=True)
ap.add_argument("--buildq", type=Path, required=True)
args = ap.parse_args()
root = args.root.resolve()
(root / "q").mkdir(parents=True, exist_ok=True)
(root / "q/config.toml").write_text(
    "[cache]\nbudget='1M'\nmin_free='0'\ndedupe='off'\n")
pool = root / "cache/pools/recent"
pool.mkdir(parents=True, exist_ok=True)
now = int(time.time() * 1000)
(pool / "pool.json").write_text(json.dumps({"toolchain_id": "qa-1.89",
                                           "last_used_ms": now, "created_ms": now}))
for member in [0, 1]:
    (pool / f"m{member}").mkdir(exist_ok=True)
    (pool / f"m{member}.lock").touch()
    (pool / f"m{member}.json").write_text(json.dumps(
        {"id": member, "last_used_ms": now, "created_ms": now}))
    (pool / f"m{member}.manifest").write_text("{}")
(pool / "m0/live").write_bytes(b"live")
(pool / ".seed-m1").mkdir(exist_ok=True)
partial = pool / ".seed-m1/partial-artifact"
with partial.open("wb") as out:
    for _ in range(64):
        out.write(b"x" * 1048576)
env = os.environ.copy()
env.pop("BUILDQ_CONFIG", None)
env.update(BUILDQ_DIR=str(root / "q"), BUILDQ_CACHE=str(root / "cache"))
records = []
for command in [["cache", "ls", "--refresh", "--json"], ["gc", "--json"],
                ["cache", "ls", "--refresh", "--json"]]:
    p = subprocess.run([str(args.buildq)] + command, env=env,
                       text=True, capture_output=True)
    records.append(dict(command=command, exit=p.returncode,
                        report=json.loads(p.stdout)))
result = dict(fault_injection=True, natural_kill_during_reflink_reproduced=False,
              records=records, partial_exists_after_gc=partial.exists(),
              partial_bytes=partial.stat().st_size if partial.exists() else 0,
              du=subprocess.check_output(["du", "-sk", str(root / "cache")], text=True).strip())
(root / "report.json").write_text(json.dumps(result, indent=2)+"\n")
print(json.dumps(result, indent=2))
