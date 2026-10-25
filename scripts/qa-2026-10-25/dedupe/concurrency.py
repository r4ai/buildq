#!/usr/bin/env python3
"""Check competing pass rejection, SIGKILL recovery and member lock exclusion."""

import fcntl
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import time

with tempfile.TemporaryDirectory(prefix="buildq-dedupe-concurrency-") as directory:
    root = pathlib.Path(directory)
    q = root / "q"
    q.mkdir()
    cache = root / "cache"
    cache.mkdir()
    (q / "config.toml").write_text(f"""[cache]
dir = {json.dumps(str(cache))}
min_free = "0"
budget = "1G"
dedupe_min_size = "1"
dedupe_max_read = "8G"
""")
    for member in range(2):
        path = cache / "pools/p" / f"m{member}/debug/deps"
        path.mkdir(parents=True)
        # Sparse zero files avoid creating bulk data but exercise the real read path.
        for i in range(4):
            with (path / f"libtest{i}.rlib").open("wb") as out:
                out.truncate(128 * 1024 * 1024)
    env = dict(os.environ, BUILDQ_DIR=str(q), BUILDQ_CACHE=str(cache))
    binary = sys.argv[1]
    command = [binary, "cache", "dedupe", "--dry-run", "--json"]
    owner = subprocess.Popen(command, env=env, stdout=subprocess.DEVNULL)
    deadline = time.monotonic() + 2
    locked = False
    while time.monotonic() < deadline and owner.poll() is None:
        try:
            with (cache / "dedupe.lock").open("rb") as lock:
                try:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    locked = True
                    break
        except FileNotFoundError:
            pass
    assert locked, "could not observe the owner lock"
    competitor = subprocess.run(command, env=env, capture_output=True, text=True)
    print(
        json.dumps(
            {
                "scenario": "competing_pass",
                "exit": competitor.returncode,
                "report": json.loads(competitor.stdout),
            }
        )
    )
    assert competitor.returncode == 1 and json.loads(competitor.stdout)["skipped"]
    owner.kill()
    owner.wait()
    recovered = subprocess.run(command, env=env, capture_output=True, text=True)
    print(
        json.dumps(
            {
                "scenario": "after_sigkill",
                "exit": recovered.returncode,
                "report": json.loads(recovered.stdout),
            }
        )
    )
    assert recovered.returncode == 0 and json.loads(recovered.stdout)["identical"] == 4
    with (cache / "pools/p/m0.lock").open("ab") as member_lock:
        fcntl.flock(member_lock, fcntl.LOCK_EX)
        dry = subprocess.run(command, env=env, capture_output=True, text=True)
        gc = subprocess.run(
            [binary, "gc", "--dry-run", "--json"],
            env=env,
            capture_output=True,
            text=True,
        )
    print(
        json.dumps(
            {
                "scenario": "locked_member",
                "dedupe": json.loads(dry.stdout),
                "gc": json.loads(gc.stdout),
            }
        )
    )
    assert (
        json.loads(dry.stdout)["skipped_busy"] == 1
        and json.loads(dry.stdout)["compared"] == 0
    )
