#!/usr/bin/env python3
"""Real-mbx integration probes. Uses only disposable local Git repositories.
Requires BUILDQ_BIN, MBX_BIN and a working Rust toolchain on PATH.
Run: python3 functional.py --root /tmp/buildq-mbx-qa --output results
"""

import argparse
import json
import os
import pathlib
import signal
import subprocess
import time

from qa_support import fresh_paths
from qa_support import popen as qa_popen
from qa_support import run as qa_run

p = argparse.ArgumentParser()
p.add_argument("--root", type=pathlib.Path, required=True)
p.add_argument("--output", type=pathlib.Path, required=True)
p.add_argument("--toolchain", default=os.environ.get("RUSTUP_TOOLCHAIN", "stable"))
a = p.parse_args()
root, out = fresh_paths(a.root, a.output)
bq = os.environ["BUILDQ_BIN"]
mbx = os.environ["MBX_BIN"]
repo = root / "repo"
repo.mkdir(exist_ok=True)
files = {
    "Cargo.toml": '[workspace]\nresolver="3"\nmembers=["app","lib"]\n',
    "lib/Cargo.toml": '[package]\nname="qa-lib"\nversion="0.1.0"\nedition="2024"\n',
    "lib/src/lib.rs": """pub const VALUE: &str="version one";
/// Example.
/// ```
/// assert_eq!(qa_lib::add(1, 2), 3);
/// ```
pub fn add(a: i32, b: i32) -> i32 { a + b }
#[test]
fn good() { assert_eq!(add(1, 2), 3); }
""",
    "app/Cargo.toml": (
        '[package]\nname="qa-app"\nversion="0.1.0"\nedition="2024"\n'
        '[dependencies]\nqa-lib={path="../lib"}\n'
    ),
    "app/src/main.rs": """fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    println!(
        "{} args={:?} cwd={} env={}",
        qa_lib::VALUE, args,
        std::env::current_dir().unwrap().display(),
        std::env::var("QA_VALUE").unwrap_or_default()
    );
    if args.iter().any(|a| a == "sleep") {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
    if args.iter().any(|a| a == "fail") { std::process::exit(7) }
}
""",
    ".cargo/config.toml": '[alias]\ngc="check --all-targets"\n',
}
for name, contents in files.items():
    f = repo / name
    f.parent.mkdir(parents=True, exist_ok=True)
    f.write_text(contents)


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
    "fixture",
)
q = root / "queue"
q.mkdir(exist_ok=True)
(q / "config.toml").write_text(f"""slots=2
poll_ms=10
[cache]
dir={json.dumps(str(root / "pool"))}
budget="2G"
min_free="0"
gc_after_job=false
max_members=2
[cargo]
backend="mbx"
mbx={json.dumps(mbx)}
""")
env = {
    **os.environ,
    "BUILDQ_DIR": str(q),
    "BUILDQ_CACHE": str(root / "pool"),
    "MBX_CACHE_DIR": str(root / "mbx-cache"),
    "MBX_DISPLAY": "plain",
    "MBX_SUMMARY": "full",
    "MBX_GC_AUTO": "0",
    "QA_VALUE": "qa inherited value",
}
env.pop("CARGO_TARGET_DIR", None)
env.pop("BUILDQ_HELD", None)
env.pop("RUSTC_WRAPPER", None)
rows = []


def probe(name, args, code=0, contains=None, cwd=None):
    start = time.monotonic()
    r = qa_run(
        [bq, "cargo", *args],
        cwd=cwd or repo,
        env=env,
        capture_output=True,
        text=True,
        timeout=90,
    )
    text = r.stdout + r.stderr
    (out / (name + ".log")).write_text(text)
    passed = r.returncode == code and (contains is None or contains in text)
    row = {
        "name": name,
        "args": args,
        "exit": r.returncode,
        "expected_exit": code,
        "passed": passed,
        "seconds": round(time.monotonic() - start, 6),
    }
    rows.append(row)
    print(json.dumps(row), flush=True)
    return r


