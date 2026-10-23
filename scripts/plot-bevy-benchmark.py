#!/usr/bin/env python3
"""Render README charts from the recorded benchmark (requires matplotlib).

Usage: MPLCONFIGDIR=/tmp/buildq-mpl python3 scripts/plot-bevy-benchmark.py
Japanese labels use the system Noto Sans CJK font.
"""
import json
import os
from pathlib import Path

os.environ.setdefault('MPLCONFIGDIR', '/tmp/buildq-mpl')
import matplotlib
matplotlib.use('Agg')
matplotlib.rcParams['svg.hashsalt'] = 'buildq-bevy-2026-10-23'
import matplotlib.pyplot as plt
from matplotlib.font_manager import FontProperties

ROOT = Path(__file__).resolve().parents[1]
DATA = ROOT / 'docs/benchmarks/data/bevy-2026-10-23/summary.json'
OUT = ROOT / 'docs/benchmarks/assets'


def render():
    data = json.loads(DATA.read_text())
    plain = data['waves']['direct-concurrent-wave']
    pooled = data['waves']['pooled-all-cold-wave']
    disk = data['disk_allocated_bytes']
    values = [
        (plain['wall_s'], pooled['wall_s'], 's'),
        (plain['peak_tree_rss_bytes'] / 2**30, pooled['peak_tree_rss_bytes'] / 2**30, 'GiB'),
        (sum(disk[f'direct-{i}'] for i in range(2, 5)) / 2**30, disk['cache-all-cold'] / 2**30, 'GiB'),
    ]
    OUT.mkdir(parents=True, exist_ok=True)
    for locale in ['en', 'ja']:
        japanese = locale == 'ja'
        font = FontProperties(fname='/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc') if japanese else FontProperties(family='DejaVu Sans')
        headings = ['(a) ビルド時間 (s)', '(b) ピークメモリ RSS (GiB)', '(c) target ディスク使用量 (GiB)'] if japanese else ['(a) Build time (s)', '(b) Peak memory RSS (GiB)', '(c) Target disk space (GiB)']
        limits = [(200, [0, 50, 100, 150, 200]), (5, [0, 1, 2, 3, 4, 5]), (4, [0, 1, 2, 3, 4])]
        fig, axes = plt.subplots(3, 1, figsize=(6.4, 5.2))
        fig.set_facecolor('white')
        fig.subplots_adjust(left=.21, right=.96, top=.93, bottom=.08, hspace=1.05)
        for ax, title, (before, after, unit), (maximum, ticks) in zip(axes, headings, values, limits):
            ax.set_axisbelow(True)
            ax.grid(axis='x', color='#e5e5e5', linewidth=.6)
            ax.barh([1, 0], [before, after], height=.52, color=['#c5c5c5', '#64788c'], edgecolor='#555555', linewidth=.4)
            ax.set_xlim(0, maximum)
            ax.set_ylim(-.65, 1.65)
            ax.set_yticks([1, 0], labels=['通常 Cargo' if japanese else 'Plain Cargo', 'buildq'], fontproperties=font, fontsize=11, color='#222222')
            ax.set_xticks(ticks)
            ax.tick_params(axis='y', length=0, pad=8)
            ax.tick_params(axis='x', length=3, width=.6, labelsize=10, colors='#444444')
            for label in ax.get_xticklabels():
                label.set_fontproperties(font)
                label.set_fontsize(10)
            for side in ['top', 'right', 'left']:
                ax.spines[side].set_visible(False)
            ax.spines['bottom'].set_color('#888888')
            ax.spines['bottom'].set_linewidth(.6)
            ax.set_title(title, loc='left', fontproperties=font, fontsize=12, color='#222222', pad=10)
            for y, value in [(1, before), (0, after)]:
                ax.text(value + maximum * .018, y, f'{value:.2f}', va='center', fontproperties=font, fontsize=10, color='#222222')
        filename = 'bevy-performance.ja.svg' if japanese else 'bevy-performance.svg'
        fig.savefig(OUT / filename, format='svg', bbox_inches='tight', pad_inches=.16, metadata={'Date': None, 'Description': 'Three simultaneous Bevy worktree builds, initially empty caches; each metric starts at zero. Pool reuse is included.'})
        svg = OUT / filename
        svg.write_text('\n'.join(line.rstrip() for line in svg.read_text().splitlines()) + '\n')
        fig.savefig(f'/tmp/buildq-performance-{locale}.png', dpi=150, bbox_inches='tight', pad_inches=.16)
        plt.close(fig)


if __name__ == '__main__':
    render()
