#!/usr/bin/env python3
"""Audit saved Bevy outcomes and queue history without rebuilding anything."""

import argparse
import json
from pathlib import Path


def peak_overlap(rows):
    events = []
    for row in rows:
        if row["ended_ms"] > row["started_ms"]:
            events.extend([(row["started_ms"], 1), (row["ended_ms"], -1)])
    active = peak = 0
    for _, change in sorted(events):
        active += change
        peak = max(peak, active)
    assert active == 0
    return peak


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()
    provenance = json.loads((args.root / "provenance.json").read_text())
    results = json.loads((args.root / "results.json").read_text())
    modes = provenance.get(
        "modes", ["plain", "buildq2", "mbx", "combined2", "combined4"]
    )
    phases = {"cold", "warm1", "warm2"}
    expected = {
        (trial, mode, phase)
        for trial in range(1, provenance["trials"] + 1)
        for mode in modes
        for phase in phases
    }
    keys = [(row["trial"], row["mode"], row["phase"]) for row in results]
    assert len(keys) == len(set(keys)), "duplicate batch records"
    assert set(keys) == expected, f"missing batches: {expected - set(keys)}"
    checked_jobs = 0
    for row in results:
        assert len(row["jobs"]) == provenance["clients"]
        assert row["wall_s"] > 0 and row["peak_rss_bytes"] > 0
        assert row["storage"]["allocated"] > 0
        for job in row["jobs"]:
            assert job["exit"] == 0 and job["output"] == "version-A:entities=1"
            checked_jobs += 1
    histories = []
    for trial in range(1, provenance["trials"] + 1):
        for mode in modes:
            if not mode.startswith(("buildq", "combined")):
                continue
            rows = json.loads(
                (args.root / f"trial{trial}-{mode}" / "history.json").read_text()
            )
            limit = 4 if mode == "combined4" else 2
            batch_rows = [row for row in rows if row["class"] == "build"]
            assert len(batch_rows) == provenance["clients"] * len(phases)
            assert all(row["exit"] == 0 and row["target"] for row in batch_rows)
            assert len({row["target"] for row in batch_rows}) <= limit
            for phase in phases:
                selected = [
                    row
                    for row in batch_rows
                    if row.get("label", "").startswith(phase + "-")
                ]
                assert len(selected) == provenance["clients"]
                peak = peak_overlap(selected)
                assert peak <= limit, (trial, mode, phase, peak)
                histories.append(
                    dict(trial=trial, mode=mode, phase=phase, peak_build_jobs=peak)
                )
    summary = dict(
        passed=True,
        batches=len(results),
        checked_executables=checked_jobs,
        queue_history=histories,
    )
    encoded = json.dumps(summary, indent=2) + "\n"
    if args.out:
        args.out.write_text(encoded)
    print(encoded, end="")


if __name__ == "__main__":
    main()