probe("01-build-cold", ["build", "--offline"])
probe("02-build-warm", ["build", "--offline"])
probe("03-check", ["check", "--workspace", "--all-targets", "--offline"])
probe("04-test", ["test", "--workspace", "--offline"], contains="test result: ok")
probe(
    "05-run-cwd-env-args",
    ["run", "--offline", "-p", "qa-app", "--", "hello world", "--color", "always"],
    contains="env=qa inherited value",
)
probe("06-run-exit", ["run", "--offline", "-p", "qa-app", "--", "fail"], code=7)
probe("07-alias-mbx-command", ["--offline", "gc"])
probe("08-global-options", ["--offline", "--color", "never", "-v", "build"])
probe("09-global-config", ["--config", "profile.dev.opt-level=1", "build", "--offline"])
probe(
    "10-explicit-target", ["build", "--offline", "--target-dir", str(root / "explicit")]
)
probe(
    "11-subdirectory",
    ["run", "--offline", "--", "subdirectory"],
    cwd=repo / "app",
    contains="cwd=" + str(repo / "app"),
)
probe("12-toolchain", ["+" + a.toolchain, "check", "--offline"])
probe(
    "13-json-run",
    ["run", "--offline", "--message-format=json", "-p", "qa-app", "--", "json"],
    contains="version one",
)
f = repo / "lib/src/lib.rs"
stamp = f.stat()
f.write_text(f.read_text().replace("version one", "version two"))
os.utime(f, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
probe(
    "14-preserve-mtime-edit",
    ["run", "--offline", "-p", "qa-app", "--", "edited"],
    contains="version two",
)
saved = f.read_text()
f.write_text(saved + '\ncompile_error!("intentional QA error");\n')
probe(
    "15-build-failure",
    ["build", "--offline"],
    code=101,
    contains="intentional QA error",
)
f.write_text(saved)
probe(
    "16-recover-from-build-failure",
    ["run", "--offline", "-p", "qa-app", "--", "recovered"],
    contains="version two",
)
f.write_text(
    saved
    + '\n#[test] fn intentionally_failing(){panic!("intentional test failure");}\n'
)
probe(
    "17-test-failure",
    ["test", "--offline", "-p", "qa-lib"],
    code=101,
    contains="intentional test failure",
)
f.write_text(saved)
probe(
    "18-recover-from-test-failure",
    ["test", "--offline", "-p", "qa-lib"],
    contains="test result: ok",
)
probe("19-rustdoc", ["doc", "--workspace", "--no-deps", "--offline"])
probe("20-rustc", ["rustc", "--offline", "-p", "qa-lib", "--", "-Dwarnings"])
# A long-running staged app must let the compile slot go, and die on SIGINT.
start = time.monotonic()
child = qa_popen(
    [bq, "cargo", "run", "--offline", "-p", "qa-app", "--", "sleep"],
    cwd=repo,
    env=env,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    text=True,
    start_new_session=True,
)
time.sleep(2)
snapshot = qa_run(
    [bq, "status", "--json"], cwd=repo, env=env, capture_output=True, text=True
)
os.killpg(child.pid, signal.SIGINT)
try:
    stdout, stderr = child.communicate(timeout=12)
except subprocess.TimeoutExpired:
    os.killpg(child.pid, signal.SIGKILL)
    stdout, stderr = child.communicate()
(out / "21-signal.log").write_text(
    stdout + stderr + "\nstatus during run:\n" + snapshot.stdout
)
rows.append(
    {
        "name": "21-signal",
        "exit": child.returncode,
        "passed": child.returncode in (130, -2),
        "seconds": round(time.monotonic() - start, 6),
    }
)
probe("22-after-signal", ["check", "--offline"])
for name, args in [
    ("history", [bq, "history", "--json", "-n", "100"]),
    ("status", [bq, "status", "--json"]),
    ("buildq-cache", [bq, "cache", "ls", "--refresh", "--json"]),
    ("mbx-stats", [mbx, "stats", "--json"]),
    ("mbx-cache", [mbx, "cache", "stats", "--json"]),
]:
    r = qa_run(args, cwd=repo, env=env, capture_output=True, text=True)
    (out / (name + ".json")).write_text(r.stdout)
    (out / (name + ".stderr")).write_text(r.stderr)
(out / "results.json").write_text(
    json.dumps(
        {
            "buildq": subprocess.check_output([bq, "--version"], text=True).strip(),
            "mbx": subprocess.check_output([mbx, "--version"], text=True).strip(),
            "results": rows,
        },
        indent=2,
    )
    + "\n"
)
print(f"{sum(r['passed'] for r in rows)}/{len(rows)} passed")
