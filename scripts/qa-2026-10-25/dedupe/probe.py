#!/usr/bin/env python3
"""Reproducible, isolated dedupe QA; does not modify buildq or external repos.

Usage: python3 probe.py --buildq /abs/path/buildq --root /workspace/qa-dedupe-runtime
Results are JSON; filesystem success is distinguished from dry-run discovery.
"""

import argparse
import fcntl
import hashlib
import json
import os
import pathlib
import shutil
import struct
import subprocess
import tempfile
import time

p = argparse.ArgumentParser()
p.add_argument("--buildq", required=True)
p.add_argument("--root", required=True)
p.add_argument("--scale", type=int, default=10000)
a = p.parse_args()
root = pathlib.Path(a.root).resolve()
root.mkdir(parents=True, exist_ok=True)
results = []


def setup(name, max_read="8G"):
    r = root / name
    if r.exists():
        shutil.rmtree(r)
    q = r / "queue"
    q.mkdir(parents=True)
    cache = r / "cache"
    cache.mkdir()
    (q / "config.toml").write_text(f'''slots = 2
[cache]
dir = {json.dumps(str(cache))}
min_free = "0"
budget = "4G"
accounting = "allocated"
gc_after_job = false
dedupe_min_size = "1"
dedupe_max_read = "{max_read}"
''')
    return r, dict(os.environ, BUILDQ_DIR=str(q), BUILDQ_CACHE=str(cache))


def put(r, member, rel, data, pool="p", sparse_len=None):
    f = r / "cache" / "pools" / pool / f"m{member}" / rel
    f.parent.mkdir(parents=True, exist_ok=True)
    f.write_bytes(data)
    if sparse_len is not None:
        with f.open("r+b") as out:
            out.truncate(sparse_len)
    return f


def run(name, env, args):
    t = time.monotonic()
    with (
        tempfile.TemporaryFile(mode="w+t") as out,
        tempfile.TemporaryFile(mode="w+t") as err,
    ):
        c = subprocess.Popen([a.buildq, *args], env=env, stdout=out, stderr=err)
        _, status, usage = os.wait4(c.pid, 0)
        c.returncode = os.waitstatus_to_exitcode(status)
        out.seek(0)
        stdout = out.read()
        err.seek(0)
        stderr = err.read()
    try:
        report = json.loads(stdout)
    except ValueError:
        report = {"stdout": stdout, "stderr": stderr}
    item = {
        "scenario": name,
        "wall_seconds": round(time.monotonic() - t, 6),
        "cpu_user_seconds": usage.ru_utime,
        "cpu_system_seconds": usage.ru_stime,
        "peak_rss_kib": usage.ru_maxrss,
        "exit": c.returncode,
        "report": report,
    }
    if stderr:
        item["stderr"] = stderr
    results.append(item)
    return item


def identity(f):
    s = f.stat()
    return {
        "sha256": hashlib.sha256(f.read_bytes()).hexdigest(),
        "size": s.st_size,
        "ino": s.st_ino,
        "mode": s.st_mode,
        "mtime_ns": s.st_mtime_ns,
        "nlink": s.st_nlink,
    }


r, env = setup("safety")
files = []
for member in range(4):
    for i, size in enumerate([4096, 65536, 1048577]):
        files.append(
            put(r, member, f"debug/deps/libsame{i}-abcd.rlib", bytes([i + 1]) * size)
        )
    files.append(
        put(r, member, "debug/deps/libdifferent-abcd.rlib", bytes([member]) * 65536)
    )
    put(r, member, "debug/incremental/private/cache.bin", b"private" * 10000)
    put(r, member, "debug/.fingerprint/private/stamp", b"private" * 10000)
    os.link(files[-2], files[-2].parent.parent / "libsame2.rlib")
    files.append(files[-2].parent.parent / "libsame2.rlib")
before = {str(f.relative_to(r)): identity(f) for f in files}
run("safety_dry_run", env, ["cache", "dedupe", "--dry-run", "--json"])
run("safety_real_or_unsupported", env, ["cache", "dedupe", "--json"])
after = {str(f.relative_to(r)): identity(f) for f in files}
results.append(
    {
        "scenario": "safety_identities",
        "unchanged": before == after,
        "files_checked": len(files),
        "before": before,
        "after": after,
    }
)
lockpath = r / "cache/pools/p/m1.lock"
lockpath.touch()
with lockpath.open("rb") as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    run("busy_member_skipped", env, ["cache", "dedupe", "--dry-run", "--json"])
lockpath = r / "cache/dedupe.lock"
with lockpath.open("rb") as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    run("dedupe_lock_skipped", env, ["cache", "dedupe", "--json"])

r, env = setup("oversized", "1M")
for m in range(2):
    put(r, m, "debug/deps/libaaa-large.rlib", b"A" * (600 * 1024))
    put(r, m, "debug/deps/libzzz-small.rlib", b"B" * (64 * 1024))
for i in range(3):
    run(f"oversized_dry_{i}", env, ["cache", "dedupe", "--dry-run", "--json"])
for i in range(3):
    run(f"oversized_real_{i}", env, ["cache", "dedupe", "--json"])
run(
    "oversized_override",
    env,
    ["cache", "dedupe", "--dry-run", "--max-read", "2M", "--json"],
)

r, env = setup("many_files", "1")
for member in range(2):
    for i in range(a.scale // 2):
        put(r, member, f"debug/deps/libprobe-{i:016x}.rlib", b"", sparse_len=65536)
for i in range(3):
    run(f"index_{a.scale}_{i}", env, ["cache", "dedupe", "--dry-run", "--json"])

# Direct Linux ioctl returns both syscall and per-file status.
for location in [root / "fs-probe", pathlib.Path("/tmp/buildq-qa-dedupe-probe")]:
    location.mkdir(parents=True, exist_ok=True)
    src = location / "a"
    dst = location / "b"
    src.write_bytes(b"x" * 65536)
    dst.write_bytes(b"x" * 65536)
    with src.open("rb") as s, dst.open("rb") as d:
        b = bytearray(
            struct.pack("QQHHIqQQiI", 0, 65536, 1, 0, 0, d.fileno(), 0, 0, 0, 0)
        )
        try:
            fcntl.ioctl(s.fileno(), 0xC0189436, b, True)
            result = {
                "syscall": "ok",
                "bytes_deduped": struct.unpack("QQHHIqQQiI", b)[7],
                "status": struct.unpack("QQHHIqQQiI", b)[8],
            }
        except OSError as error:
            result = {"errno": error.errno, "error": str(error)}
        results.append(
            {
                "scenario": "filesystem_probe",
                "location": str(location),
                "result": result,
            }
        )
    src.unlink()
    dst.unlink()
print(json.dumps({"buildq": a.buildq, "scale": a.scale, "results": results}, indent=2))
