#!/usr/bin/env python3
"""Concurrent real-mbx worktrees with proc macros, generated Rust, native C,
feature/compile-time environment changes and timestamp-preserving source edits.
Requires BUILDQ_BIN and MBX_BIN; native cc/ar; Rust on PATH. No dependencies.
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
repo.mkdir(exist_ok=True)
files = {
    "Cargo.toml": '[workspace]\nresolver="3"\nmembers=["app","lib","macros"]\n',
    "lib/Cargo.toml": (
        '[package]\nname="qa-complex-lib"\nversion="0.1.0"\nedition="2024"\n'
        "[features]\nalpha=[]\nbeta=[]\n[dependencies]\n"
        'qa-complex-macros={path="../macros"}\n'
    ),
    "lib/src/lib.rs": """include!(concat!(env!("OUT_DIR"),"/generated.rs"));
#[cfg(feature="alpha")] pub const FEATURE: &str="alpha";
#[cfg(all(feature="beta",not(feature="alpha")))] pub const FEATURE: &str="beta";
#[cfg(not(any(feature="alpha",feature="beta")))] pub const FEATURE: &str="default";
pub const SOURCE: &str="source one";
pub const ENV: &str=env!("QA_RUST_VALUE");
unsafe extern "C" { fn answer() -> i32; }
pub fn value() -> String {
    format!(
        "{}:{}:{}:{}:{}:{}",
        FEATURE, GENERATED, SOURCE, ENV, qa_complex_macros::word!(),
        unsafe { answer() }
    )
}
""",
    "lib/build.rs": """use std::{env,fs,path::PathBuf,process::Command};
fn main(){
println!("cargo:rerun-if-env-changed=QA_BUILD_VALUE");
println!("cargo:rerun-if-changed=native.c");
let out=PathBuf::from(env::var_os("OUT_DIR").unwrap());
let value=env::var("QA_BUILD_VALUE").unwrap();
fs::write(
    out.join("generated.rs"),
    format!("pub const GENERATED: &str={:?};", value)
).unwrap();
assert!(Command::new("cc")
    .args(["-c", "native.c", "-o"])
    .arg(out.join("native.o")).status().unwrap().success());
assert!(Command::new("ar")
    .arg("crs").arg(out.join("libqa_native.a"))
    .arg(out.join("native.o")).status().unwrap().success());
println!("cargo:rustc-link-search=native={}",out.display());
println!("cargo:rustc-link-lib=static=qa_native");
}
""",
    "lib/native.c": "int answer(void){return 42;}\n",
    "macros/Cargo.toml": (
        '[package]\nname="qa-complex-macros"\nversion="0.1.0"\nedition="2024"\n'
        "[lib]\nproc-macro=true\n"
    ),
    "macros/src/lib.rs": (
        "extern crate proc_macro;\n"
        "#[proc_macro]\n"
        "pub fn word(_: proc_macro::TokenStream) -> proc_macro::TokenStream {\n"
        '    "\\"macro one\\"".parse().unwrap()\n'
        "}\n"
    ),
    "app/Cargo.toml": (
        '[package]\nname="qa-complex-app"\nversion="0.1.0"\nedition="2024"\n'
        '[dependencies]\nqa-complex-lib={path="../lib"}\n[features]\n'
        'alpha=["qa-complex-lib/alpha"]\nbeta=["qa-complex-lib/beta"]\n'
    ),
    "app/src/main.rs": 'fn main(){println!("{}",qa_complex_lib::value())}\n',
    ".gitignore": "/target\n/Cargo.lock\n",
}
for name, s in files.items():
    f = repo / name
    f.parent.mkdir(parents=True, exist_ok=True)
    f.write_text(s)


def git(*args):
    return qa_run(
        ["git", "-C", str(repo), *args], check=True, capture_output=True, text=True
    )


git("init", "-q")
git("add", ".")
git(
    "-c",
    "user.name=QA",
    "-c",
    "user.email=qa@example.invalid",
    "commit",
    "-qm",
    "complex fixture",
)
worktrees = []
for i in range(6):
    wt = root / f"wt{i}"
    git("worktree", "add", "--detach", str(wt), "HEAD")
    worktrees.append(wt)
env = {
    **os.environ,
    "MBX_CACHE_DIR": str(root / "mbx-cache"),
    "MBX_DISPLAY": "plain",
    "MBX_SUMMARY": "full",
    "MBX_GC_AUTO": "0",
    "CARGO_BUILD_JOBS": "2",
}
for k in ["CARGO_TARGET_DIR", "BUILDQ_HELD", "RUSTC_WRAPPER"]:
    env.pop(k, None)
for backend in ["cargo", "mbx"]:
    q = root / (backend + "-queue")
    q.mkdir(exist_ok=True)
    (q / "config.toml").write_text(f"""slots=2
