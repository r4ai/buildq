#!/usr/bin/env python3
"""Archive a completed churn run and summarize busy-collector evidence."""
import argparse
import gzip
import json
from pathlib import Path
import shutil
import statistics

ap = argparse.ArgumentParser()
ap.add_argument("--root", type=Path, required=True)
ap.add_argument("--prefix", required=True)
ap.add_argument("--output", type=Path, default=Path(__file__).resolve().parent)
args = ap.parse_args()
root = args.root
output = args.output
output.mkdir(parents=True, exist_ok=True)
summary = json.loads((root / "summary.json").read_text())
jobs = [json.loads(line) for line in (root / "results.jsonl").read_text().splitlines()]
summary["verified_completed_jobs"] = len(jobs)
summary["verified_correct_jobs"] = sum(j["correct"] for j in jobs)
history = [json.loads(line) for line in (root / "q/history.jsonl").read_text().splitlines()]
summary["queue_wait"] = {}
for cls in ["build", "run"]:
    waits = sorted((r["started_ms"]-r["enqueued_ms"])/1000
                   for r in history if r["class"] == cls)
    if waits:
        summary["queue_wait"][cls] = dict(jobs=len(waits),
            median_seconds=statistics.median(waits),
            p95_seconds=waits[int((len(waits)-1)*0.95)], max_seconds=max(waits))
path = root / "concurrent-dedupe.jsonl"
if path.exists():
    records = [json.loads(line) for line in path.read_text().splitlines()]
    errors = [r for r in records if r["report"].get("errors")
              or "unparseable" in r["report"]
              or r["exit"] != 0 and not r["report"].get("skipped")]
    summary["concurrent_dedupe"] = dict(samples=len(records),
        busy_skips=sum(r["report"].get("skipped_busy", 0) for r in records),
        collector_lock_skips=sum(bool(r["report"].get("skipped")) for r in records),
        errors=len(errors), error_examples=errors[:5],
        max_seconds=max(r["seconds"] for r in records))
    with gzip.open(output / (args.prefix+"-dedupe.jsonl.gz"), "wb") as out:
        out.write(path.read_bytes())
    examples = records[:5] + [max(records, key=lambda r: r["report"].get("skipped_busy", 0))] + records[-5:]
    (output / (args.prefix+"-dedupe-examples.json")).write_text(json.dumps(examples, indent=2)+"\n")
shutil.copy2(root / "results.jsonl", output / (args.prefix+"-results.jsonl"))
shutil.copy2(root / "q/history.jsonl", output / (args.prefix+"-history.jsonl"))
for log in ["r000-wt0.stderr", "r004-wt0.stderr", "r299-wt0.stderr",
            "r299-cache-dedupe.json", "r299-cache-ls.json"]:
    src = root / "logs" / log
    if src.exists():
        shutil.copy2(src, output / (args.prefix+"-"+log))
(output / (args.prefix+"-summary.json")).write_text(json.dumps(summary, indent=2)+"\n")
print(json.dumps(summary, indent=2))
