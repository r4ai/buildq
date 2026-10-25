#!/usr/bin/env python3
"""Run two waves of real Bevy with buildq and mbx automatic GC both enabled.

Preparation only is cheap; executing this runner performs four real Bevy builds.
Use a new root and a separate evidence directory. Dependencies must be fetched
for the pinned fixture Cargo.lock before using --offline/--locked.
CPU/RSS are sampled process-tree metrics: detached/reparented mbx collectors may
be missed, so these values are not total GC CPU or complete GC peak memory.
"""

import argparse
import concurrent.futures
import hashlib
import importlib.util
import json
import os
import shutil
import sys
import time
from pathlib import Path

import psutil

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "mbx"))
sys.path.insert(0, str(HERE.parents[1]))
from qa_support import cleanup, fresh_paths, popen, run, stop_owned  # noqa: E402

from buildq_bench.measurement import ProcessSampler  # noqa: E402

_spec = importlib.util.spec_from_file_location("bevy_qa_run", HERE / "run.py")
_bevy_run = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_bevy_run)
inventory = _bevy_run.inventory


def command(argv, **kwargs):
    return run(
        argv, capture_output=True, text=True, check=True, timeout=60, **kwargs
    ).stdout


def collectors(cache):
    """Find only mbx auto-collectors whose working directory is our store."""
    found = []
    for process in psutil.process_iter(["pid", "cmdline", "cwd", "status"]):
        try:
            info = process.info
            argv = info["cmdline"] or []
            if (
                info["cwd"] == str(cache)
                and "gc" in argv
                and "--automatic" in argv
                and info["status"] != psutil.STATUS_ZOMBIE
            ):
                found.append(info["pid"])
        except (psutil.NoSuchProcess, psutil.AccessDenied):
            continue
    return found


