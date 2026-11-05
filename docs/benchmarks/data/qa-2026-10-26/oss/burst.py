#!/usr/bin/env python3
"""Simultaneous real-OSS builds sharing one two-slot queue."""

import argparse
import json
import os
import subprocess
import time
from pathlib import Path
import psutil

p = argparse.ArgumentParser()
p.add_argument("--root", type=Path, required=True)
p.add_argument("--buildq", required=True)
p.add_argument("--jobs", type=int, default=12)
p.add_argument("--label", default="burst")
p.add_argument("--timeout", type=float, default=900)
a = p.parse_args()
root = a.root.resolve()
logs = root / "logs" / a.label
logs.mkdir(parents=True, exist_ok=True)
env = {
    k: v
    for k, v in os.environ.items()
    if not k.startswith("BUILDQ_")
    and k not in ("CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR")
}
env.update(
    BUILDQ_DIR=str(root / "state"),
    BUILDQ_CACHE=str(root / "cache"),
    BUILDQ_CONFIG=str(root / "state/config.toml"),
    CARGO_HOME=str(root / "cargo-home"),
    CARGO_BUILD_JOBS="1",
    TMPDIR=str(root / "tmp"),
    RUSTC_WRAPPER="",
)
packages = {
    "bevy": ["bevy_ecs"],
    "tokio": ["tokio@1.45.1"],
    "ripgrep": ["ripgrep"],
    "rust-analyzer": ["syntax"],
}
jobs = []
start = time.monotonic()
for i in range(a.jobs):
    repo, pkgs = list(packages.items())[i % len(packages)]
    wt = (i // len(packages)) % 3
    name = f"{repo}-{wt}" + (f"-{i:02d}" if a.jobs != 12 else "")
    command = [a.buildq, "cargo", "build", "-p", pkgs[0]]
    cwd = root / "worktrees" / f"{repo}-{wt}"
    out = (logs / f"{name}.out").open("wb")
    err = (logs / f"{name}.err").open("wb")
    proc = subprocess.Popen(
        command,
        cwd=cwd,
        env={**env, "BUILDQ_LABEL": f"qa-{a.label}-{name}"},
        stdout=out,
        stderr=err,
        start_new_session=True,
    )
    jobs.append(
        {
            "name": name,
            "command": command,
            "cwd": str(cwd),
            "process": proc,
            "out": out,
            "err": err,
            "started_s": time.monotonic() - start,
        }
    )
peak_rss = 0
samples = []
while any(j["process"].poll() is None for j in jobs):
    rss = 0
    for job in jobs:
        proc = job["process"]
        rc = proc.poll()
        if rc is not None and "exit" not in job:
            job.update(exit=rc, finished_s=round(time.monotonic() - start, 3))
        try:
            procs = [
                psutil.Process(proc.pid),
                *psutil.Process(proc.pid).children(recursive=True),
            ]
            for child in procs:
                try:
                    rss += child.memory_info().rss
                except psutil.Error:
                    pass
        except psutil.Error:
            pass
    peak_rss = max(peak_rss, rss)
    status = subprocess.run(
        [a.buildq, "status", "--json"],
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
    )
    try:
        state = json.loads(status.stdout)
    except ValueError:
        state = {"error": status.stderr}
    samples.append(
        {
            "elapsed_s": round(time.monotonic() - start, 3),
            "rss_bytes": rss,
            "status": state,
        }
    )
    if time.monotonic() - start > a.timeout:
        for job in jobs:
            if job["process"].poll() is None:
                import signal

                os.killpg(job["process"].pid, signal.SIGTERM)
        break
    time.sleep(2)
for j in jobs:
    j["exit"] = j["process"].wait()
    j.setdefault("finished_s", round(time.monotonic() - start, 3))
    j["out"].close()
    j["err"].close()
result = {
    "submitted_jobs": a.jobs,
    "wall_s": round(time.monotonic() - start, 3),
    "peak_tree_rss_bytes": peak_rss,
    "jobs": [
        {k: v for k, v in j.items() if k not in ("process", "out", "err")} for j in jobs
    ],
    "samples": samples,
}
(logs / "results.json").write_text(json.dumps(result, indent=2) + "\n")
print(json.dumps({k: v for k, v in result.items() if k != "samples"}, indent=2))
