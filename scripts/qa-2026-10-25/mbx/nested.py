#!/usr/bin/env python3
"""Exercise unmodified Cargo launched from a build script, with real mbx.
Two isolated nested helpers compile concurrently, using OUT_DIR-scoped targets.
"""

import argparse
import concurrent.futures
import json
import os
import pathlib
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
repo = root / "repo"
(repo / "src").mkdir(parents=True, exist_ok=True)
(repo / "Cargo.toml").write_text(
    '[package]\nname="qa-nested"\nversion="0.1.0"\nedition="2024"\n[workspace]\nexclude=["helper0","helper1"]\n'
)
(repo / "src/main.rs").write_text(
    'include!(concat!(env!("OUT_DIR"),"/value.rs"));fn main(){println!("{}",VALUE)}\n'
)
(
    repo / "build.rs"
).write_text("""use std::{env,fs,path::PathBuf,process::Command,thread};
fn main(){ println!("cargo:rerun-if-env-changed=QA_NESTED_VALUE");
let out=PathBuf::from(env::var_os("OUT_DIR").unwrap());
let children:Vec<_>=(0..2).map(|i| {let out=out.clone();thread::spawn(move|| {
let result = Command::new(env::var_os("CARGO").unwrap())
    .args(["run", "--offline", "--manifest-path"])
    .arg(format!("helper{i}/Cargo.toml"))
    .arg("--target-dir").arg(out.join(format!("helper{i}-target")))
    .output().unwrap();
assert!(
    result.status.success(), "nested failed: {}",
    String::from_utf8_lossy(&result.stderr)
);
String::from_utf8(result.stdout).unwrap().trim().to_owned()
})}).collect();
let value=children.into_iter().map(|h|h.join().unwrap()).collect::<Vec<_>>().join("+");
fs::write(out.join("value.rs"),format!("const VALUE: &str={:?};",value)).unwrap();}
""")
for i in range(2):
    h = repo / f"helper{i}"
    (h / "src").mkdir(parents=True, exist_ok=True)
    (h / "Cargo.toml").write_text(
        f'[package]\nname="qa-helper{i}"\nversion="0.1.0"\nedition="2024"\n[workspace]\n'
    )
    (h / "src/main.rs").write_text(
        f'fn main(){{println!("helper{i}:{{}}",env!("QA_NESTED_VALUE"))}}\n'
    )
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
q = root / "q"
q.mkdir(exist_ok=True)
(q / "config.toml").write_text(f"""slots=2
poll_ms=10
[cache]
dir={json.dumps(str(root / "pool"))}
min_free="0"
budget="2G"
gc_after_job=false
max_members=2
[cargo]
backend="mbx"
mbx={json.dumps(mbx)}
""")
worktrees = []
for i in range(2):
    w = root / f"wt{i}"
    qa_run(
        ["git", "-C", str(repo), "worktree", "add", "--detach", str(w), "HEAD"],
        check=True,
        capture_output=True,
    )
    worktrees.append(w)
env = {
    **os.environ,
    "BUILDQ_DIR": str(q),
    "BUILDQ_CACHE": str(root / "pool"),
    "MBX_CACHE_DIR": str(root / "mbx-cache"),
    "MBX_DISPLAY": "plain",
    "MBX_GC_AUTO": "0",
    "MBX_SUMMARY": "full",
    "CARGO_BUILD_JOBS": "1",
}
for k in ["BUILDQ_HELD", "CARGO_TARGET_DIR", "RUSTC_WRAPPER"]:
    env.pop(k, None)
rows = []


def run(phase, i):
    value = f"phase{phase}wt{i}"
    e = {**env, "QA_NESTED_VALUE": value}
    start = time.monotonic()
    r = qa_run(
        [bq, "cargo", "run", "--offline"],
        cwd=worktrees[i],
        env=e,
        capture_output=True,
        text=True,
        timeout=90,
    )
    expect = f"helper0:{value}+helper1:{value}"
    row = {
        "phase": phase,
        "worktree": i,
        "exit": r.returncode,
        "seconds": round(time.monotonic() - start, 6),
        "expected": expect,
        "actual": r.stdout.strip(),
        "passed": r.returncode == 0 and r.stdout.strip() == expect,
    }
    (out / f"phase{phase}-wt{i}.log").write_text(r.stdout + r.stderr)
    print(json.dumps(row), flush=True)
    return row


for phase in range(3):
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as ex:
        rows += list(ex.map(lambda i, phase=phase: run(phase, i), range(2)))
(out / "results.json").write_text(json.dumps(rows, indent=2) + "\n")
