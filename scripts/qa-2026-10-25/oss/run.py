#!/usr/bin/env python3
"""Real-OSS native-backend QA. Run after sourcing the documented Rust env.

Only local clones are changed. Logs and cache go under --root; no remote writes.
"""

import argparse
import concurrent.futures
import json
import os
import shutil
import subprocess
import time
from pathlib import Path

P = argparse.ArgumentParser()
P.add_argument("--root", type=Path, default=Path("/workspace/qa-oss"))
P.add_argument(
    "--buildq", type=Path, default=Path("/workspace/buildq/target/debug/buildq")
)
P.add_argument(
    "--phase",
    choices=[
        "prepare",
        "prepare-ra",
        "cold",
        "switch",
        "parallel",
        "tests",
        "ra",
        "edits",
        "recovery",
    ],
    required=True,
)
P.add_argument("--rounds", type=int, default=6)
P.add_argument("--start", type=int, default=0)
A = P.parse_args()
ROOT = A.root.resolve()
OUT = ROOT / "results"
OUT.mkdir(parents=True, exist_ok=True)
ENV = dict(
    os.environ,
    BUILDQ_DIR=str(ROOT / "state"),
    BUILDQ_CACHE=str(ROOT / "cache"),
    CARGO_BUILD_JOBS="2",
    BUILDQ_TRACE="1",
    BUILDQ_GC="0",
)
ENV.pop("CARGO_TARGET_DIR", None)
ENV.pop("BUILDQ_HELD", None)
FAILURES = []
EXPECTED_REVISIONS = {
    "tokio": "556820ff84030b37e74e11b86b7733f5795770ea",
    "ripgrep": "4649aa9700619f94cf9c66876e9549d83420e16c",
    "rust-analyzer": "db02cdc7fc8b0e0b9aa1be4110a74620bbac1f98",
}


def verify_revision(name):
    actual = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=ROOT / name, text=True
    ).strip()
    if actual != EXPECTED_REVISIONS[name]:
        raise SystemExit(f"Unexpected {name} revision: {actual}")


def marker_source(marker):
    return (
        "\n// BUILDQ QA PROBE\n/// Local QA marker.\n"
        "pub fn buildq_qa_marker() -> &'static str { \"" + marker + '" }\n'
    )


def command(repo, label, args, expected=None, extra=None, expected_exit=0):
    original_label = label
    iteration = 0
    while (OUT / f"{label}.stdout").exists():
        iteration += 1
        label = f"{original_label}-repeat-{iteration}"
    env = ENV | (extra or {})
    start = time.monotonic()
    proc = subprocess.run(
        [str(A.buildq), "cargo", *args],
        cwd=ROOT / repo,
        env=env,
        capture_output=True,
        timeout=600,
    )
    elapsed = time.monotonic() - start
    (OUT / f"{label}.stdout").write_bytes(proc.stdout)
    (OUT / f"{label}.stderr").write_bytes(proc.stderr)
    result = {
        "label": label,
        "repo": repo,
        "args": args,
        "seconds": round(elapsed, 4),
        "exit": proc.returncode,
        "expected_exit": expected_exit,
        "correct": proc.returncode == expected_exit,
        "stdout": proc.stdout.decode(errors="replace")[-1000:],
    }
    if expected is not None:
        result["expected"] = expected
        result["correct"] = (
            result["correct"] and proc.stdout.decode().strip() == expected
        )
    if not result["correct"]:
        FAILURES.append(label)
    print(json.dumps(result), flush=True)
    with (OUT / "results.jsonl").open("a") as f:
        f.write(json.dumps(result) + "\n")
    return result


def tokio(repo, label, expected):
    return command(
        repo,
        label,
        [
            "run",
            "--offline",
            "--locked",
            "-p",
            "tokio@1.48.0",
            "--features",
            "full",
            "--example",
            "buildq_qa_probe",
        ],
        expected,
    )


def ripgrep(repo, label, expected):
    return command(
        repo,
        label,
        [
            "run",
            "--offline",
            "--locked",
            "-p",
            "ripgrep",
            "--bin",
            "rg",
            "--",
            "--version",
        ],
        expected,
        {"BUILDQ_QA_PROBE": "1"},
    )


