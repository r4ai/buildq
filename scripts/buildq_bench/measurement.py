"""Measure a command without quantizing its completion time to the sample interval."""

import argparse
import json
import os
import signal
import subprocess
import threading
import time
from collections.abc import Mapping, Sequence
from contextlib import suppress
from dataclasses import asdict, dataclass
from pathlib import Path
from types import TracebackType
from typing import Self

import psutil

SAMPLE_INTERVAL_SECONDS = 0.05
OUTPUT_LOCK = threading.Lock()


@dataclass
class ProcessStats:
    """Simultaneous process-tree peaks; RSS includes shared pages more than once."""

    peak_rss_bytes: int = 0
    peak_processes: int = 0
    peak_rustc_processes: int = 0
    sampled_cpu_seconds: float = 0.0


class ProcessSampler:
    """Sample one process tree on a worker thread; stop promptly on context exit."""

    def __init__(self, root_pid: int, *, include_root: bool = True) -> None:
        self.stats = ProcessStats()
        self.include_root = include_root
        self._root_pid = root_pid
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._sample_until_stopped, daemon=True)
        self._cpu_by_process: dict[tuple[int, float], float] = {}
        self._error: Exception | None = None

    def __enter__(self) -> Self:
        self._thread.start()
        return self

    def __exit__(
        self,
        exception_type: type[BaseException] | None,
        exception: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        self._stop.set()
        self._thread.join()
        if self._error is not None and exception is None:
            raise RuntimeError("Process sampling failed") from self._error

    def _sample_until_stopped(self) -> None:
        try:
            while not self._stop.is_set():
                self._sample_once()
                self._stop.wait(SAMPLE_INTERVAL_SECONDS)
        except Exception as error:
            self._error = error

    def _sample_once(self) -> None:
        try:
            root = psutil.Process(self._root_pid)
            processes = root.children(recursive=True)
            if self.include_root:
                processes.insert(0, root)
        except (psutil.NoSuchProcess, psutil.AccessDenied):
            return

        rss_bytes = 0
        rustc_processes = 0
        for process in processes:
            try:
                with process.oneshot():
                    rss_bytes += process.memory_info().rss
                    rustc_processes += process.name() == "rustc"
                    cpu_time = process.cpu_times()
                    identity = (process.pid, process.create_time())
                    self._cpu_by_process[identity] = cpu_time.user + cpu_time.system
            except (psutil.NoSuchProcess, psutil.AccessDenied):
                continue

        self.stats.peak_rss_bytes = max(self.stats.peak_rss_bytes, rss_bytes)
        self.stats.peak_processes = max(self.stats.peak_processes, len(processes))
        self.stats.peak_rustc_processes = max(
            self.stats.peak_rustc_processes, rustc_processes
        )
        self.stats.sampled_cpu_seconds = sum(self._cpu_by_process.values())


@dataclass(frozen=True)
class Measurement:
    """One JSONL record, retaining the schema used by the published results."""

    name: str
    command: list[str]
    cwd: str
    wall_s: float
    exit: int
    peak_tree_rss_bytes: int
    sampled_cpu_s: float
    peak_processes: int
    compiling_lines: int

    def to_dict(self) -> dict[str, object]:
        return asdict(self)


def stop_command(process: subprocess.Popen[bytes]) -> None:
    """Clean up this command's process group when measurement is interrupted."""
    if process.poll() is not None:
        return
    if os.name == "posix":
        with suppress(ProcessLookupError):
            os.killpg(process.pid, signal.SIGTERM)
    else:
        process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        if os.name == "posix":
            with suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGKILL)
        else:
            process.kill()
        process.wait()


def measure(
    name: str,
    cwd: Path,
    logs: Path,
    command: Sequence[str],
    env: Mapping[str, str] | None = None,
) -> Measurement:
    """Run a command and save output, sampled resource usage, and exact wait time."""
    if not name or Path(name).name != name or name in {".", ".."}:
        raise ValueError("Measurement name must be a single nonempty filename")
    if not command:
        raise ValueError("A command is required")

    logs.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    with (
        (logs / f"{name}.stdout").open("wb") as stdout,
        (logs / f"{name}.stderr").open("wb") as stderr,
    ):
        process = subprocess.Popen(
            command,
            cwd=cwd,
            env=env,
            stdout=stdout,
            stderr=stderr,
            start_new_session=os.name == "posix",
        )
        with ProcessSampler(process.pid) as sampler:
            try:
                process.wait()
                wall_seconds = time.monotonic() - started
            except BaseException:
                stop_command(process)
                raise

    stderr_text = (logs / f"{name}.stderr").read_text(errors="replace")
    result = Measurement(
        name=name,
        command=list(command),
        cwd=str(cwd),
        wall_s=wall_seconds,
        exit=process.returncode,
        peak_tree_rss_bytes=sampler.stats.peak_rss_bytes,
        sampled_cpu_s=sampler.stats.sampled_cpu_seconds,
        peak_processes=sampler.stats.peak_processes,
        compiling_lines=sum(
            line.lstrip().startswith("Compiling ") for line in stderr_text.splitlines()
        ),
    )
    encoded = json.dumps(result.to_dict())
    with OUTPUT_LOCK:
        with (logs / "measurements.jsonl").open("a") as output:
            output.write(encoded + "\n")
        print(encoded, flush=True)
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("name")
    parser.add_argument("cwd", type=Path)
    parser.add_argument("logs", type=Path)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    options = parser.parse_args()
    command = options.command
    if command[:1] == ["--"]:
        command = command[1:]
    if not command:
        parser.error("a command is required after --")
    result = measure(options.name, options.cwd, options.logs, command)
    return result.exit
