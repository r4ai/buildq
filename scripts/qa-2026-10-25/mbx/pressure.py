#!/usr/bin/env python3
"""Cross a deliberately high free-space reserve without filling a volume.
Use an executable tmpfs for --root. Each backend writes at most 64 MiB.
"""

import argparse
import json
import os
import pathlib
import re
import time

from qa_support import fresh_paths
from qa_support import run as qa_run

p = argparse.ArgumentParser()
p.add_argument("--root", type=pathlib.Path, required=True)
p.add_argument("--output", type=pathlib.Path, required=True)
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


def free_bytes():
    info = os.statvfs(root)
    return info.f_bavail * info.f_frsize


for backend in ["cargo", "mbx"]:
    r = root / backend
    repo = r / "repo"
    (repo / "src").mkdir(parents=True)
    (repo / "Cargo.toml").write_text(
        '[package]\nname="qa-pressure"\nversion="0.1.0"\nedition="2024"\n'
    )
    (repo / "src/lib.rs").write_text("pub fn value() -> i32 {42}\n")
    (repo / "build.rs").write_text("""use std::{
    env, fs, io::Write, path::PathBuf, process::Command, thread, time::Duration
};
fn main(){
println!("cargo:rerun-if-env-changed=QA_WRITE");
println!("cargo:rerun-if-env-changed=QA_MARKER");
if env::var("QA_WRITE").unwrap()!="1" {return;}
let mut child=Command::new("sleep").arg("30").spawn().unwrap();
let out=PathBuf::from(env::var_os("OUT_DIR").unwrap());
fs::write(
    env::var("QA_MARKER").unwrap(),
    format!("{} {}", std::process::id(), child.id())
).unwrap();
let mut file=fs::File::create(out.join("pressure.bin")).unwrap();
let bytes=vec![42u8;1024*1024];
for _ in 0..64 {
    file.write_all(&bytes).unwrap();
    thread::sleep(Duration::from_millis(50));
}
child.wait().unwrap();
}
""")
    for args in [
        ["init", "-q", str(repo)],
        ["-C", str(repo), "add", "."],
        [
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
    ]:
        qa_run(["git", *args], check=True)
    q = r / "q"
    q.mkdir()

    def configure(reserve, *, q=q, r=r, backend=backend):
        (q / "config.toml").write_text(f'''slots=1
poll_ms=10
[cache]
dir={json.dumps(str(r / "pool"))}
min_free="{reserve}"
budget="1G"
gc_after_job=false
max_members=1
[cargo]
backend={json.dumps(backend)}
mbx={json.dumps(mbx)}
''')

    env = {
        **os.environ,
        "BUILDQ_DIR": str(q),
        "BUILDQ_CACHE": str(r / "pool"),
        "MBX_CACHE_DIR": str(r / "mbx"),
        "MBX_DISPLAY": "plain",
        "MBX_SUMMARY": "full",
        "MBX_GC_AUTO": "0",
        "CARGO_BUILD_JOBS": "1",
        "QA_WRITE": "0",
        "QA_MARKER": str(r / "warm-marker"),
    }
    for key in ["BUILDQ_HELD", "CARGO_TARGET_DIR", "RUSTC_WRAPPER"]:
        env.pop(key, None)
    configure(0)
    warm = qa_run(
        [bq, "cargo", "build", "--offline"],
        cwd=repo,
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
    )
    (out / f"{backend}-warm.log").write_text(warm.stdout + warm.stderr)
    if warm.returncode != 0:
        raise RuntimeError(f"{backend}: warm-up failed")
    free_before = free_bytes()
    reserve = free_before - 24 * 1024 * 1024
    if reserve <= 0:
        raise RuntimeError("at least 24 MiB free is needed for this probe")
    configure(reserve)
    marker = r / "pressure-marker"
    started = time.monotonic()
    result = qa_run(
        [bq, "cargo", "build", "--offline"],
        cwd=repo,
        env={**env, "QA_WRITE": "1", "QA_MARKER": str(marker)},
        capture_output=True,
        text=True,
        timeout=20,
    )
    elapsed = time.monotonic() - started
    (out / f"{backend}-pressure.log").write_text(result.stdout + result.stderr)
    if not marker.exists():
        raise RuntimeError(f"{backend}: pressure occurred before build script started")
    pids = [int(value) for value in marker.read_text().split()]
    deadline = time.monotonic() + 3
    while any(alive(pid) for pid in pids) and time.monotonic() < deadline:
        time.sleep(0.05)
    living = [pid for pid in pids if alive(pid)]
    status = qa_run(
        [bq, "status", "--json"], cwd=repo, env=env, capture_output=True, text=True
    )
    (out / f"{backend}-status.json").write_text(status.stdout)
    busy = [
        holder
        for cls in json.loads(status.stdout)["classes"]
        for holder in cls["holders"]
        if holder["busy"]
    ]
    pressure_files = list((r / "pool").rglob("pressure.bin"))
    written = sum(path.stat().st_size for path in pressure_files)
    free_after_abort = free_bytes()
    # Only the named files created by this disposable fixture are removed.
    for path in pressure_files:
        path.unlink()
    recovered = qa_run(
        [bq, "cargo", "build", "--offline"],
        cwd=repo,
        env={**env, "QA_MARKER": str(r / "recovery-marker")},
        capture_output=True,
        text=True,
        timeout=20,
    )
    (out / f"{backend}-recovered.log").write_text(recovered.stdout + recovered.stderr)

    def target(text):
        return re.search(r"buildq: target (\S+)", text).group(1)

    same_member = target(result.stderr) == target(recovered.stderr)
    row = {
        "backend": backend,
        "free_before": free_before,
        "reserve": reserve,
        "free_after_abort": free_after_abort,
        "max_fixture_write_bytes": 64 * 1024 * 1024,
        "actual_write_bytes": written,
        "exit": result.returncode,
        "seconds": round(elapsed, 6),
        "recorded_descendants": pids,
        "living_descendants": living,
        "busy_holders": len(busy),
        "recovery_exit": recovered.returncode,
        "same_member": same_member,
        "passed": result.returncode == 125
        and not living
        and not busy
        and recovered.returncode == 0
        and same_member,
    }
    rows.append(row)
    print(json.dumps(row), flush=True)
(out / "results.json").write_text(json.dumps(rows, indent=2) + "\n")