if A.phase == "prepare":
    for name, url, tag in [
        ("tokio", "https://github.com/tokio-rs/tokio.git", "tokio-1.48.0"),
        ("ripgrep", "https://github.com/BurntSushi/ripgrep.git", "14.1.1"),
    ]:
        if not (ROOT / name).exists():
            subprocess.run(
                [
                    "git",
                    "clone",
                    "--depth",
                    "1",
                    "--branch",
                    tag,
                    url,
                    str(ROOT / name),
                ],
                check=True,
            )
        verify_revision(name)
        b = ROOT / (name + "-b")
        if not b.exists():
            subprocess.run(
                [
                    "git",
                    "-C",
                    str(ROOT / name),
                    "worktree",
                    "add",
                    "--detach",
                    str(b),
                    "HEAD",
                ],
                check=True,
            )
        pinned_lock = Path(__file__).resolve().parent / "locks" / f"{name}.Cargo.lock"
        if pinned_lock.exists():
            shutil.copy2(pinned_lock, ROOT / name / "Cargo.lock")
        if not (ROOT / name / "Cargo.lock").exists():
            subprocess.run(["cargo", "generate-lockfile"], cwd=ROOT / name, check=True)
        shutil.copy2(ROOT / name / "Cargo.lock", b / "Cargo.lock")
        for suffix, marker in [("", "AAA"), ("-b", "BBB")]:
            repo = ROOT / (name + suffix)
            if name == "tokio":
                lib = repo / "tokio/src/lib.rs"
                text = lib.read_text().split("\n// BUILDQ QA PROBE\n")[0]
                lib.write_text(text + marker_source(marker))
                probe = repo / "tokio/examples/buildq_qa_probe.rs"
                probe.parent.mkdir(exist_ok=True)
                probe.write_text("""
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let mut jobs = Vec::new();
    for i in 0..64u64 {
        jobs.push(tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(i % 7)).await;
            i
        }));
    }
    let mut sum = 0;
    for job in jobs { sum += job.await.unwrap(); }
    assert_eq!(sum, 2016);
    println!("{}", tokio::buildq_qa_marker());
}
""")
            else:
                lib = repo / "crates/grep/src/lib.rs"
                text = lib.read_text().split("\n// BUILDQ QA PROBE\n")[0]
                lib.write_text(text + marker_source(marker))
                main = repo / "crates/core/main.rs"
                text = main.read_text()
                if "BUILDQ_QA_PROBE" not in text:
                    text = text.replace(
                        "fn main() -> ExitCode {",
                        "fn main() -> ExitCode {\n"
                        '    if std::env::var_os("BUILDQ_QA_PROBE").is_some() {\n'
                        '        println!("{}", grep::buildq_qa_marker());\n'
                        "        return ExitCode::SUCCESS;\n    }",
                    )
                main.write_text(text)
            # Same mtime and length in both trees, older than resulting outputs.
            os.utime(lib, (1600000000, 1600000000))
    (ROOT / "state").mkdir(exist_ok=True)
    (ROOT / "state/config.toml").write_text(
        'slots = 2\n[cache]\nmax_members = 2\nbudget = "8G"\n'
        'min_free = "0"\ngc_mode = "inline"\n'
    )
elif A.phase == "cold":
    tokio("tokio", "tokio-cold-a", "AAA")
    tokio("tokio", "tokio-warm-a", "AAA")
    ripgrep("ripgrep", "ripgrep-cold-a", "AAA")
    ripgrep("ripgrep", "ripgrep-warm-a", "AAA")
elif A.phase == "switch":
    for i in range(A.start, A.start + A.rounds):
        suffix, marker = ("-b", "BBB") if i % 2 == 0 else ("", "AAA")
        tokio("tokio" + suffix, f"tokio-switch-{i}", marker)
        ripgrep("ripgrep" + suffix, f"ripgrep-switch-{i}", marker)
elif A.phase == "parallel":
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        futures = []
        for i in range(8):
            suffix, marker = ("-b", "BBB") if i % 2 == 0 else ("", "AAA")
            fn = tokio if i % 4 < 2 else ripgrep
            name = "tokio" if fn == tokio else "ripgrep"
            futures.append(
                pool.submit(fn, name + suffix, f"{name}-parallel-{i}", marker)
            )
        for f in futures:
            f.result()
