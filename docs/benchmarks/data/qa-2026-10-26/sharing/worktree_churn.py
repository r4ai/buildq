#!/usr/bin/env python3
"""Bounded cross-worktree correctness/seed fallback benchmark (stdlib only).

Uses the preinstalled Rust 1.89.0 and cached regex 1.13.1. All changes are
confined to --root; no upstream repository is modified.
"""
import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import statistics
import subprocess
import threading
import time


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", type=Path, required=True)
    ap.add_argument("--buildq", type=Path, required=True)
    ap.add_argument("--rounds", type=int, default=12)
    ap.add_argument("--seed-members", choices=["true", "false"], default="true")
    ap.add_argument("--concurrent-dedupe", action="store_true")
    ap.add_argument("--identical-source", action="store_true")
    args = ap.parse_args()
    root = args.root.resolve()
    root.mkdir(parents=True, exist_ok=True)
    logs = root / "logs"
    logs.mkdir(exist_ok=True)
    env = os.environ.copy()
    for key in ["CARGO_TARGET_DIR", "BUILDQ_HELD", "BUILDQ_POOL", "BUILDQ_CONFIG"]:
        env.pop(key, None)
    env.update(CARGO_HOME="/workspace/.buildq-env/cargo",
               RUSTUP_HOME="/workspace/.buildq-env/rustup", RUSTUP_TOOLCHAIN="1.89.0",
               CARGO_BUILD_JOBS="1", BUILDQ_DIR=str(root / "q"),
               BUILDQ_CACHE=str(root / "cache"), BUILDQ_GC="0", BUILDQ_TRACE="1",
               BUILDQ_REAL_CARGO="/workspace/.buildq-env/cargo/bin/cargo")
    env["PATH"] = "/workspace/.buildq-env/cargo/bin:" + env["PATH"]
    (root / "q").mkdir(exist_ok=True)
    (root / "q/config.toml").write_text(
        "slots=2\nmax_per_worktree=1\nrun_slots=2\n[cargo]\nmirror_bins=false\n"
        "[cache]\nbudget='3G'\nmin_free='0'\nmax_members=3\n"
        f"seed_members={args.seed_members}\ndedupe_min_size='1K'\n")

    def cmd(argv, cwd=None):
        p = subprocess.run(argv, cwd=cwd, env=env, text=True, capture_output=True)
        if p.returncode:
            if argv[1:3] == ["cache", "dedupe"]:
                try:
                    report = json.loads(p.stdout)
                    if report.get("skipped") and not report.get("errors"):
                        return p
                except json.JSONDecodeError:
                    pass
            raise RuntimeError(f"{argv}: {p.stdout}\n{p.stderr}")
        return p

    repo = root / "repo"
    repo.mkdir(exist_ok=True)
    (repo / "src").mkdir(exist_ok=True)
    (repo / "Cargo.toml").write_text(
        '[package]\nname="sharing-probe"\nversion="0.1.0"\nedition="2021"\n'
        '[dependencies]\nregex="=1.13.1"\n'
        '[features]\nextra=[]\n')
    source = '''fn main() {
    let text = "MARKER";
    let pattern = regex::Regex::new("^[A-Z0-9-]+$").unwrap();
    assert!(pattern.is_match(text));
    println!("{}:{}:{}", text, cfg!(feature="extra"), env!("CARGO_MANIFEST_DIR"));
}
'''
    (repo / "src/main.rs").write_text(source.replace("MARKER", "MAIN"))
    (repo / ".gitignore").write_text("target/\n")
    if not (repo / ".git").exists():
        cmd(["git", "init", "-q"], repo)
        cmd(["git", "config", "user.name", "buildq QA"], repo)
        cmd(["git", "config", "user.email", "buildq-qa@example.invalid"], repo)
        cmd(["git", "add", "."], repo)
        cmd(["git", "commit", "-qm", "isolated sharing QA fixture"], repo)
    trees = []
    for i in range(6):
        tree = root / f"wt{i}"
        if not tree.exists():
            cmd(["git", "worktree", "add", "--detach", str(tree)], repo)
        trees.append(tree)

    records = []
    results = root / "results.jsonl"
    stop = threading.Event()

    def background_dedupe():
        counter = 0
        with (root / "concurrent-dedupe.jsonl").open("a") as out:
            while not stop.is_set():
                argv = [str(args.buildq), "cache", "dedupe", "--json"]
                if counter % 2:
                    argv.append("--dry-run")
                start = time.monotonic()
                p = subprocess.run(argv, env=env, capture_output=True, text=True)
                try:
                    report = json.loads(p.stdout)
                except json.JSONDecodeError:
                    report = {"unparseable": p.stdout, "stderr": p.stderr}
                out.write(json.dumps(dict(iteration=counter, exit=p.returncode,
                                           seconds=time.monotonic()-start,
                                           report=report))+"\n")
                out.flush()
                counter += 1
                stop.wait(0.5)

    worker = None
    if args.concurrent_dedupe:
        worker = threading.Thread(target=background_dedupe, daemon=True)
        worker.start()

    def one(round_no, i):
        tree = trees[i]
        marker = "SHARED" if args.identical_source else f"WT{i}-R{round_no}"
        path = tree / "src/main.rs"
        path.write_text(source.replace("MARKER", marker))
        # Make every edit older than all cached Cargo fingerprints. Content
        # invalidation, rather than mtime, must prevent stale executables.
        os.utime(path, (946684800, 946684800))
        features = round_no % 2 == 1
        profile = "release" if round_no % 5 == 4 else "debug"
        argv = [str(args.buildq), "cargo", "run", "--offline", "--quiet"]
        if features:
            argv += ["--features", "extra"]
        if profile == "release":
            argv += ["--release"]
        start = time.monotonic()
        p = subprocess.run(argv, cwd=tree, env=env, text=True, capture_output=True)
        elapsed = time.monotonic() - start
        expected = f"{marker}:{str(features).lower()}:{tree}"
        record = dict(round=round_no, worktree=i, command=argv, seconds=elapsed,
                      exit=p.returncode, expected=expected, actual=p.stdout.strip(),
                      correct=p.returncode == 0 and p.stdout.strip() == expected,
                      profile=profile, features=features)
        (logs / f"r{round_no:03}-wt{i}.stderr").write_text(p.stderr)
        (logs / f"r{round_no:03}-wt{i}.stdout").write_text(p.stdout)
        return record

    start = time.monotonic()
    for round_no in range(args.rounds):
        with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
            batch = list(pool.map(lambda i: one(round_no, i), range(6)))
        with results.open("a") as out:
            for record in batch:
                out.write(json.dumps(record) + "\n")
        records += batch
        for extra in [["cache", "dedupe", "--dry-run", "--json"],
                      ["cache", "dedupe", "--json"], ["cache", "ls", "--refresh", "--json"]]:
            p = cmd([str(args.buildq)] + extra, repo)
            (logs / f"r{round_no:03}-{'-'.join(extra[:2])}{'-dry' if '--dry-run' in extra else ''}.json").write_text(p.stdout)
        print(json.dumps(dict(round=round_no, seconds=time.monotonic()-start,
                              correct=sum(r["correct"] for r in batch),
                              total=len(batch))), flush=True)
        if not all(r["correct"] for r in batch):
            break
    stop.set()
    if worker is not None:
        worker.join(timeout=60)
    summary = dict(seed_members=args.seed_members, jobs=len(records),
                   correct=sum(r["correct"] for r in records),
                   seconds=time.monotonic()-start,
                   median_job_seconds=statistics.median(r["seconds"] for r in records),
                   max_job_seconds=max(r["seconds"] for r in records))
    (root / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary), flush=True)


if __name__ == "__main__":
    main()
