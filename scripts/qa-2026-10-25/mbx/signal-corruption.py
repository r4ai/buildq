#!/usr/bin/env python3
"""Show whether cancelled build.rs can overwrite an already reused pool member.
Only isolated fixture files are written. Requires BUILDQ_BIN/MBX_BIN and Rust.
"""

import argparse
import json
import os
import pathlib
import time

from qa_support import fresh_paths
from qa_support import popen as qa_popen
from qa_support import run as qa_run

p = argparse.ArgumentParser()
p.add_argument("--root", type=pathlib.Path, required=True)
p.add_argument("--output", type=pathlib.Path, required=True)
a = p.parse_args()
root, out = fresh_paths(a.root, a.output)
bq = os.environ["BUILDQ_BIN"]
mbx = os.environ["MBX_BIN"]
rows = []
for backend in ["cargo", "mbx"]:
    r = root / backend
    repo = r / "repo"
    (repo / "src").mkdir(parents=True, exist_ok=True)
    (repo / "Cargo.toml").write_text(
        '[package]\nname="qa-cancelled-output"\nversion="0.1.0"\nedition="2024"\n'
    )
    (repo / "src/main.rs").write_text(
        "fn main() {\n"
        '    println!("{}", include_str!(concat!(env!("OUT_DIR"), "/value.txt")))\n'
        "}\n"
    )
    (
        repo / "build.rs"
    ).write_text("""use std::{env,fs,path::PathBuf,time::Duration,thread};
fn main(){
for v in ["QA_BUILD_VALUE", "QA_DELAY", "QA_MARKER"] {
    println!("cargo:rerun-if-env-changed={v}");
}
let out=PathBuf::from(env::var_os("OUT_DIR").unwrap());
let marker=env::var("QA_MARKER").unwrap();
fs::write(&marker,format!("{}\\n{}",std::process::id(),out.display())).unwrap();
thread::sleep(Duration::from_secs(env::var("QA_DELAY").unwrap().parse().unwrap()));
fs::write(out.join("value.txt"),env::var("QA_BUILD_VALUE").unwrap()).unwrap();
fs::write(format!("{marker}.done"),"done").unwrap();
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
    q.mkdir(exist_ok=True)
    (q / "config.toml").write_text(f"""slots=1
poll_ms=10
[cache]
dir={json.dumps(str(r / "pool"))}
min_free="0"
budget="1G"
gc_after_job=false
max_members=1
[cargo]
backend={json.dumps(backend)}
mbx={json.dumps(mbx)}
""")
    env = {
        **os.environ,
        "BUILDQ_DIR": str(q),
        "BUILDQ_CACHE": str(r / "pool"),
        "MBX_CACHE_DIR": str(r / "mbx"),
        "MBX_DISPLAY": "plain",
        "MBX_SUMMARY": "full",
        "MBX_GC_AUTO": "0",
        "CARGO_BUILD_JOBS": "1",
    }
    for k in ["BUILDQ_HELD", "CARGO_TARGET_DIR", "RUSTC_WRAPPER"]:
        env.pop(k, None)
    old = r / "old-marker"
    new = r / "new-marker"
    oldenv = {
        **env,
        "QA_MARKER": str(old),
        "QA_DELAY": "5",
        "QA_BUILD_VALUE": "cancelled-old",
    }
    newenv = {
        **env,
        "QA_MARKER": str(new),
        "QA_DELAY": "0",
        "QA_BUILD_VALUE": "live-new",
    }
    f = (out / (backend + "-old.log")).open("w")
    child = qa_popen(
        [bq, "cargo", "build", "--offline"],
        cwd=repo,
        env=oldenv,
        stdout=f,
        stderr=f,
        start_new_session=True,
    )
    deadline = time.monotonic() + 15
    while not old.exists() and time.monotonic() < deadline and child.poll() is None:
        time.sleep(0.05)
    if not old.exists():
        raise RuntimeError("old build did not reach build script")
    oldpid, oldout = old.read_text().split("\n")
    child.terminate()
    oldcode = child.wait(timeout=8)
    before = time.monotonic()
    second = qa_run(
        [bq, "cargo", "run", "--offline"],
        cwd=repo,
        env=newenv,
        capture_output=True,
        text=True,
        timeout=12,
    )
    secondseconds = time.monotonic() - before
    (out / (backend + "-new.log")).write_text(second.stdout + second.stderr)
    newpid, newout = new.read_text().split("\n")
    deadline = time.monotonic() + 7
    while not pathlib.Path(str(old) + ".done").exists() and time.monotonic() < deadline:
        time.sleep(0.05)
    overwritten = (pathlib.Path(newout) / "value.txt").read_text()
    third = qa_run(
        [bq, "cargo", "run", "--offline"],
        cwd=repo,
        env=newenv,
        capture_output=True,
        text=True,
        timeout=12,
    )
    (out / (backend + "-after-old-finished.log")).write_text(
        third.stdout + third.stderr
    )
    row = {
        "backend": backend,
        "cancelled_exit": oldcode,
        "old_pid": int(oldpid),
        "new_pid": int(newpid),
        "old_out": oldout,
        "new_out": newout,
        "same_out_dir": oldout == newout,
        "second_exit": second.returncode,
        "second_stdout": second.stdout.strip(),
        "second_seconds": round(secondseconds, 6),
        "old_finished_after_cancellation": pathlib.Path(str(old) + ".done").exists(),
        "generated_file_after_old_finished": overwritten,
        "third_exit": third.returncode,
        "third_stdout": third.stdout.strip(),
        "passed": second.returncode == 0
        and second.stdout.strip() == "live-new"
        and third.returncode == 0
        and third.stdout.strip() == "live-new",
    }
    rows.append(row)
    print(json.dumps(row), flush=True)
    f.close()
(out / "results.json").write_text(json.dumps(rows, indent=2) + "\n")