poll_ms=10
[cache]
dir={json.dumps(str(root / (backend + "-pool")))}
budget="2G"
min_free="0"
gc_after_job=false
max_members=2
[cargo]
backend={json.dumps(backend)}
mbx={json.dumps(mbx)}
""")
rows = []


def run(backend, phase, i):
    wt = worktrees[i]
    feature = ["alpha", "beta", ""][i % 3]
    build_value = f"build{i}" if phase < 2 else f"changed{i}"
    rust_value = f"rust{i}" if phase < 2 else f"edited{i}"
    command = [bq, "cargo", "run", "--offline", "-p", "qa-complex-app"]
    if feature:
        command += ["--features", feature]
    e = {
        **env,
        "BUILDQ_DIR": str(root / (backend + "-queue")),
        "BUILDQ_CACHE": str(root / (backend + "-pool")),
        "QA_BUILD_VALUE": build_value,
        "QA_RUST_VALUE": rust_value,
    }
    start = time.monotonic()
    r = qa_run(command, cwd=wt, env=e, capture_output=True, text=True, timeout=120)
    expect = (
        f"{feature or 'default'}:{build_value}:"
        f"source {'two' if phase == 3 else 'one'}:{rust_value}:"
        f"macro {'two' if phase == 3 and i % 2 else 'one'}:42"
    )
    good = r.returncode == 0 and r.stdout.strip() == expect
    name = f"{backend}-phase{phase}-wt{i}"
    (out / (name + ".log")).write_text(r.stdout + r.stderr)
    row = {
        "name": name,
        "backend": backend,
        "phase": phase,
        "worktree": i,
        "expected": expect,
        "actual": r.stdout.strip(),
        "exit": r.returncode,
        "passed": good,
        "seconds": round(time.monotonic() - start, 6),
    }
    print(json.dumps(row), flush=True)
    return row


for phase in range(4):
    if phase == 3:
        for i, wt in enumerate(worktrees):
            for rel, old, new in [("lib/src/lib.rs", "source one", "source two")] + (
                [("macros/src/lib.rs", "macro one", "macro two")] if i % 2 else []
            ):
                f = wt / rel
                st = f.stat()
                f.write_text(f.read_text().replace(old, new))
                os.utime(f, ns=(st.st_atime_ns, st.st_mtime_ns))
    for backend in ["cargo", "mbx"]:
        with concurrent.futures.ThreadPoolExecutor(max_workers=6) as ex:
            rows += list(
                ex.map(
                    lambda i, backend=backend, phase=phase: run(backend, phase, i),
                    range(6),
                )
            )
        e = {
            **env,
            "BUILDQ_DIR": str(root / (backend + "-queue")),
            "BUILDQ_CACHE": str(root / (backend + "-pool")),
        }
        for label, cmd in [
            ("history", [bq, "history", "--json", "-n", "100"]),
            ("cache", [bq, "cache", "ls", "--json", "--refresh"]),
        ]:
            r = qa_run(cmd, cwd=repo, env=e, capture_output=True, text=True)
            (out / f"{backend}-phase{phase}-{label}.json").write_text(r.stdout)
for label, args in [
    ("mbx-stats", ["stats", "--json"]),
    ("mbx-cache", ["cache", "stats", "--json"]),
]:
    r = qa_run([mbx, *args], cwd=repo, env=env, capture_output=True, text=True)
    (out / (label + ".json")).write_text(r.stdout)
(out / "results.json").write_text(json.dumps(rows, indent=2) + "\n")
print(f"{sum(r['passed'] for r in rows)}/{len(rows)} passed")
