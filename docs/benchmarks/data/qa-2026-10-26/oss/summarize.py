#!/usr/bin/env python3
"""Extract isolated queue/storage observations and plot the recorded experiment."""

import argparse
import json
from collections import Counter
from pathlib import Path
import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt

p = argparse.ArgumentParser()
p.add_argument("--logs", type=Path, required=True)
p.add_argument("--output", type=Path, required=True)
a = p.parse_args()
a.output.mkdir(parents=True, exist_ok=True)
records = [json.loads(s) for s in (a.logs / "soak.jsonl").read_text().splitlines()]
samples = [r for r in records if r["type"] == "sample"]
actions = [r for r in records if r["type"] == "action"]
start = next(r["t"] for r in records if r["type"] == "start")
summary = {
    "elapsed_s": round(samples[-1]["t"] - start, 3),
    "action_counts": dict(Counter(r["kind"] for r in actions)),
    "repo_action_counts": dict(Counter(r["repo"] for r in actions)),
    "unexpected_exit_failures": [
        {
            "index": r["index"],
            "kind": r["kind"],
            "repo": r["repo"],
            "exit": r.get("exit"),
            "timed_out": r.get("timed_out"),
            "stderr_tail": r.get("stderr_tail"),
        }
        for r in actions
        if r.get("expect_success") and r.get("exit") != 0
    ],
    "sample_violations": dict(
        Counter(v["invariant"] for r in samples for v in r.get("violations", []))
    ),
    "max_cache_allocated_bytes": max(r["du_bytes"] or 0 for r in samples),
    "min_free_bytes": min(r["free_disk"] for r in samples),
    "max_build_holders": max(
        sum(
            h["busy"]
            for g in r["status"]["classes"]
            if g["class"] == "build"
            for h in g["holders"]
        )
        for r in samples
        if r.get("status")
    ),
    "max_queue_waiters": max(
        sum(len(g["waiters"]) for g in r["status"]["classes"])
        for r in samples
        if r.get("status")
    ),
    "rss_scope": "Use burst.peak_tree_rss_bytes for scoped RSS; soak global RSS includes other QA experiments.",
}
if (a.logs / "burst/results.json").exists():
    burst = json.loads((a.logs / "burst/results.json").read_text())
    summary["burst"] = {k: v for k, v in burst.items() if k != "samples"}
if (a.logs / "burst64/results.json").exists():
    burst = json.loads((a.logs / "burst64/results.json").read_text())
    summary["burst64"] = {k: v for k, v in burst.items() if k != "samples"}
    summary["burst64"]["sample_count"] = len(burst["samples"])
    summary["burst64"]["max_visible_burst_waiters"] = max(
        sum(
            str(w.get("label", "")).startswith("qa-burst64-")
            for g in s["status"]["classes"]
            for w in g["waiters"]
        )
        for s in burst["samples"]
    )
    summary["burst64"]["max_build_holders"] = max(
        sum(
            h["busy"]
            for g in s["status"]["classes"]
            if g["class"] == "build"
            for h in g["holders"]
        )
        for s in burst["samples"]
    )
if (a.logs / "real-edit/results.json").exists():
    summary["real_edit"] = json.loads((a.logs / "real-edit/results.json").read_text())
if (a.logs / "full/measurements.jsonl").exists():
    summary["full_application"] = [
        json.loads(s)
        for s in (a.logs / "full/measurements.jsonl").read_text().splitlines()
    ]
if (a.logs / "run/correctness.json").exists():
    summary["staged_ripgrep"] = json.loads(
        (a.logs / "run/correctness.json").read_text()
    )
if (a.logs / "manual-busy-gc.json").exists():
    summary["manual_busy_gc"] = json.loads((a.logs / "manual-busy-gc.json").read_text())
if any(r["type"] == "resume" for r in records):
    resumed_at = max(r["t"] for r in records if r["type"] == "resume")
    summary["post_resume_sample_violations"] = dict(
        Counter(
            v["invariant"]
            for r in samples
            if r["t"] >= resumed_at
            for v in r.get("violations", [])
        )
    )
(a.output / "observations.json").write_text(json.dumps(summary, indent=2) + "\n")
plt.rcParams.update(
    {
        "font.family": "DejaVu Sans",
        "font.size": 10,
        "axes.spines.top": False,
        "axes.spines.right": False,
    }
)
fig, axs = plt.subplots(2, 1, figsize=(10, 6), layout="constrained")
x = [(r["t"] - start) / 60 for r in samples]
held = [
    sum(
        h["busy"]
        for g in r["status"]["classes"]
        if g["class"] == "build"
        for h in g["holders"]
    )
    if r.get("status")
    else float("nan")
    for r in samples
]
wait = [
    sum(len(g["waiters"]) for g in r["status"]["classes"])
    if r.get("status")
    else float("nan")
    for r in samples
]
axs[0].step(x, wait, where="post", color="#147d92", label="Queued jobs")
axs[0].step(x, held, where="post", color="#e08a34", label="Occupied build slots")
axs[0].axhline(2, color="#e08a34", linestyle=":", alpha=0.5)
axs[0].set_ylabel("Jobs")
axs[0].legend(loc="upper right")
axs[0].set_title("Four real OSS repositories under one two-slot queue")
axs[1].plot(
    x,
    [
        r["du_bytes"] / 2**30 if r["du_bytes"] is not None else float("nan")
        for r in samples
    ],
    color="#147d92",
    label="Allocated target cache",
)
axs[1].axhline(5, color="#b74b53", linestyle="--", label="5 GiB target budget")
for r in actions:
    if r["kind"] in ("gc", "gc_pressure"):
        axs[1].axvline((r["t"] - start) / 60, color="#b74b53", alpha=0.3)
if (a.logs / "manual-busy-gc-event.json").exists():
    event = json.loads((a.logs / "manual-busy-gc-event.json").read_text())
    axs[1].axvline(
        (event["t"] - start) / 60,
        color="#b74b53",
        alpha=0.6,
        label="Manual busy GC (1 GiB)",
    )
axs[1].set_xlabel("Minutes since soak start")
axs[1].set_ylabel("GiB")
axs[1].legend(loc="upper right")
fig.suptitle("buildq QA · 2026-10-26 · shared 4-CPU / 16 GiB host", fontweight="bold")
fig.savefig(a.output / "oss-queue-storage.svg")
fig.savefig(a.output / "oss-queue-storage.png", dpi=150)
plt.close(fig)
print(json.dumps(summary, indent=2))
