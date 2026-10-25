#!/usr/bin/env python3
"""Runtime GC QA with real Cargo builds and isolated, disposable fixtures.

Requires buildq and cargo on PATH. Uses no network and never edits an upstream
repository. --root must not already exist. Retain results.json and commands.jsonl
as evidence; the generated caches can be removed after inspection.
Owned Popen groups are stopped on normal exit, exceptions, SIGINT and SIGTERM.
SIGKILL of the Python harness cannot run cleanup.
"""

import argparse
import atexit
import hashlib
import json
import os
import shutil
import signal
import statistics
import subprocess
import time
from pathlib import Path

P = argparse.ArgumentParser()
P.add_argument("--root", type=Path, required=True)
P.add_argument("--out", type=Path, required=True)
P.add_argument("--cycles", type=int, default=240)
P.add_argument("--repos", type=int, default=24)
P.add_argument("--buildq", default=shutil.which("buildq"))
P.add_argument("--cargo", default=shutil.which("cargo"))
A = P.parse_args()
A.root = A.root.resolve()
A.out = A.out.resolve()
A.root.mkdir(parents=True, exist_ok=False)
A.out.mkdir(parents=True, exist_ok=True)
# Track only processes started by this script; never scan/kill other QA jobs.
owned_children = []


def cleanup_children():
    for child in owned_children:
        if child.poll() is None:
            try:
                # Each Popen owns a fresh session, so this group cannot include
                # unrelated jobs. SIGKILL avoids the SIGTERM tree leak (#111).
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
    deadline = time.monotonic() + 3
    for child in owned_children:
        try:
            child.wait(timeout=max(0, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
    for child in owned_children:
        try:
            child.wait(timeout=1)
        except subprocess.TimeoutExpired:
            pass


def interrupted(signum, _frame):
    raise SystemExit(128 + signum)


atexit.register(cleanup_children)
signal.signal(signal.SIGTERM, interrupted)

env = dict(os.environ)
for k in (
    "CARGO_TARGET_DIR",
    "BUILDQ_HELD",
    "BUILDQ_BUDGET",
    "BUILDQ_GC",
    "BUILDQ_POOL",
    "BUILDQ_CONFIG",
):
    env.pop(k, None)
env.update(
    BUILDQ_DIR=str(A.root / "queue"),
    BUILDQ_CACHE=str(A.root / "cache"),
    BUILDQ_REAL_CARGO=A.cargo,
    CARGO_BUILD_JOBS="1",
    RUSTUP_TOOLCHAIN="1.89.0",
)
queue, cache = A.root / "queue", A.root / "cache"
queue.mkdir()
results = {
    "buildq_head": subprocess.check_output(
        ["git", "rev-parse", "HEAD"], text=True
    ).strip(),
    "cargo": subprocess.check_output(
        [A.cargo, "--version"], env=env, text=True
    ).strip(),
    "rustc": subprocess.check_output(
        ["rustc", "--version"], env=env, text=True
    ).strip(),
    "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    "root": str(A.root),
}


def configure(budget="32MiB", gc=True, min_free="0", idle_days=21):
    (queue / "config.toml").write_text(f'''slots=2
max_per_worktree=0
poll_ms=10
[cache]
budget="{budget}"
min_free="{min_free}"
pool_idle_days={idle_days}
gc_after_job={str(gc).lower()}
gc_mode="inline"
dedupe="off"
[cargo]
jobs=1
mirror_bins=false
''')


def call(args, cwd=None, extra=None, check=True):
    start = time.monotonic()
    done = subprocess.run(
        [A.buildq] + args,
        cwd=cwd or A.root,
        env=env | (extra or {}),
        capture_output=True,
        text=True,
        timeout=90,
    )
    duration = time.monotonic() - start
    record = {
        "args": args,
        "cwd": str(cwd or A.root),
        "seconds": round(duration, 6),
        "exit": done.returncode,
        "stdout": done.stdout,
        "stderr": done.stderr,
    }
    with (A.out / "commands.jsonl").open("a") as f:
        f.write(json.dumps(record) + "\n")
    if check and done.returncode:
        raise RuntimeError(record)
    return done, duration


def usage(refresh=True):
    return json.loads(
        call(["cache", "ls", "--json"] + (["--refresh"] if refresh else []))[0].stdout
    )


def collect(*args):
    return json.loads(call(["gc", "--json", *args])[0].stdout)


def clean():
    call(["cache", "clean", "--all", "--json"])


def fixture(name):
    repo = A.root / name
    (repo / "src").mkdir(parents=True)
    (repo / "Cargo.toml").write_text(f"""[package]
name="qa-{name}"
version="0.1.0"
edition="2021"
""")
    (repo / "src/main.rs").write_text('fn main() { println!("qa-ok"); }\n')
    (repo / "build.rs").write_text(r"""
use std::{env,fs,io::Write,path::Path,thread,time::Duration};
fn main() {
    println!("cargo:rerun-if-env-changed=QA_BYTES");
    println!("cargo:rerun-if-env-changed=QA_GATE");
    let n: usize=env::var("QA_BYTES").unwrap_or("0".into()).parse().unwrap();
    let path=Path::new(&env::var("OUT_DIR").unwrap()).join("payload");
    let mut f=fs::File::create(path).unwrap();
    let chunk=vec![13u8; 1024*1024];
    for i in (0..n).step_by(chunk.len()) {
        f.write_all(&chunk[..chunk.len().min(n-i)]).unwrap();
    }
    f.sync_all().unwrap();
    if let Ok(ready)=env::var("QA_READY") { fs::write(ready,"ready").unwrap(); }
    if let Ok(gate)=env::var("QA_GATE") {
        while Path::new(&gate).exists() { thread::sleep(Duration::from_millis(10)); }
    }
}
""")
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
            "local QA fixture",
        ],
    ]:
        subprocess.run(["git"] + args, cwd=repo, check=True, capture_output=True)
    return repo


