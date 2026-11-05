#!/usr/bin/env python3
"""Measure repeated dedupe on a volume whose capability is unsupported.

Creates two allocated, identical 256 MiB artifacts; reports actual reads and
elapsed/CPU cost. The artifacts are generated test data, not real OSS outputs.
"""
import json
import os
from pathlib import Path
import resource
import subprocess
import time

root = Path("/workspace/qa-sharing/unsupported-read")
root.mkdir(parents=True, exist_ok=True)
(root / "q").mkdir(exist_ok=True)
(root / "q/config.toml").write_text("[cache]\nbudget='3G'\nmin_free='0'\ndedupe_max_read='2G'\n")
for member in range(2):
    dest = root / f"cache/pools/unsupported/m{member}/debug/deps/liblarge-0123456789abcdef.rlib"
    dest.parent.mkdir(parents=True, exist_ok=True)
    with dest.open("wb") as out:
        for _ in range(256):
            out.write(b"01234567" * (1024*1024//8))
env = os.environ.copy()
env.pop("BUILDQ_CONFIG", None)
env.update(BUILDQ_DIR=str(root / "q"), BUILDQ_CACHE=str(root / "cache"))
records = []
for n in range(8):
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    start = time.monotonic()
    p = subprocess.run(["/workspace/qa-bin/buildq", "cache", "dedupe", "--json"],
                       env=env, capture_output=True, text=True)
    seconds = time.monotonic()-start
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    record = dict(iteration=n, seconds=seconds,
                  cpu_seconds=after.ru_utime+after.ru_stime-before.ru_utime-before.ru_stime,
                  exit=p.returncode, report=json.loads(p.stdout))
    records.append(record)
    (root / f"pass-{n}.json").write_text(json.dumps(record, indent=2)+"\n")
    print(json.dumps(record), flush=True)
(root / "results.json").write_text(json.dumps(records, indent=2)+"\n")