def settle_collectors(cache):
    started = time.monotonic()
    quiet_since = None
    observed = set()
    while time.monotonic() - started < 30:
        current = collectors(cache)
        observed.update(current)
        if current:
            quiet_since = None
        elif quiet_since is None:
            quiet_since = time.monotonic()
        elif time.monotonic() - quiet_since >= 0.5:
            return dict(
                seconds=time.monotonic() - started,
                observed_pids=sorted(observed),
                timed_out=False,
            )
        time.sleep(0.05)
    return dict(
        seconds=time.monotonic() - started,
        observed_pids=sorted(observed),
        timed_out=True,
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root", type=Path, required=True, help="new, nonexistent fixture root"
    )
    parser.add_argument("--out", "--output", dest="out", type=Path, required=True)
    parser.add_argument("--binary", "--buildq", dest="binary", type=Path, required=True)
    parser.add_argument("--mbx", type=Path, required=True)
    parser.add_argument("--fixture", type=Path, default=HERE / "fixture")
    parser.add_argument(
        "--timeout",
        type=float,
        default=1800,
        help="maximum seconds per Cargo invocation",
    )
    args = parser.parse_args()
    args.binary = args.binary.resolve(strict=True)
    args.mbx = args.mbx.resolve(strict=True)
    args.fixture = args.fixture.resolve(strict=True)
    root, out = fresh_paths(args.root, args.out)
    cargo = shutil.which("cargo")
    if cargo is None:
        raise RuntimeError("cargo must be on PATH")
    fixture_lock = args.fixture / "Cargo.lock"
    if not fixture_lock.is_file():
        raise RuntimeError("a pinned fixture Cargo.lock is required")
    first = root / "wt0"
    shutil.copytree(
        args.fixture, first, ignore=shutil.ignore_patterns("target", ".git")
    )
    command(["git", "init", "-q", str(first)])
    command(["git", "-C", str(first), "add", "."])
    command(
        [
            "git",
            "-C",
            str(first),
            "-c",
            "user.name=QA",
            "-c",
            "user.email=qa@example.invalid",
            "commit",
            "-qm",
            "Pinned Bevy tight-budget probe",
        ]
    )
    command(
        [
            "git",
            "-C",
            str(first),
            "worktree",
            "add",
            "--detach",
            str(root / "wt1"),
            "HEAD",
        ]
    )
    state = root / "state"
    state.mkdir()
    config = f"""slots = 2
poll_ms = 25
[cache]
max_members = 2
budget = "768MiB"
min_free = "1GiB"
gc_after_job = true
gc_mode = "inline"
[cargo]
jobs = 2
backend = "mbx"
mbx = {json.dumps(str(args.mbx))}
"""
    (state / "config.toml").write_text(config)
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("BUILDQ_", "MBX_"))
        and key not in {"CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR", "RUSTC_WRAPPER"}
    }
    env.update(
        BUILDQ_DIR=str(state),
        BUILDQ_CACHE=str(root / "pool"),
        BUILDQ_TRACE="1",
        CARGO=cargo,
        CARGO_BUILD_JOBS="2",
        CARGO_INCREMENTAL="1",
        MBX_CACHE_DIR=str(root / "mbx-cache"),
        MBX_TARGET_VIEWS="0",
        MBX_TARGET_LANES="0",
        MBX_GC_AUTO="1",
        MBX_GC_INTERVAL="0s",
        MBX_GC_MAX_TOTAL_SIZE="256MiB",
        MBX_GC_MAX_SIZE="256MiB",
        MBX_GC_INCREMENTAL_MAX_SIZE="64MiB",
        MBX_GC_MIN_FREE_SIZE="1GiB",
        MBX_REMOTE_URL="",
        MBX_DISPLAY="plain",
        MBX_SUMMARY="full",
        MBX_SCHEDULER_CPUS="4",
        MBX_SCHEDULER_MEMORY="12GiB",
    )
    provenance = dict(
        buildq_sha=command(["git", "rev-parse", "HEAD"], cwd=HERE).strip(),
        buildq_sha256=hashlib.sha256(args.binary.read_bytes()).hexdigest(),
        mbx_sha256=hashlib.sha256(args.mbx.read_bytes()).hexdigest(),
        fixture_lock_sha256=hashlib.sha256(fixture_lock.read_bytes()).hexdigest(),
        rustc=command(["rustc", "-Vv"], env=env),
        cargo=command([cargo, "-V"], env=env),
        mbx=command([str(args.mbx), "--version"], env=env),
        started_utc=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        buildq_config=config,
        mbx_settings={
            key: value for key, value in env.items() if key.startswith("MBX_")
        },
        sampling=(
            "50 ms recursive process-tree RSS and sampled CPU; detached/reparented "
            "mbx collectors and short-lived processes may be missed; not total GC "
            "CPU or complete GC peak memory; RSS counts shared pages per process"
        ),
        storage=(
            "st_blocks*512, unique (device,inode) across pool and mbx-cache; "
            "reflink shared extents are not identified"
        ),
    )
    provenance["effective_mbx_settings"] = {
        key: command(
            [str(args.mbx), "settings", "get", key], cwd=first, env=env
        ).strip()
        for key in (
            "gc.auto",
            "gc.interval",
            "gc.max_total_size",
            "gc.max_size",
            "gc.incremental_max_size",
            "gc.min_free_size",
        )
    }
    (out / "provenance.json").write_text(json.dumps(provenance, indent=2) + "\n")
    results = []

    def one(wave, client):
        name = f"wave{wave}-client{client}"
        argv = [str(args.binary), "cargo", "run", "--offline", "--locked"]
        started = time.monotonic()
        with (
            (out / f"{name}.stdout").open("wb") as stdout,
            (out / f"{name}.stderr").open("wb") as stderr,
        ):
            process = popen(
                argv,
                cwd=root / f"wt{client}",
                env=env | {"BUILDQ_LABEL": name},
                stdout=stdout,
                stderr=stderr,
            )
            with ProcessSampler(process.pid) as sampler:
                try:
                    code = process.wait(timeout=args.timeout)
                except BaseException:
                    stop_owned(process)
                    raise
        text = (out / f"{name}.stdout").read_text(errors="replace").strip()
        error_text = (out / f"{name}.stderr").read_text(errors="replace")
        return dict(
            name=name,
            command=argv,
            exit=code,
            output=text,
            passed=code == 0 and text == "version-A:entities=1",
            wall_s=time.monotonic() - started,
            peak_tree_rss_bytes=sampler.stats.peak_rss_bytes,
            sampled_cpu_s=sampler.stats.sampled_cpu_seconds,
            peak_processes=sampler.stats.peak_processes,
            peak_rustc=sampler.stats.peak_rustc_processes,
            compiling_lines=sum(
                line.lstrip().startswith("Compiling ")
                for line in error_text.splitlines()
            ),
        )

    for wave in range(2):
        started = time.monotonic()
        with ProcessSampler(os.getpid(), include_root=False) as sampler:
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as workers:
                futures = [workers.submit(one, wave, client) for client in range(2)]
                try:
                    jobs = [future.result() for future in futures]
                except BaseException:
                    cleanup()
                    raise
            jobs_wall = time.monotonic() - started
            settling = settle_collectors(root / "mbx-cache")
        settled_wall = time.monotonic() - started
        snapshot = out / f"wave{wave}"
        snapshot.mkdir()
        for label, argv in [
            ("buildq-cache", [str(args.binary), "cache", "ls", "--refresh", "--json"]),
            ("buildq-history", [str(args.binary), "history", "-n", "100", "--json"]),
            ("buildq-status", [str(args.binary), "status", "--json"]),
            ("mbx-cache", [str(args.mbx), "cache", "stats", "--json"]),
            ("mbx-stats", [str(args.mbx), "stats", "--json"]),
        ]:
            result = run(
                argv, cwd=first, env=env, capture_output=True, text=True, timeout=60
            )
            (snapshot / f"{label}.json").write_text(result.stdout)
            (snapshot / f"{label}.stderr").write_text(result.stderr)
            if result.returncode:
                raise RuntimeError(
                    f"inventory command failed: {label}, exit {result.returncode}"
                )
        record = dict(
            wave=wave,
            jobs=jobs,
            jobs_wall_s=jobs_wall,
            wall_including_collector_settle_s=settled_wall,
            collector_settle=settling,
            peak_rss_bytes=sampler.stats.peak_rss_bytes,
            sampled_cpu_s=sampler.stats.sampled_cpu_seconds,
            peak_processes=sampler.stats.peak_processes,
            peak_rustc=sampler.stats.peak_rustc_processes,
            combined_storage=inventory([root / "pool", root / "mbx-cache"]),
            pool_storage=inventory([root / "pool"]),
            mbx_storage=inventory([root / "mbx-cache"]),
            whole_fixture_storage=inventory([root]),
        )
        results.append(record)
        (out / "results.json").write_text(json.dumps(results, indent=2) + "\n")
        print(json.dumps(record), flush=True)
        if settling["timed_out"] or not all(job["passed"] for job in jobs):
            raise RuntimeError(f"wave {wave} failed; retained all logs and inventories")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
