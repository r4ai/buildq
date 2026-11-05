#!/usr/bin/env python3
"""Plot measured observability gaps; both snapshots retain raw commands and times."""

import argparse
import json
from pathlib import Path
import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt

p = argparse.ArgumentParser()
p.add_argument("--data", type=Path, required=True)
a = p.parse_args()
f = json.loads((a.data / "freshness-snapshot.json").read_text())
v = json.loads((a.data / "burst64-visibility-snapshot.json").read_text())
cached = json.loads(f["cached_listing"]["stdout"])["total"] / 2**30
fresh = json.loads(f["fresh_listing"]["stdout"])["total"] / 2**30
actual = int(f["du"]["stdout"].split()[0]) * 1024 / 2**30
fig, axs = plt.subplots(1, 2, figsize=(11, 4.8), layout="constrained")
colors = ["#d18a32", "#147d92", "#78aaa8"]
values = [cached, fresh, actual]
bars = axs[0].bar(["Cached total", "--refresh", "Actual du"], values, color=colors)
for b, y in zip(bars, values):
    axs[0].text(b.get_x() + b.get_width() / 2, y + 0.06, f"{y:.2f}", ha="center")
axs[0].set_ylabel("Allocated GiB")
axs[0].set_ylim(0, 5)
axs[0].set_title(
    "Live member growth is hidden by cached totals\n06:07:16 UTC · ~0.25-second window"
)
values = [
    v["active_owned_buildq"],
    v["visible_owned_buildq"],
    len(v["missing_owned_buildq"]),
]
bars = axs[1].bar(["Live jobs", "In status", "Pool wait hidden"], values, color=colors)
for b, y in zip(bars, values):
    axs[1].text(b.get_x() + b.get_width() / 2, y + 0.7, str(y), ha="center")
axs[1].set_ylim(0, 65)
axs[1].set_ylabel("Owned buildq processes")
axs[1].set_title(
    "Pool lease waiters are absent from status\n06:13:12 UTC · 40-millisecond snapshot"
)
for ax in axs:
    ax.spines["top"].set_visible(False)
    ax.spines["right"].set_visible(False)
fig.suptitle("buildq QA · measured observability limitations", fontweight="bold")
fig.savefig(a.data / "observability.svg")
fig.savefig(a.data / "observability.png", dpi=150)
