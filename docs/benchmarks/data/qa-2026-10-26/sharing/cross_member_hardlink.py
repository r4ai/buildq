#!/usr/bin/env python3
"""Reproduce dedupe treating cross-member hard links as unshared artifacts."""
import argparse
import json
import os
from pathlib import Path
import subprocess

ap = argparse.ArgumentParser()
ap.add_argument("--root", type=Path, required=True)
ap.add_argument("--buildq", type=Path, required=True)
args = ap.parse_args()
root = args.root.resolve()
(root / "q").mkdir(parents=True, exist_ok=True)
(root / "q/config.toml").write_text(
    "[cache]\nbudget='3G'\nmin_free='0'\naccounting='logical'\ndedupe_min_size='1'\n")
a = root / "cache/pools/cross/m0/debug/deps/libsame.rlib"
b = root / "cache/pools/cross/m1/debug/deps/libsame.rlib"
a.parent.mkdir(parents=True, exist_ok=True)
b.parent.mkdir(parents=True, exist_ok=True)
a.write_bytes(b"z" * 65536)
if not b.exists():
    os.link(a, b)
env = os.environ.copy()
env.pop("BUILDQ_CONFIG", None)
env.update(BUILDQ_DIR=str(root / "q"), BUILDQ_CACHE=str(root / "cache"))
reports = {}
for command in [["cache", "dedupe", "--dry-run", "--json"],
                ["cache", "dedupe", "--json"]]:
    p = subprocess.run([str(args.buildq)] + command, env=env,
                       text=True, capture_output=True)
    reports[" ".join(command)] = json.loads(p.stdout)
reports["identities"] = [{"dev": p.stat().st_dev, "ino": p.stat().st_ino,
                          "nlink": p.stat().st_nlink, "size": p.stat().st_size}
                         for p in [a, b]]
(root / "report.json").write_text(json.dumps(reports, indent=2) + "\n")
print(json.dumps(reports, indent=2))
