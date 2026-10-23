"""Run isolated Bevy build, worktree-reuse, and concurrency benchmarks on Linux."""

import argparse
import json
import os
import shutil
import subprocess
import time
from collections.abc import Mapping, Sequence
from concurrent.futures import Future, ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path

from buildq_bench.measurement import Measurement, ProcessSampler, measure

FIXTURE = Path(__file__).resolve().parents[1] / "fixtures/bevy-heavy"
BUILD_ARGUMENTS = ("build", "--offline", "--locked")
RUN_ARGUMENTS = ("run", "--offline", "--locked")
OLD_MTIME_SECONDS = 1_600_000_000
CONFIG = """\
slots = 2
poll_ms = 50

[cache]
max_members = 2
budget = "12G"
min_free = "3G"
gc_after_job = false

[cargo]
jobs = 2
"""


class BenchmarkError(RuntimeError):
    """An observed result violated a benchmark's expected behavior."""


def isolated_environment(root: Path) -> dict[str, str]:
    """Override queue settings without changing the caller's Rust toolchain."""
    environment = {
        name: value
        for name, value in os.environ.items()
        if not name.startswith("BUILDQ_")
        and name not in {"CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"}
    }
    environment.update(
        BUILDQ_DIR=str(root / "state"),
        BUILDQ_CACHE=str(root / "cache"),
        BUILDQ_GC="0",
        BUILDQ_TRACE="1",
        CARGO_BUILD_JOBS="2",
        RUSTC_WRAPPER="",
    )
    return environment


def maximum_overlap(jobs: Sequence[dict]) -> int:
    """Count overlapping half-open [started_ms, ended_ms) job intervals."""
    events: list[tuple[int, int]] = []
    for job in jobs:
        started, ended = job["started_ms"], job["ended_ms"]
        if ended > started:
            events.extend([(started, 1), (ended, -1)])
    active = peak = 0
    for _, change in sorted(events):
        active += change
        peak = max(peak, active)
    return peak


def has_run_holder(status: dict, label: str) -> bool:
    return any(
        group["class"] == "run"
        and any(
            holder["busy"] and (holder.get("job") or {}).get("label") == label
            for holder in group["holders"]
        )
        for group in status["classes"]
    )


