import json
import xml.etree.ElementTree as ET
from pathlib import Path

import pytest

from buildq_bench.plotting import DEFAULT_SUMMARY, metrics_from_summary, render_figures


def test_chart_metrics_use_the_published_wave_and_disk_totals() -> None:
    metrics = metrics_from_summary(json.loads(DEFAULT_SUMMARY.read_text()))

    assert metrics[0].plain == pytest.approx(165.14, abs=0.01)
    assert metrics[0].pooled == pytest.approx(120.80, abs=0.01)
    assert metrics[1].plain == pytest.approx(4.37, abs=0.01)
    assert metrics[1].pooled == pytest.approx(2.89, abs=0.01)
    assert metrics[2].plain == pytest.approx(3.21, abs=0.01)
    assert metrics[2].pooled == pytest.approx(2.14, abs=0.01)
    assert all(metric.ticks[0] == 0 for metric in metrics)


def test_bilingual_figures_are_reproducible(tmp_path: Path) -> None:
    outputs = render_figures(DEFAULT_SUMMARY, tmp_path)
    first_render = [path.read_bytes() for path in outputs]
    for path in outputs:
        assert ET.parse(path).getroot().tag.endswith("svg")
    render_figures(DEFAULT_SUMMARY, tmp_path)

    assert [path.read_bytes() for path in outputs] == first_render
    assert not list(tmp_path.glob("*.png"))


def test_missing_font_fails_before_writing_figures(tmp_path: Path) -> None:
    with pytest.raises(ValueError, match="Font file does not exist"):
        render_figures(DEFAULT_SUMMARY, tmp_path, font_path=tmp_path / "missing.ttf")
    assert not list(tmp_path.glob("*.svg"))
