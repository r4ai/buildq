"""Render the recorded Bevy comparison with zero-based, independent metric axes."""

from __future__ import annotations

import argparse
import json
import os
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Literal

if TYPE_CHECKING:
    from matplotlib.axes import Axes
    from matplotlib.font_manager import FontProperties

REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_SUMMARY = REPOSITORY_ROOT / "docs/benchmarks/data/bevy-2026-10-23/summary.json"
DEFAULT_OUTPUT = REPOSITORY_ROOT / "docs/benchmarks/assets"
LANGUAGES = ("en", "ja")
type Language = Literal["en", "ja"]


@dataclass(frozen=True)
class Metric:
    title_en: str
    title_ja: str
    plain: float
    pooled: float
    axis_maximum: float
    ticks: tuple[int, ...]


def metrics_from_summary(summary: dict) -> list[Metric]:
    plain = summary["waves"]["direct-concurrent-wave"]
    pooled = summary["waves"]["pooled-all-cold-wave"]
    disk = summary["disk_allocated_bytes"]
    return [
        Metric(
            "(a) Build time (s)",
            "(a) ビルド時間 (s)",
            plain["wall_s"],
            pooled["wall_s"],
            200,
            (0, 50, 100, 150, 200),
        ),
        Metric(
            "(b) Peak memory RSS (GiB)",
            "(b) ピークメモリ RSS (GiB)",
            plain["peak_tree_rss_bytes"] / 2**30,
            pooled["peak_tree_rss_bytes"] / 2**30,
            5,
            (0, 1, 2, 3, 4, 5),
        ),
        Metric(
            "(c) Target disk space (GiB)",
            "(c) target ディスク使用量 (GiB)",
            sum(disk[f"direct-{index}"] for index in range(2, 5)) / 2**30,
            disk["cache-all-cold"] / 2**30,
            4,
            (0, 1, 2, 3, 4),
        ),
    ]


def japanese_font(font_path: Path | None) -> Path:
    from matplotlib.font_manager import FontProperties, findfont

    if font_path is not None:
        if not font_path.is_file():
            raise ValueError(f"Font file does not exist: {font_path}")
        return font_path
    try:
        return Path(
            findfont(
                FontProperties(family="Noto Sans CJK JP"), fallback_to_default=False
            )
        )
    except ValueError as error:
        raise ValueError(
            "Install Noto Sans CJK, or select its font file with --font PATH"
        ) from error


def draw_metric(
    axes: Axes, metric: Metric, font: FontProperties, language: Language
) -> None:
    japanese = language == "ja"
    axes.set_axisbelow(True)
    axes.grid(axis="x", color="#e5e5e5", linewidth=0.6)
    axes.barh(
        [1, 0],
        [metric.plain, metric.pooled],
        height=0.52,
        color=["#c5c5c5", "#64788c"],
        edgecolor="#555555",
        linewidth=0.4,
    )
    axes.set_xlim(0, metric.axis_maximum)
    axes.set_ylim(-0.65, 1.65)
    axes.set_yticks(
        [1, 0],
        labels=["通常 Cargo" if japanese else "Plain Cargo", "buildq"],
        fontproperties=font,
        fontsize=11,
        color="#222222",
    )
    axes.set_xticks(metric.ticks)
    axes.tick_params(axis="y", length=0, pad=8)
    axes.tick_params(axis="x", length=3, width=0.6, labelsize=10, colors="#444444")
    for label in axes.get_xticklabels():
        label.set_fontproperties(font)
        label.set_fontsize(10)
    for side in ["top", "right", "left"]:
        axes.spines[side].set_visible(False)
    axes.spines["bottom"].set_color("#888888")
    axes.spines["bottom"].set_linewidth(0.6)
    axes.set_title(
        metric.title_ja if japanese else metric.title_en,
        loc="left",
        fontproperties=font,
        fontsize=12,
        color="#222222",
        pad=10,
    )
    for position, value in [(1, metric.plain), (0, metric.pooled)]:
        axes.text(
            value + metric.axis_maximum * 0.018,
            position,
            f"{value:.2f}",
            va="center",
            fontproperties=font,
            fontsize=10,
            color="#222222",
        )


def render_figures(
    summary_path: Path,
    output_dir: Path,
    *,
    font_path: Path | None = None,
    preview_dir: Path | None = None,
) -> list[Path]:
    # Configure the noninteractive backend only when plotting is requested.
    os.environ.setdefault(
        "MPLCONFIGDIR", str(Path(tempfile.gettempdir()) / "buildq-mpl")
    )
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from matplotlib.font_manager import FontProperties

    metrics = metrics_from_summary(json.loads(summary_path.read_text(encoding="utf-8")))
    for metric in metrics:
        if (
            not 0
            <= min(metric.plain, metric.pooled)
            <= max(metric.plain, metric.pooled)
            < metric.axis_maximum
        ):
            raise ValueError(f"Chart axis would clip the values: {metric.title_en}")
    japanese_font_path = japanese_font(font_path)
    output_dir.mkdir(parents=True, exist_ok=True)
    if preview_dir is not None:
        preview_dir.mkdir(parents=True, exist_ok=True)

    outputs = []
    with matplotlib.rc_context({"svg.hashsalt": "buildq-bevy-2026-10-23"}):
        for language in LANGUAGES:
            font = (
                FontProperties(fname=str(japanese_font_path))
                if language == "ja"
                else FontProperties(family="DejaVu Sans")
            )
            figure, axes = plt.subplots(3, 1, figsize=(6.4, 5.2))
            try:
                figure.set_facecolor("white")
                figure.subplots_adjust(
                    left=0.21, right=0.96, top=0.93, bottom=0.08, hspace=1.05
                )
                for panel, metric in zip(axes, metrics, strict=True):
                    draw_metric(panel, metric, font, language)
                filename = (
                    "bevy-performance.ja.svg"
                    if language == "ja"
                    else "bevy-performance.svg"
                )
                output = output_dir / filename
                figure.savefig(
                    output,
                    format="svg",
                    bbox_inches="tight",
                    pad_inches=0.16,
                    metadata={
                        "Date": None,
                        "Description": (
                            "Three simultaneous Bevy worktree builds, initially empty "
                            "caches; each metric starts at zero. "
                            "Pool reuse is included."
                        ),
                    },
                )
                svg = output.read_text(encoding="utf-8")
                output.write_text(
                    "\n".join(line.rstrip() for line in svg.splitlines()) + "\n",
                    encoding="utf-8",
                )
                if preview_dir is not None:
                    figure.savefig(
                        preview_dir / f"bevy-performance-{language}.png",
                        dpi=150,
                        bbox_inches="tight",
                        pad_inches=0.16,
                    )
                outputs.append(output)
            finally:
                plt.close(figure)
    return outputs


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--summary", type=Path, default=DEFAULT_SUMMARY)
    parser.add_argument("--output-dir", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--font", type=Path, help="Japanese Noto Sans CJK font file")
    parser.add_argument(
        "--preview-dir", type=Path, help="Optional PNG preview directory"
    )
    options = parser.parse_args()
    try:
        outputs = render_figures(
            options.summary,
            options.output_dir,
            font_path=options.font,
            preview_dir=options.preview_dir,
        )
    except ValueError as error:
        parser.error(str(error))
    for output in outputs:
        print(output)
    return 0
