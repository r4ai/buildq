#!/usr/bin/env python3
"""Check parent death during a real Cargo build, including real mbx descendants.
On Linux only. The build script and its sleep child record PIDs in QA_MARKER.
"""

import argparse
import json
import os
import pathlib
import signal
import subprocess
import time

from qa_support import fresh_paths, stop_owned
from qa_support import popen as qa_popen
from qa_support import run as qa_run

p = argparse.ArgumentParser()
p.add_argument("--root", type=pathlib.Path, required=True)
p.add_argument("--output", type=pathlib.Path, required=True)
p.add_argument("--repetitions", type=int, default=2)
p.add_argument("--min-free", default="0")
p.add_argument(
    "--signals",
    nargs="+",
    choices=["SIGTERM", "SIGKILL"],
    default=["SIGTERM", "SIGKILL"],
)
a = p.parse_args()
root, out = fresh_paths(a.root, a.output)
bq = os.environ["BUILDQ_BIN"]
mbx = os.environ["MBX_BIN"]
rows = []


def alive(pid):
    try:
        return (
            pathlib.Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0]
            != "Z"
        )
    except FileNotFoundError:
        return False


def identity(pid):
    try:
        stat = pathlib.Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return {
            "pid": pid,
            "ppid": int(stat[1]),
            "state": stat[0],
            "command": pathlib.Path(f"/proc/{pid}/cmdline")
            .read_bytes()
            .replace(b"\0", b" ")
            .decode(errors="replace")
            .strip(),
        }
    except FileNotFoundError:
        return {"pid": pid, "exited": True}


for backend, repeat in [(b, i) for b in ["cargo", "mbx"] for i in range(a.repetitions)]:
    for sig in [getattr(signal, name) for name in a.signals]:
        name = f"{backend}-{sig.name}-repeat{repeat}"
        r = root / name
        repo = r / "repo"
        (repo / "src").mkdir(parents=True, exist_ok=True)
        (repo / "Cargo.toml").write_text(
            '[package]\nname="qa-lifecycle"\nversion="0.1.0"\nedition="2024"\n'
        )
        (repo / "src/lib.rs").write_text("pub fn value()->i32 {42}\n")
        (repo / "build.rs").write_text("""use std::{env,fs,process::Command};
fn main(){println!("cargo:rerun-if-env-changed=QA_MARKER");
let mut child=Command::new("sleep").arg("45").spawn().unwrap();
fs::write(
    env::var("QA_MARKER").unwrap(),
    format!("{} {}", std::process::id(), child.id())
).unwrap();
child.wait().unwrap();}
""")
        qa_run(["git", "init", "-q", str(repo)], check=True)
        qa_run(["git", "-C", str(repo), "add", "."], check=True)
        qa_run(
            [
                "git",
                "-C",
                str(repo),
                "-c",
                "user.name=QA",
                "-c",
                "user.email=qa@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
            check=True,
        )
        q = r / "q"
        q.mkdir(exist_ok=True)
        (q / "config.toml").write_text(f"""slots=1
poll_ms=10
[cache]
dir={json.dumps(str(r / "pool"))}
min_free={json.dumps(a.min_free)}
budget="1G"
gc_after_job=false
[cargo]
backend={json.dumps(backend)}
mbx={json.dumps(mbx)}
""")
        marker = r / "marker"
        env = {
            **os.environ,
            "BUILDQ_DIR": str(q),
            "BUILDQ_CACHE": str(r / "pool"),
            "MBX_CACHE_DIR": str(r / "mbx"),
            "MBX_DISPLAY": "plain",
            "MBX_SUMMARY": "full",
            "MBX_GC_AUTO": "0",
            "QA_MARKER": str(marker),
            "CARGO_BUILD_JOBS": "1",
        }
        for k in ["BUILDQ_HELD", "CARGO_TARGET_DIR", "RUSTC_WRAPPER"]:
            env.pop(k, None)
        log = out / (name + ".log")
        f = log.open("w")
        child = qa_popen(
            [bq, "cargo", "build", "--offline"],
            cwd=repo,
            env=env,
            stdout=f,
            stderr=f,
            start_new_session=True,
        )
        deadline = time.monotonic() + 20
        while (
            not marker.exists() and time.monotonic() < deadline and child.poll() is None
        ):
            time.sleep(0.05)
        if not marker.exists():
            stop_owned(child)
            f.close()
            rows.append(
                {"name": name, "passed": False, "reason": "build script did not start"}
            )
            continue
        pids = list(map(int, marker.read_text().split()))
        before = [identity(pid) for pid in pids]
        start = time.monotonic()
        child.send_signal(sig)
        try:
            code = child.wait(timeout=8)
        except subprocess.TimeoutExpired:
            child.kill()
            code = child.wait()
        deadline = time.monotonic() + 8
        while any(alive(pid) for pid in pids) and time.monotonic() < deadline:
            time.sleep(0.05)
        living = [pid for pid in pids if alive(pid)]
        status = qa_run(
            [bq, "status", "--json"], cwd=repo, env=env, capture_output=True, text=True
        )
        (out / (name + "-status.json")).write_text(status.stdout)
        s = json.loads(status.stdout)
        busy = [h for c in s["classes"] for h in c["holders"] if h["busy"]]
        deadline = time.monotonic() + 8
        while busy and time.monotonic() < deadline:
            time.sleep(0.05)
            status = qa_run(
                [bq, "status", "--json"],
                cwd=repo,
                env=env,
                capture_output=True,
                text=True,
            )
            s = json.loads(status.stdout)
            busy = [h for c in s["classes"] for h in c["holders"] if h["busy"]]
        (out / (name + "-status.json")).write_text(status.stdout)
        row = {
            "name": name,
            "signal": sig.name,
            "min_free": a.min_free,
            "parent_exit": code,
            "recorded_descendants": pids,
            "before_signal": before,
            "after_signal": [identity(pid) for pid in pids],
            "living_descendants": living,
            "busy_holders": len(busy),
            "seconds_after_signal": round(time.monotonic() - start, 6),
            "passed": not living and not busy,
        }
        rows.append(row)
        print(json.dumps(row), flush=True)
        # Cleanup after observations includes reparented processes in our session.
        stop_owned(child)
        f.close()
(out / "results.json").write_text(json.dumps(rows, indent=2) + "\n")
