import json
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from buildq_bench.measurement import measure


def test_command_output_exit_code_and_resource_record(tmp_path: Path) -> None:
    program = (
        "import sys,time; memory=bytearray(8*1024*1024); "
        "print('version-A:entities=1'); "
        "print('   Compiling probe',file=sys.stderr); time.sleep(.15); sys.exit(7)"
    )
    result = measure("probe", tmp_path, tmp_path, [sys.executable, "-c", program])

    assert result.exit == 7
    assert result.compiling_lines == 1
    assert result.wall_s >= 0.15
    assert result.peak_tree_rss_bytes >= 8 * 1024 * 1024
    assert (tmp_path / "probe.stdout").read_text().strip() == "version-A:entities=1"
    assert json.loads((tmp_path / "measurements.jsonl").read_text()) == result.to_dict()


def test_resources_include_a_real_child_process(tmp_path: Path) -> None:
    child = "import time; memory=bytearray(12*1024*1024); time.sleep(.25)"
    parent = f"import subprocess,sys; subprocess.run([sys.executable,'-c',{child!r}])"
    result = measure("tree", tmp_path, tmp_path, [sys.executable, "-c", parent])

    assert result.exit == 0
    assert result.peak_processes >= 2
    assert result.peak_tree_rss_bytes >= 12 * 1024 * 1024


def test_completion_does_not_wait_for_the_sample_interval(
    tmp_path, monkeypatch
) -> None:
    monkeypatch.setattr("buildq_bench.measurement.SAMPLE_INTERVAL_SECONDS", 5)
    result = measure(
        "short",
        tmp_path,
        tmp_path,
        [sys.executable, "-c", "import time; time.sleep(.05)"],
    )

    assert result.exit == 0
    assert result.wall_s < 2


def test_concurrent_results_remain_separate_jsonl_records(tmp_path: Path) -> None:
    with ThreadPoolExecutor(max_workers=3) as workers:
        results = list(
            workers.map(
                lambda index: measure(
                    f"job-{index}",
                    tmp_path,
                    tmp_path,
                    [sys.executable, "-c", "import time; time.sleep(.08)"],
                ),
                range(3),
            )
        )

    records = [
        json.loads(line)
        for line in (tmp_path / "measurements.jsonl").read_text().splitlines()
    ]
    assert {record["name"] for record in records} == {result.name for result in results}
    assert len(records) == 3
    assert all(record["exit"] == 0 for record in records)


def test_already_exited_commands_are_valid_measurements(tmp_path: Path) -> None:
    for index in range(3):
        result = measure(
            f"instant-{index}", tmp_path, tmp_path, [sys.executable, "-c", "pass"]
        )
        assert result.exit == 0
