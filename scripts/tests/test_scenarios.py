from pathlib import Path

from buildq_bench.scenarios import has_run_holder, isolated_environment, maximum_overlap


def test_overlap_uses_job_intervals_and_ignores_zero_duration() -> None:
    jobs = [
        {"started_ms": 0, "ended_ms": 20},
        {"started_ms": 10, "ended_ms": 30},
        {"started_ms": 20, "ended_ms": 40},
        {"started_ms": 20, "ended_ms": 20},
    ]
    assert maximum_overlap(jobs) == 2
    assert maximum_overlap([]) == 0


def test_run_readiness_requires_the_correct_busy_holder() -> None:
    def group(name: str, busy: bool, label: str) -> dict:
        return {
            "class": name,
            "holders": [{"busy": busy, "job": {"label": label}}],
        }

    status = {"classes": [group("build", True, "app"), group("run", False, "app")]}
    assert not has_run_holder(status, "app")
    status["classes"][1] = group("run", True, "another-app")
    assert not has_run_holder(status, "app")
    status["classes"][1] = group("run", True, "app")
    assert has_run_holder(status, "app")


def test_environment_isolation_preserves_toolchain_settings(monkeypatch) -> None:
    monkeypatch.setenv("BUILDQ_SLOTS", "99")
    monkeypatch.setenv("BUILDQ_HELD", "all")
    monkeypatch.setenv("CARGO_TARGET_DIR", "/unrelated-target")
    monkeypatch.setenv("CARGO_BUILD_TARGET_DIR", "/another-target")
    monkeypatch.setenv("CARGO_HOME", "/toolchain/cargo")
    environment = isolated_environment(Path("/benchmark"))

    assert "BUILDQ_SLOTS" not in environment
    assert "BUILDQ_HELD" not in environment
    assert "CARGO_TARGET_DIR" not in environment
    assert "CARGO_BUILD_TARGET_DIR" not in environment
    assert environment["CARGO_HOME"] == "/toolchain/cargo"
    assert environment["CARGO_BUILD_JOBS"] == "2"
    assert environment["BUILDQ_DIR"] == "/benchmark/state"