def build(repo, size=0, extra=None):
    return call(
        ["cargo", "build", "--offline"],
        cwd=repo,
        extra={"QA_BYTES": str(size)} | (extra or {}),
    )


def payloads():
    return list(cache.glob("pools/*/m*/debug/build/*/out/payload"))


def footprint():
    files = [p for p in cache.rglob("*") if p.is_file()]
    return {
        "files": len(files),
        "allocated_bytes": sum(p.stat().st_blocks * 512 for p in files),
        "logical_bytes": sum(p.stat().st_size for p in files),
        "pools": len(list((cache / "pools").glob("*"))),
        "pool_locks": len(list(cache.glob("pools/*/*.lock"))),
    }


configure()
repo = fixture("budget")
# One build cannot fit the budget. Inline GC must explain and safely evict it.
oversize = []
for _ in range(2):
    done, seconds = build(repo, 64 * 1024 * 1024)
    state = usage()
    oversize.append(
        {
            "seconds": seconds,
            "warning": "exceeds the entire cache budget" in done.stderr,
            "after": state["total"],
            "members": sum(len(p["members"]) for p in state["pools"]),
        }
    )
assert all(x["warning"] and x["after"] == 0 for x in oversize), oversize
results["oversize_single_build"] = oversize

# Growing again immediately after a small build tests the 60-second estimate.
configure()
build(repo, 1 * 1024 * 1024)
done, seconds = build(repo, 64 * 1024 * 1024)
cheap = usage(False)
actual = usage(True)
retained = bool(payloads())
results["warm_growth_within_size_ttl"] = {
    "seconds": seconds,
    "oversize_warning": "exceeds the entire cache budget" in done.stderr,
    "cached_total": cheap["total"],
    "refreshed_total": actual["total"],
    "payload_retained": retained,
    "manual_gc": collect(),
}