@dataclass
class Benchmark:
    root: Path
    buildq: Path
    warm_runs: int = 10
    environment: dict[str, str] = field(init=False)

    def __post_init__(self) -> None:
        self.root = self.root.resolve()
        self.buildq = self.buildq.resolve()
        self.environment = isolated_environment(self.root)

    @property
    def logs(self) -> Path:
        return self.root / "logs"

    def git(self, *arguments: str) -> None:
        subprocess.run(
            [
                "git",
                "-C",
                str(self.root / "wt1"),
                "-c",
                "user.name=benchmark",
                "-c",
                "user.email=benchmark@example.invalid",
                *arguments,
            ],
            check=True,
            stdout=subprocess.DEVNULL,
        )

    def prepare(self) -> None:
        self.root.mkdir(parents=True, exist_ok=False)
        shutil.copytree(FIXTURE, self.root / "wt1")
        (self.root / "state").mkdir()
        (self.root / "state/config.toml").write_text(CONFIG)
        # Dependency fetching is excluded from all compilation comparisons.
        with (self.root / "fetch.log").open("wb") as log:
            subprocess.run(
                ["cargo", "fetch", "--locked"],
                cwd=self.root / "wt1",
                env=self.environment,
                stdout=log,
                stderr=log,
                check=True,
            )

    def run(
        self,
        name: str,
        worktree: int,
        arguments: Sequence[str],
        *,
        queued: bool = True,
        extra_env: Mapping[str, str] | None = None,
        expected_exit: int = 0,
        expect_fresh: bool = False,
    ) -> Measurement:
        environment = self.environment | dict(extra_env or {})
        environment["BUILDQ_LABEL"] = name
        command = [str(self.buildq), "cargo"] if queued else ["cargo"]
        result = measure(
            name,
            self.root / f"wt{worktree}",
            self.logs,
            [*command, *arguments],
            environment,
        )
        if result.exit != expected_exit:
            raise BenchmarkError(
                f"{name}: expected exit {expected_exit}, got {result.exit}; "
                f"see {self.logs / (name + '.stderr')}"
            )
        if expect_fresh and result.compiling_lines:
            raise BenchmarkError(f"{name}: unexpected recompilation")
        return result

    def expect_output(self, name: str, marker: str) -> None:
        expected = f"version-{marker}:entities=1"
        actual = (self.logs / f"{name}.stdout").read_text().strip()
        if actual != expected:
            raise BenchmarkError(f"{name}: expected {expected!r}, got {actual!r}")

    def cold_and_warm_builds(self) -> None:
        self.run(
            "baseline-cold",
            1,
            [*BUILD_ARGUMENTS, "--target-dir", str(self.root / "baseline")],
            queued=False,
        )
        self.git("init", "-q")
        self.git("add", ".")
        self.git("commit", "-qm", "Pinned Bevy heavy probe")
        for index in range(2, 5):
            self.git("worktree", "add", "--detach", str(self.root / f"wt{index}"))
        self.run("buildq-cold", 1, BUILD_ARGUMENTS)
        self.warm_builds()
        self.run("worktree2-first", 2, BUILD_ARGUMENTS, expect_fresh=True)

    def warm_builds(self) -> None:
        for index in range(self.warm_runs):
            self.run(
                f"direct-warm-{index}",
                1,
                [*BUILD_ARGUMENTS, "--target-dir", str(self.root / "baseline")],
                queued=False,
                expect_fresh=True,
            )
            self.run(f"buildq-warm-{index}", 1, BUILD_ARGUMENTS, expect_fresh=True)

    def source_invalidation(self) -> None:
        source = self.root / "wt2/src/main.rs"
        original = source.read_text()
        if "version-A" not in original:
            raise BenchmarkError("The fixture must initially print version-A")
        try:
            source.write_text(original.replace("version-A", "version-B"))
            os.utime(source, (OLD_MTIME_SECONDS, OLD_MTIME_SECONDS))
            self.run(
                "naive-old-mtime",
                2,
                [*RUN_ARGUMENTS, "--target-dir", str(self.root / "baseline")],
                queued=False,
            )
            self.run("buildq-old-mtime", 2, RUN_ARGUMENTS)
            self.expect_output("buildq-old-mtime", "B")
            self.run("worktree1-return", 1, RUN_ARGUMENTS)
            self.expect_output("worktree1-return", "A")
            source.write_text(original + "\nthis is deliberately invalid Rust\n")
            self.run("compile-failure", 2, BUILD_ARGUMENTS, expected_exit=101)
            source.write_text(original)
            self.run("after-failure", 2, RUN_ARGUMENTS)
            self.expect_output("after-failure", "A")
        finally:
            source.write_text(original)

    def validate_wave(self, name: str, cache: Path) -> None:
        history = self.root / "state/history.jsonl"
        entries = [json.loads(line) for line in history.read_text().splitlines()]
        jobs = [
            job
            for job in entries
            if (job.get("label") or "").startswith(name + "-")
            and job["class"] == "build"
        ]
        if len(jobs) != 3:
            raise BenchmarkError(f"{name}: expected 3 history records, got {len(jobs)}")
        if maximum_overlap(jobs) > 2:
            raise BenchmarkError(f"{name}: exceeded 2 simultaneous build jobs")
        if len(list(cache.glob("pools/*/m[0-9]*.json"))) > 2:
            raise BenchmarkError(f"{name}: exceeded 2 pool members")

    def wave(
        self,
        name: str,
        *,
        queued: bool = True,
        extra_env: Mapping[str, str] | None = None,
        expect_fresh: bool = False,
    ) -> None:
        started = time.monotonic()
        with (
            ProcessSampler(os.getpid(), include_root=False) as sampler,
            ThreadPoolExecutor(max_workers=3) as workers,
        ):
            jobs: list[Future[Measurement]] = []
            for index in range(2, 5):
                arguments = list(BUILD_ARGUMENTS)
                if not queued:
                    arguments.extend(
                        ["--target-dir", str(self.root / f"direct-{index}")]
                    )
                jobs.append(
                    workers.submit(
                        self.run,
                        f"{name}-{index}",
                        index,
                        arguments,
                        queued=queued,
                        extra_env=extra_env,
                        expect_fresh=expect_fresh,
                    )
                )
                time.sleep(0.03)
            results = [job.result() for job in jobs]
            elapsed = time.monotonic() - started
        if queued:
            cache = Path((extra_env or {}).get("BUILDQ_CACHE", self.root / "cache"))
            self.validate_wave(name, cache)
        record = {
            "wall_s": elapsed,
            "peak_tree_rss_bytes": sampler.stats.peak_rss_bytes,
            "peak_rustc_processes": sampler.stats.peak_rustc_processes,
            "jobs": [result.to_dict() for result in results],
        }
        (self.root / f"{name}-wave.json").write_text(
            json.dumps(record, indent=2) + "\n"
        )

    def concurrency(self) -> None:
        self.wave("pooled-concurrent")
        self.wave("pooled-warm-concurrent", expect_fresh=True)
        self.wave("direct-concurrent", queued=False)
        self.wave(
            "pooled-all-cold",
            extra_env={"BUILDQ_CACHE": str(self.root / "cache-all-cold")},
        )

    def run_overlap(self) -> None:
        name = "sleeping-app"
        with ThreadPoolExecutor(max_workers=1) as worker:
            app = worker.submit(
                self.run, name, 1, RUN_ARGUMENTS, extra_env={"PROBE_SLEEP_MS": "5000"}
            )
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                status = subprocess.check_output(
                    [str(self.buildq), "status", "--json"], env=self.environment
                )
                if has_run_holder(json.loads(status), name):
                    break
                if app.done():
                    app.result()
                    raise BenchmarkError(
                        "Application exited before its run slot was seen"
                    )
                time.sleep(0.05)
            else:
                raise BenchmarkError(
                    "Application did not acquire a run slot within 30 s"
                )
            (self.root / "run-overlap-status.json").write_bytes(status)
            self.run("build-during-app", 2, BUILD_ARGUMENTS)
            if app.done():
                raise BenchmarkError("Build must finish while the app is still running")
            app.result()
        self.expect_output(name, "A")

    def dynamic_linking(self) -> None:
        # Resolve and fetch optional dependencies outside the timed run.
        with (self.root / "dynamic-metadata.json").open("wb") as metadata:
            subprocess.run(
                [
                    "cargo",
                    "metadata",
                    "--format-version=1",
                    "--features",
                    "bevy/dynamic_linking",
                ],
                cwd=self.root / "wt1",
                env=self.environment,
                stdout=metadata,
                check=True,
            )
        self.run(
            "bevy-dynamic-linking",
            1,
            [*RUN_ARGUMENTS, "--features", "bevy/dynamic_linking"],
        )
        self.expect_output("bevy-dynamic-linking", "A")

    def shim(self) -> None:
        shim_dir = self.root / "shim"
        subprocess.run(
            [str(self.buildq), "shim", "install", "cargo", "--into", str(shim_dir)],
            env=self.environment,
            check=True,
        )
        environment = {"PATH": str(shim_dir) + os.pathsep + self.environment["PATH"]}
        for index in range(self.warm_runs):
            self.run(
                f"shim-warm-{index}",
                4,
                BUILD_ARGUMENTS,
                queued=False,
                extra_env=environment,
                expect_fresh=True,
            )
        self.run(
            "actual-shim-run", 4, RUN_ARGUMENTS, queued=False, extra_env=environment
        )
        self.expect_output("actual-shim-run", "A")

    def save_inventory(self) -> None:
        for arguments, name in [
            (["cache", "ls", "--refresh", "--json"], "cache-inventory"),
            (["history", "-n", "200", "--json"], "history"),
        ]:
            content = subprocess.check_output(
                [str(self.buildq), *arguments], env=self.environment
            )
            (self.root / f"{name}.json").write_bytes(content)

    def execute(self) -> None:
        self.prepare()
        self.cold_and_warm_builds()
        self.source_invalidation()
        self.concurrency()
        self.run_overlap()
        self.dynamic_linking()
        self.shim()
        self.save_inventory()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True, help="New output directory")
    parser.add_argument(
        "--buildq", type=Path, required=True, help="Release buildq binary"
    )
    parser.add_argument("--warm-runs", type=int, default=10)
    options = parser.parse_args()
    if options.root.exists():
        parser.error("--root must not exist; existing caches are never deleted")
    if not options.buildq.is_file() or not os.access(options.buildq, os.X_OK):
        parser.error("--buildq must point to an existing executable")
    if options.warm_runs < 1:
        parser.error("--warm-runs must be at least 1")
    benchmark = Benchmark(options.root, options.buildq, options.warm_runs)
    benchmark.execute()
    print(f"Results: {benchmark.root}", flush=True)
    return 0
