#!/usr/bin/env python3
"""Plot completed contention measurements; no invented speedup comparison."""
import json
import os
from pathlib import Path
import tempfile

os.environ.setdefault("MPLCONFIGDIR", tempfile.mkdtemp(prefix="buildq-qa-mpl-"))
os.environ.setdefault("XDG_CACHE_HOME", tempfile.mkdtemp(prefix="buildq-qa-cache-"))
import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt

root = Path(__file__).resolve().parent
rows = [json.loads(line) for line in
        (root / "churn-contention-results.jsonl").read_text().splitlines()]
correct = sum(r["correct"] for r in rows)
fig, ax = plt.subplots(figsize=(12, 5), layout="constrained")
for profile, color in [("debug", "#c47732"), ("release", "#3973ad")]:
    subset = [r for r in rows if r["profile"] == profile]
    ax.scatter([r["round"] for r in subset], [r["seconds"] for r in subset],
               s=10, alpha=0.5, color=color, label=profile)
ax.set_yscale("log")
ax.set_xlabel("Round (six worktrees per round)")
ax.set_ylabel("End-to-end job seconds, including queue wait (log scale)")
ax.set_title(f"{correct:,}/{len(rows):,} correct outputs under concurrent dedupe",
             fontsize=17, fontweight="bold")
ax.grid(axis="y", alpha=0.2)
ax.spines[["top", "right"]].set_visible(False)
ax.legend(frameon=False)
fig.supxlabel("Source mtimes forced to 2000-01-01; feature/profile switches; two build slots, three members\n"
              "Controlled fixture with regex 1.13.1; shared 4-CPU host; reflinks unsupported; not a speedup comparison",
              fontsize=10)
fig.savefig(root / "worktree-contention.png", dpi=160)
fig.savefig(root / "worktree-contention.svg")