# Busy artifacts survive manual GC. A killed buildq must not leave a held lease.
configure(gc=False)
gate, ready = A.root / "gate", A.root / "ready"
gate.touch()
live_env = env | {
    "QA_BYTES": str(8 * 1024 * 1024),
    "QA_GATE": str(gate),
    "QA_READY": str(ready),
}
live_log = (A.out / "live-build.log").open("w")
live = subprocess.Popen(
    [A.buildq, "cargo", "build", "--offline"],
    cwd=repo,
    env=live_env,
    stdout=live_log,
    stderr=live_log,
    start_new_session=True,
)
owned_children.append(live)
deadline = time.monotonic() + 30
while not ready.exists() and time.monotonic() < deadline:
    if live.poll() is not None:
        raise RuntimeError("live build exited before gate")
    time.sleep(0.01)
assert ready.exists()
before_hashes = {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in payloads()}
busy_gc = collect("--budget", "1MiB")
after_hashes = {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in payloads()}
assert before_hashes == after_hashes and busy_gc["over_budget"]
live.kill()
live.wait(timeout=10)
gate.unlink()
live_log.close()
deadline = time.monotonic() + 10
while time.monotonic() < deadline:
    u = usage()
    if not any(m["busy"] for p in u["pools"] for m in p["members"]):
        break
    time.sleep(0.05)
else:
    raise RuntimeError("lease remained held after crashed buildq")
post_crash_gc = collect("--budget", "1MiB")
assert not post_crash_gc["over_budget"]
results["busy_gc_and_crash_recovery"] = {
    "busy_gc": busy_gc,
    "payload_sha256_preserved": before_hashes == after_hashes,
    "recovery_gc": post_crash_gc,
}

# Real compilation under impossible reserve must fail before starting Cargo.
configure(min_free=str(shutil.disk_usage(cache).free + 1024 * 1024))
done, seconds = (
    build(repo, 0)
    if False
    else call(["cargo", "build", "--offline"], cwd=repo, check=False)
)
assert done.returncode != 0 and "pooled build refused" in done.stderr
results["min_free_admission"] = {
    "exit": done.returncode,
    "seconds": seconds,
    "stderr": done.stderr,
}

# Dozens of independent repos repeatedly replace cold caches under one budget.
configure(budget="64MiB")
repos = [fixture(f"switch-{n:02}") for n in range(A.repos)]
churn = []
for n in range(A.cycles):
    _, seconds = build(repos[n % len(repos)], 8 * 1024 * 1024)
    u = usage()
    assert not u["over_budget"], (n, u)
    churn.append(
        {
            "cycle": n,
            "repo": n % len(repos),
            "build_seconds": round(seconds, 6),
            "pool_bytes": u["total"],
            "members": sum(len(p["members"]) for p in u["pools"]),
            "files": footprint()["files"] if n % 24 == 0 else None,
        }
    )
    if n % 24 == 23:
        print(f"{n + 1}/{A.cycles} cycles complete", flush=True)
with (A.out / "churn.json").open("w") as f:
    json.dump(churn, f, indent=2)
results["repo_switch_churn"] = {
    "cycles": len(churn),
    "repos": len(repos),
    "median_build_seconds": statistics.median(x["build_seconds"] for x in churn),
    "max_pool_bytes": max(x["pool_bytes"] for x in churn),
    "after": footprint(),
    "gc": collect(),
}

# Simulate 30 idle days by aging metadata, not by claiming a month-long soak.
old = int((time.time() - 30 * 86400) * 1000)
for meta_path in cache.glob("pools/*/*.json"):
    meta = json.loads(meta_path.read_text())
    for key in ("last_used_ms", "created_ms"):
        if key in meta:
            meta[key] = old
    meta_path.write_text(json.dumps(meta))
aged_gc = collect()
assert usage()["total"] == 0
results["simulated_30_idle_days"] = {
    "gc": aged_gc,
    "after": footprint(),
    "note": "Metadata timestamps aged; not a real multi-day soak.",
}
results["ended_utc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
(A.out / "results.json").write_text(json.dumps(results, indent=2) + "\n")
print(json.dumps(results, indent=2))
