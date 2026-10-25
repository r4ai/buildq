#!/usr/bin/env python3
"""Run the burst payload with four direct workers as a diagnostic lower bound.

This control has no queue, priority, pool, watcher or history semantics. It is
not a feature-equivalent replacement for buildq. Match Python, payload length,
client count and storage with queue/run.py when interpreting the difference.
"""

import argparse
import concurrent.futures
import json
import resource
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "mbx"))
from qa_support import cleanup, fresh_paths, run  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--clients", type=int, default=128)
    args = parser.parse_args()
    root, out = fresh_paths(args.root, args.out)
    events = root / "events.jsonl"

    def one(index):
        run(
            [
                sys.executable,
                str(HERE / "run.py"),
                "payload",
                str(events),
                f"direct-{index}",
                "0.12",
            ],
            check=True,
            timeout=10,
            capture_output=True,
        )

    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    start = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as workers:
        try:
            list(workers.map(one, range(args.clients)))
        except BaseException:
            cleanup()
            raise
    elapsed = time.monotonic() - start
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    rows = [json.loads(line) for line in events.read_text().splitlines()]
    active = peak = 0
    for row in rows:
        active += 1 if row["kind"] == "start" else -1
        peak = max(peak, active)
    assert active == 0 and peak <= 4
    assert len(rows) == 2 * args.clients
    result = dict(
        clients=args.clients,
        direct_workers=4,
        payload_seconds=0.12,
        elapsed_seconds=elapsed,
        aggregate_child_cpu_seconds=(
            after.ru_utime + after.ru_stime - before.ru_utime - before.ru_stime
        ),
        peak_payload_concurrency=peak,
        passed=True,
    )
    encoded = json.dumps(result, indent=2) + "\n"
    (out / "results.json").write_text(encoded)
    print(encoded, end="")


if __name__ == "__main__":
    main()
