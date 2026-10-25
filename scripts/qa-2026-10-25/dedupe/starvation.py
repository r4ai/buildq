#!/usr/bin/env python3
"""Small repro: an oversized first file starves a later in-budget pair.

python3 starvation.py /absolute/path/to/buildq
Only a new temporary directory is touched; no reflink-capable FS is needed.
"""

import json
import os
import pathlib
import subprocess
import sys
import tempfile

with tempfile.TemporaryDirectory(prefix="buildq-dedupe-starvation-") as directory:
    root = pathlib.Path(directory)
    queue = root / "queue"
    queue.mkdir()
    cache = root / "cache"
    (queue / "config.toml").write_text(f"""[cache]
dir = {json.dumps(str(cache))}
min_free = "0"
dedupe_min_size = "1"
dedupe_max_read = "1M"
""")
    for member in range(2):
        deps = cache / "pools/p" / f"m{member}/debug/deps"
        deps.mkdir(parents=True)
        for name, size in [
            ("libaaa-large.rlib", 600 * 1024),
            ("libzzz-small.rlib", 64 * 1024),
        ]:
            (deps / name).write_bytes(b"x" * size)
    env = dict(os.environ, BUILDQ_DIR=str(queue), BUILDQ_CACHE=str(cache))
    reports = []
    for i in range(3):
        c = subprocess.run(
            [sys.argv[1], "cache", "dedupe", "--json"],
            env=env,
            check=True,
            capture_output=True,
            text=True,
        )
        report = json.loads(c.stdout)
        reports.append(report)
        print(
            json.dumps(
                {
                    "pass": i,
                    "compared": report["compared"],
                    "read_bytes": report["read_bytes"],
                    "read_limited": report["read_limited"],
                    "resume": json.loads((cache / "dedupe.json").read_text()).get(
                        "resume"
                    ),
                }
            )
        )
    # Both pairs fit this total read budget, demonstrated without changing data.
    c = subprocess.run(
        [sys.argv[1], "cache", "dedupe", "--dry-run", "--max-read", "2M", "--json"],
        env=env,
        check=True,
        capture_output=True,
        text=True,
    )
    print(json.dumps({"override": json.loads(c.stdout)}))
    assert all(r["compared"] == 0 and r["read_limited"] for r in reports), (
        "HEAD behavior changed: reassess the issue"
    )
    assert json.loads(c.stdout)["identical"] == 2
