#!/usr/bin/env python3
"""Plot the checked-in metadata benchmark evidence with matplotlib."""
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
names = ["metadata-normal80k", "metadata-hardlink", "metadata-hardlink320k",
         "metadata-hardlink320k-default"]
rows = [json.loads((root / (name + ".json")).read_text()) for name in names]
labels = ["80k paths\nplain files\ncap 1,000", "80k paths\nhard links\ncap 1,000",
          "320k paths\nhard links\ncap 1,000", "320k paths\nhard links\ncap 100,000"]
colors = ["#218578", "#d27133", "#c24935", "#a83b49"]
plt.rcParams.update({"font.size": 11, "axes.spines.top": False,
                     "axes.spines.right": False})
fig, axes = plt.subplots(1, 2, figsize=(12, 5.5), layout="constrained")
fig.suptitle("Hard-linked artifacts bypass the dedupe index cap", fontsize=18,
             fontweight="bold")
for ax, values, ylabel in [
    (axes[0], [r["max_rss_kib"]/1024 for r in rows], "Child peak RSS (MiB)"),
    (axes[1], [r["report"]["candidates"] for r in rows], "Candidates in the first batch")]:
    bars = ax.bar(labels, values, color=colors, width=0.65)
    ax.set_ylabel(ylabel)
    ax.grid(axis="y", alpha=0.18)
    ax.set_axisbelow(True)
    ax.bar_label(bars, labels=[f"{v:,.1f}" if ax == axes[0] else f"{v:,}" for v in values],
                 padding=5)
    ax.margins(y=0.15)
axes[1].scatter(range(len(rows)), [r["max_index"] for r in rows],
                marker="_", s=600, linewidths=2, color="#173b52",
                label="Configured index cap", zorder=4)
axes[1].legend(loc="upper left", frameon=False)
fig.supxlabel("Four members; 64 KiB sparse artifacts; max_read = 1 byte; actual read_bytes = 0\n"
              "Linux overlay, buildq 1391d00; synthetic metadata workload, not an OSS build", fontsize=10)
fig.savefig(root / "dedupe-index-memory.png", dpi=160)
fig.savefig(root / "dedupe-index-memory.svg")