elif A.phase == "tests":
    command(
        "tokio",
        "tokio-sync-tests",
        [
            "test",
            "--offline",
            "--locked",
            "-p",
            "tokio@1.48.0",
            "--features",
            "full",
            "--test",
            "sync_mpsc",
        ],
    )
    command(
        "ripgrep",
        "ripgrep-integration-tests",
        ["test", "--offline", "--locked", "-p", "ripgrep", "--test", "integration"],
    )
elif A.phase == "edits":
    for repo, rel, fn in [
        ("tokio-b", "tokio/src/lib.rs", tokio),
        ("ripgrep-b", "crates/grep/src/lib.rs", ripgrep),
    ]:
        lib = ROOT / repo / rel
        original = lib.read_bytes()
        stamp = lib.stat()
        try:
            lib.write_bytes(original.replace(b'{ "BBB" }', b'{ "CCC" }'))
            os.utime(lib, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
            fn(repo, repo + "-same-mtime-edit", "CCC")
        finally:
            lib.write_bytes(original)
            os.utime(lib, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
        fn(repo, repo + "-same-mtime-restore", "BBB")
elif A.phase == "recovery":
    lib = ROOT / "tokio-b/tokio/src/lib.rs"
    original = lib.read_bytes()
    stamp = lib.stat()
    try:
        lib.write_bytes(
            original + b'\ncompile_error!("intentional buildq QA compile failure");\n'
        )
        os.utime(lib, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
        command(
            "tokio-b",
            "tokio-intentional-compile-error",
            [
                "build",
                "--offline",
                "--locked",
                "-p",
                "tokio@1.48.0",
                "--features",
                "full",
                "--example",
                "buildq_qa_probe",
            ],
            expected_exit=101,
        )
    finally:
        lib.write_bytes(original)
        os.utime(lib, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
    tokio("tokio-b", "tokio-compile-error-recovery", "BBB")
elif A.phase == "prepare-ra":
    repo = ROOT / "rust-analyzer"
    b = ROOT / "rust-analyzer-b"
    if not repo.exists():
        subprocess.run(
            [
                "git",
                "clone",
                "--depth",
                "1",
                "--branch",
                "2025-07-28",
                "https://github.com/rust-lang/rust-analyzer.git",
                str(repo),
            ],
            check=True,
        )
    verify_revision("rust-analyzer")
    if not b.exists():
        subprocess.run(
            ["git", "-C", str(repo), "worktree", "add", "--detach", str(b), "HEAD"],
            check=True,
        )
    for checkout, marker in [(repo, "AAA"), (b, "BBB")]:
        lib = checkout / "crates/syntax/src/lib.rs"
        text = lib.read_text().split("\n// BUILDQ QA PROBE\n")[0]
        lib.write_text(text + marker_source(marker))
        os.utime(lib, (1600000000, 1600000000))
        probe = checkout / "crates/syntax/examples/buildq_qa_probe.rs"
        probe.parent.mkdir(exist_ok=True)
        probe.write_text("""fn main() {
    let source = "fn example() { let answer: i32 = 42; }\\n".repeat(1000);
    let parsed = syntax::SourceFile::parse(&source, syntax::Edition::CURRENT);
    assert!(parsed.errors().is_empty());
    assert_eq!(parsed.syntax_node().text().to_string(), source);
    println!("{}", syntax::buildq_qa_marker());
}
""")
elif A.phase == "ra":
    for suffix, marker, label in [
        ("", "AAA", "cold-a"),
        ("", "AAA", "warm-a"),
        ("-b", "BBB", "switch-b"),
        ("", "AAA", "switch-a"),
    ]:
        command(
            "rust-analyzer" + suffix,
            "ra-" + label,
            [
                "run",
                "--offline",
                "--locked",
                "-p",
                "syntax",
                "--example",
                "buildq_qa_probe",
            ],
            marker,
        )
    command(
        "rust-analyzer",
        "ra-parser-syntax-tests",
        ["test", "--offline", "--locked", "-p", "parser", "-p", "syntax", "--lib"],
    )

if FAILURES:
    raise SystemExit("Failed QA commands: " + ", ".join(FAILURES))
