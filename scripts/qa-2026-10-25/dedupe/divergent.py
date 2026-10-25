#!/usr/bin/env python3
"""Bounded synthetic same-toolchain, different-content artifact comparison cost.

Uses 16 KiB per artifact and dry runs; it measures comparison planning/read cost,
not actual reflink saving. All pools belong to a new temporary directory.
"""

import json
import os
import pathlib
import subprocess
import sys
import tempfile
import time

for count in (8, 32, 128):
    with tempfile.TemporaryDirectory(prefix="buildq-divergent-") as directory:
        root = pathlib.Path(directory)
        q = root / "q"
        q.mkdir()
        cache = root / "cache"
        (q / "config.toml").write_text(f"""[cache]
dir = {json.dumps(str(cache))}
min_free = "0"
dedupe_min_size = "1"
dedupe_max_read = "8G"
""")
        for i in range(count):
            pool = cache / "pools" / f"project-{i:04}"
            deps = pool / "m0/debug/deps"
            deps.mkdir(parents=True)
            (pool / "pool.json").write_text(
                json.dumps({"toolchain_id": "synthetic-rustc-v1"})
            )
            (deps / "libsame-unit-hash.rlib").write_bytes(
                b"x" * (16384 - 1) + bytes([i])
            )
        env = dict(os.environ, BUILDQ_DIR=str(q), BUILDQ_CACHE=str(cache))
        started = time.monotonic()
        c = subprocess.run(
            [sys.argv[1], "cache", "dedupe", "--dry-run", "--json"],
            env=env,
            capture_output=True,
            text=True,
            check=True,
        )
        r = json.loads(c.stdout)
        print(
            json.dumps(
                {
                    "pools": count,
                    "bytes_per_artifact": 16384,
                    "wall_seconds": time.monotonic() - started,
                    "report": r,
                }
            )
        )
        assert (
            r["compared"] == count * (count - 1) // 2
            and r["differing"] == r["compared"]
        )
