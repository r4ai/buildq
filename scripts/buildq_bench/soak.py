"""Resumable, bounded soak runner that churns buildq's queue, pools, and GC.

Everything lives under ``--root``. Progress is an append-only JSONL log, so a
killed runner resumes with the original start time and the same schedule.
The top half of this module is pure logic (parsing, scheduling, invariants,
summaries) covered by unit tests; the bottom half drives real processes.
"""

import argparse
import json
import os
import random
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
from collections import Counter
from collections.abc import Iterable, Mapping, Sequence
from concurrent.futures import ThreadPoolExecutor
from contextlib import suppress
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path

from buildq_bench.measurement import ProcessSampler, stop_command

STDERR_TAIL_CHARS = 2000
DEAD_GROUP_WINDOW_SECONDS = 600.0
PROCESS_NAMES = frozenset({"buildq", "cargo", "rustc"})
INVARIANTS = (
    "exit_codes",
    "correct_outputs",
    "no_orphan_processes",
    "slot_limit",
    "no_stranded_tickets",
    "cancellation_cleanup",
    "gc_errors",
    "storage_policy",
    "accounting_matches_disk",
)
IDLE_OVER_BUDGET_STREAK = 3
DISK_TOLERANCE_RATIO = 1.25
DISK_TOLERANCE_BYTES = 256 * 1024 * 1024
_DURATION = re.compile(r"(\d+(?:\.\d+)?)([smhd])")
_UNITS = {"s": 1, "m": 60, "h": 3600, "d": 86400}
_SIZE = re.compile(r"(\d+(?:\.\d+)?)\s*([KMGT]?)(?:i?B)?", re.IGNORECASE)
_MARKER = re.compile(r'MARKER: &str = "([^"]*)"')


class SoakError(RuntimeError):
    """The runner cannot continue (bad input or unusable environment)."""


# --- pure logic -----------------------------------------------------------


def parse_duration(text: str) -> float:
    """Parse ``90s``, ``10m``, ``24h``, ``1d12h`` into seconds."""
    text = text.strip().lower()
    parts = _DURATION.findall(text)
    if not parts or "".join(n + u for n, u in parts) != text:
        raise ValueError(f"invalid duration: {text!r}")
    seconds = sum(float(n) * _UNITS[u] for n, u in parts)
    if seconds <= 0:
        raise ValueError("duration must be positive")
    return seconds


def parse_size(text: str) -> int:
    """Parse ``512M`` / ``20G`` into bytes (binary units)."""
    match = _SIZE.fullmatch(text.strip())
    if not match:
        raise ValueError(f"invalid size: {text!r}")
    power = " KMGT".index(match.group(2).upper() or " ")
    return int(float(match.group(1)) * 1024**power)


@dataclass(frozen=True)
class RepoSpec:
    """A pinned repository and the Cargo arguments that exercise it."""

    name: str
    url: str | None  # None means a generated local fixture
    ref: str | None
    packages: tuple[str, ...]
    test_packages: tuple[str, ...]
    feature_flags: tuple[str, ...]
    edit_file: str
    cancel_flags: tuple[str, ...] = ("--release",)

    @property
    def is_fixture(self) -> bool:
        return self.url is None


OSS_REPOS: Mapping[str, RepoSpec] = {
    spec.name: spec
    for spec in (
        RepoSpec(
            "bevy",
            "https://github.com/bevyengine/bevy",
            "v0.16.1",
            ("-p", "bevy_ecs"),
            ("-p", "bevy_ecs"),
            ("--features", "multi_threaded"),
            "crates/bevy_ecs/src/lib.rs",
        ),
        RepoSpec(
            "tokio",
            "https://github.com/tokio-rs/tokio",
            "tokio-1.45.1",
            ("-p", "tokio@1.45.1"),  # the old snapshot's graph can hold two tokios
            ("-p", "tokio@1.45.1"),
            ("--features", "full"),
            "tokio/src/lib.rs",
        ),
        RepoSpec(
            "ripgrep",
            "https://github.com/BurntSushi/ripgrep",
            "14.1.1",
            ("-p", "ripgrep"),
            ("-p", "ripgrep"),
            ("--no-default-features",),
            "crates/core/main.rs",
        ),
        RepoSpec(
            "rust-analyzer",
            "https://github.com/rust-lang/rust-analyzer",
            "2025-06-02",
            ("-p", "syntax"),
            ("-p", "syntax"),
            ("--no-default-features",),
            "crates/syntax/src/lib.rs",
        ),
    )
}


def fixture_spec(name: str) -> RepoSpec:
    return RepoSpec(
        name,
        None,
        None,
        ("-p", "soak-app"),
        (),
        ("--features", "extra"),
        "util/src/lib.rs",
        ("--features", "slow"),
    )


def resolve_repos(spec: str | None, *, local_fixtures: bool) -> list[RepoSpec]:
    """Turn ``--repos`` into specs; fixtures are named ``fixture-<id>``."""
    if local_fixtures:
        names = (spec or "fixture-a,fixture-b,fixture-c,fixture-d").split(",")
        if any(not re.fullmatch(r"fixture-[a-z0-9]+", n) for n in names):
            raise ValueError("--local-fixtures repos must be named fixture-<id>")
        return [fixture_spec(n) for n in names]
    names = (spec or ",".join(OSS_REPOS)).split(",")
    unknown = [n for n in names if n not in OSS_REPOS]
    if unknown:
        raise ValueError(f"unknown repos {unknown}; known: {sorted(OSS_REPOS)}")
    return [OSS_REPOS[n] for n in names]


KINDS = (
    "build",
    "build_release",
    "build_features",
    "test",
    "edit",
    "revert",
    "burst",
    "cancel",
    "gc",
    "gc_pressure",
)
_WEIGHTS = {
    "build": 5,
    "build_release": 3,
    "build_features": 3,
    "test": 4,
    "edit": 2,
    "revert": 0,
    "burst": 2,
    "cancel": 2,
    "gc": 0,
    "gc_pressure": 0,
}
BUILD_KINDS = ("build", "build_release", "build_features")
BURST_KINDS = ("build", "test", "build_release")


@dataclass(frozen=True)
class Action:
    index: int
    kind: str
    repo: str
    worktree: int
    burst_size: int = 0
    cancel_delay_s: float = 0.0
    cancel_signal: str = ""

    def to_dict(self) -> dict[str, object]:
        return {
            "index": self.index,
            "kind": self.kind,
            "repo": self.repo,
            "worktree": self.worktree,
            "burst_size": self.burst_size,
            "cancel_delay_s": self.cancel_delay_s,
            "cancel_signal": self.cancel_signal,
        }


def plan_action(
    seed: int,
    index: int,
    repos: Sequence[str],
    worktrees: int,
    dirty: frozenset[tuple[str, int]] = frozenset(),
    gc_every: int = 12,
) -> Action:
    """Pick action ``index`` from ``seed`` alone so a resume continues the schedule."""
    rng = random.Random(f"{seed}:{index}")
    repo = repos[rng.randrange(len(repos))]
    worktree = rng.randrange(worktrees)
    if gc_every > 0 and index % gc_every == gc_every - 1:
        kind = "gc_pressure" if (index // gc_every) % 2 else "gc"
        return Action(index, kind, repo, worktree)
    weights = dict(_WEIGHTS)
    if dirty:
        weights["revert"] = 6
        if (repo, worktree) not in dirty:
            repo, worktree = sorted(dirty)[rng.randrange(len(dirty))]
            if rng.random() < 0.5:
                return Action(index, "revert", repo, worktree)
    if (repo, worktree) in dirty:
        weights["edit"] = 0
    kind = rng.choices(KINDS, [weights[k] for k in KINDS])[0]
    return Action(
        index,
        kind,
        repo,
        worktree,
        burst_size=rng.randint(3, 8) if kind == "burst" else 0,
        cancel_delay_s=round(rng.uniform(1.0, 6.0), 1) if kind == "cancel" else 0.0,
        cancel_signal=rng.choice(["SIGTERM", "SIGINT"]) if kind == "cancel" else "",
    )


def cargo_arguments(repo: RepoSpec, kind: str) -> list[str]:
    """Arguments after ``buildq cargo`` for a build-like action kind."""
    if repo.is_fixture:
        run = ["run", "--offline", "-q", *repo.packages]
        build = ["build", "--offline", "-q", *repo.packages]
        test = ["test", "--offline", "-q"]
    else:
        run = build = ["build", *repo.packages]
        test = ["test", *repo.test_packages]
    if kind == "build":
        return run
    if kind == "build_release":
        return [*run, "--release"]
    if kind == "build_features":
        return [*run, *repo.feature_flags]
    if kind == "test":
        return test
    if kind == "cancel":
        return [*build, *repo.cancel_flags]
    raise ValueError(f"not a cargo action: {kind}")


def read_marker(source: str) -> str | None:
    match = _MARKER.search(source)
    return match.group(1) if match else None


def expected_fixture_output(marker: str, kind: str) -> str:
    profile = "release" if kind == "build_release" else "debug"
    extra = 1 if kind == "build_features" else 0
    return f"SOAK {marker} profile={profile} extra={extra}"


def output_matches(stdout: str, expected: str) -> bool:
    return [x for x in stdout.splitlines() if x.startswith("SOAK ")] == [expected]


@dataclass(frozen=True)
class Limits:
    slots: int
    stranded_seconds: float
    min_free_bytes: int = 0


def parse_ps(text: str) -> list[dict[str, object]]:
    """Parse ``ps -axo pid=,ppid=,pgid=,rss=,comm=`` output."""
    rows: list[dict[str, object]] = []
    for line in text.splitlines():
        fields = line.split(None, 4)
        if len(fields) < 5 or not all(f.isdigit() for f in fields[:4]):
            continue
        pid, ppid, pgid, rss = (int(f) for f in fields[:4])
        rows.append(
            {
                "pid": pid,
                "ppid": ppid,
                "pgid": pgid,
                "rss_kb": rss,
                "comm": Path(fields[4].strip()).name,
            }
        )
    return rows


def relevant_processes(
    rows: Iterable[Mapping[str, object]], groups: Iterable[int]
) -> list[dict[str, object]]:
    wanted = set(groups)
    return [dict(r) for r in rows if r["pgid"] in wanted or r["comm"] in PROCESS_NAMES]


def busy_holders(status: Mapping[str, object]) -> list[dict]:
    return [
        holder
        for group in status.get("classes", [])
        for holder in group["holders"]
        if holder["busy"]
    ]


def waiters(status: Mapping[str, object]) -> list[dict]:
    return [w for group in status.get("classes", []) for w in group["waiters"]]


def evaluate_sample(sample: Mapping[str, object], limits: Limits) -> list[dict]:
    """Return invariant violations visible in one sample."""
    violations: list[dict] = []
    processes = list(sample.get("processes", []))
    dead = set(sample.get("dead_groups", []))
    live = set(sample.get("live_groups", []))

    orphans = [p for p in processes if p["pgid"] in dead]
    if orphans:
        violations.append(
            {
                "invariant": "no_orphan_processes",
                "detail": f"{len(orphans)} process(es) outlive their buildq: "
                + ", ".join(f"{p['comm']}[{p['pid']}]" for p in orphans[:5]),
            }
        )

    status = sample.get("status")
    if not isinstance(status, dict):
        return violations
    now = float(sample["t"])
    for group in status["classes"]:
        held = [h for h in group["holders"] if h["busy"]]
        pids = [h["job"]["pid"] for h in held if h.get("job")]
        if len(held) > limits.slots or len(pids) != len(set(pids)):
            violations.append(
                {
                    "invariant": "slot_limit",
                    "detail": f"class {group['class']}: {len(held)} holders for "
                    f"{limits.slots} slots (pids {pids})",
                }
            )
        free_slot = len(held) < group["slots"]
        for waiter in group["waiters"]:
            age = now - waiter["enqueued_ms"] / 1000
            if age > limits.stranded_seconds and (
                free_slot or age > 3 * limits.stranded_seconds
            ):
                violations.append(
                    {
                        "invariant": "no_stranded_tickets",
                        "detail": f"pid {waiter['pid']} waited {age:.0f}s "
                        f"({'free slot' if free_slot else 'slots busy'})",
                    }
                )
    cargos = sum(1 for p in processes if p["comm"] == "cargo" and p["pgid"] in live)
    if cargos > 2 * limits.slots:  # one cargo per build slot plus one per run slot
        violations.append(
            {
                "invariant": "slot_limit",
                "detail": f"{cargos} cargo processes for {limits.slots} slots",
            }
        )
    return violations


def is_idle(status: object) -> bool:
    return isinstance(status, dict) and not busy_holders(status) and not waiters(status)


def idle_over_budget(sample: Mapping[str, object]) -> bool:
    cache = sample.get("cache")
    return (
        isinstance(cache, dict)
        and is_idle(sample.get("status"))
        and bool(cache.get("over_budget"))
    )


def accounting_mismatch(sample: Mapping[str, object]) -> str | None:
    """Compare ``du`` against buildq's own report while nothing is building."""
    cache, du = sample.get("cache"), sample.get("du_bytes")
    if not isinstance(cache, dict) or du is None or not is_idle(sample.get("status")):
        return None
    reported = int(cache.get("total", 0)) + int(cache.get("trash_bytes", 0))
    if du > reported * DISK_TOLERANCE_RATIO + DISK_TOLERANCE_BYTES:
        return f"du={du} bytes but buildq reports {reported} bytes"
    return None


def gc_problems(result: Mapping[str, object]) -> list[str]:
    """Problems in a ``buildq gc --json`` result that must stay visible."""
    problems: list[str] = []
    if result.get("failed_deletions"):
        problems.append(f"{result['failed_deletions']} failed deletion(s)")
    dedupe = result.get("dedupe")
    if isinstance(dedupe, dict) and dedupe.get("errors"):
        problems.append(f"dedupe errors: {dedupe['errors'][:3]}")
    return problems


def summarize(records: Sequence[Mapping[str, object]], now: float) -> dict[str, object]:
    """Fold the whole JSONL history into a pass/fail summary."""
    start = next((r for r in records if r["type"] == "start"), {})
    actions = [r for r in records if r["type"] == "action"]
    samples = [r for r in records if r["type"] == "sample"]
    history = [r for r in records if r["type"] == "history_stats"]
    found: dict[str, list[str]] = {name: [] for name in INVARIANTS}

    for a in actions:
        label = f"#{a['index']} {a['kind']} {a['repo']}"
        if a.get("expect_success") and a.get("exit") != 0:
            found["exit_codes"].append(f"{label}: exit {a.get('exit')}")
        if a.get("output_ok") is False:
            found["correct_outputs"].append(f"{label}: {a.get('output_note')}")
        if a.get("leftover_pids"):
            found["cancellation_cleanup"].append(
                f"{label}: leftover pids {a['leftover_pids']}"
            )
        if a.get("cancel_hung"):
            found["cancellation_cleanup"].append(f"{label}: ignored the signal")
        for problem in a.get("gc_problems", []):
            found["gc_errors"].append(f"{label}: {problem}")
    streak = 0
    for s in samples:
        for v in s.get("violations", []):
            found[v["invariant"]].append(f"t={s['t']}: {v['detail']}")
        if s.get("accounting_mismatch"):
            found["accounting_matches_disk"].append(
                f"t={s['t']}: {s['accounting_mismatch']}"
            )
        streak = streak + 1 if idle_over_budget(s) else 0
        if streak == IDLE_OVER_BUDGET_STREAK:
            reasons = s["cache"].get("over_budget_reasons")
            found["storage_policy"].append(
                f"t={s['t']}: over budget while idle for {streak} samples: {reasons}"
            )
    floor = int(start.get("min_free_bytes", 0))
    for s in samples:
        if (
            floor
            and s.get("idle")
            and s.get("free_disk") is not None
            and s["free_disk"] < floor
        ):
            found["storage_policy"].append(
                f"t={s['t']}: free {s['free_disk']} < min_free {floor} while idle"
            )

    free = [s["free_disk"] for s in samples if s.get("free_disk") is not None]
    started = float(start.get("t", now))
    return {
        "started": started,
        "ended": now,
        "elapsed_s": round(now - started, 1),
        "seed": start.get("seed"),
        "actions": len(actions),
        "actions_by_kind": dict(Counter(a["kind"] for a in actions)),
        "samples": len(samples),
        "passed": not any(found.values()),
        "invariants": {
            name: {
                "passed": not found[name],
                "violations": len(found[name]),
                "examples": found[name][:5],
            }
            for name in INVARIANTS
        },
        "min_free_disk_bytes": min(free, default=None),
        "max_du_bytes": max(
            (s["du_bytes"] for s in samples if s.get("du_bytes") is not None),
            default=None,
        ),
        "max_trash_bytes": max(
            (int((s.get("cache") or {}).get("trash_bytes", 0)) for s in samples),
            default=0,
        ),
        "max_tree_rss_kb": max(
            (sum(p["rss_kb"] for p in s.get("processes", [])) for s in samples),
            default=0,
        ),
        "queue_wait": history[-1]["stats"] if history else None,
        "gc_runs": sum(1 for a in actions if a["kind"] in {"gc", "gc_pressure"}),
    }


def dirty_after(records: Iterable[Mapping[str, object]]) -> frozenset[tuple[str, int]]:
    dirty: set[tuple[str, int]] = set()
    for r in records:
        if r["type"] != "action" or r.get("exit") != 0:
            continue
        key = (str(r["repo"]), int(r["worktree"]))  # type: ignore[call-overload]
        if r["kind"] == "edit":
            dirty.add(key)
        elif r["kind"] == "revert":
            dirty.discard(key)
    return frozenset(dirty)


def wait_stats(jobs: Iterable[Mapping[str, object]]) -> dict[str, float]:
    waits = sorted(
        (int(j["started_ms"]) - int(j["enqueued_ms"])) / 1000  # type: ignore[call-overload]
        for j in jobs
        if j.get("started_ms") and j.get("enqueued_ms")
    )
    if not waits:
        return {"jobs": 0, "max_wait_s": 0.0, "p95_wait_s": 0.0}
    return {
        "jobs": len(waits),
        "max_wait_s": waits[-1],
        "p95_wait_s": waits[min(len(waits) - 1, int(len(waits) * 0.95))],
    }


# --- fixtures ---------------------------------------------------------------

FIXTURE_FILES = {
    "Cargo.toml": '[workspace]\nmembers = ["util", "app"]\nresolver = "2"\n',
    "util/Cargo.toml": (
        '[package]\nname = "soak-util"\nversion = "0.1.0"\nedition = "2021"\n'
    ),
    "util/src/lib.rs": (
        'pub const MARKER: &str = "@MARKER@";\n'
        "pub fn marker() -> &'static str { MARKER }\n"
        "#[cfg(test)]\nmod tests {\n"
        "    #[test]\n"
        '    fn marker_is_tagged() { assert!(super::MARKER.starts_with("fixture")); }\n'
        "}\n"
    ),
    "app/Cargo.toml": (
        '[package]\nname = "soak-app"\nversion = "0.1.0"\nedition = "2021"\n'
        'build = "build.rs"\n\n[features]\nextra = []\nslow = []\n\n'
        '[dependencies]\nsoak-util = { path = "../util" }\n'
    ),
    "app/build.rs": (
        "fn main() {\n"
        '    println!("cargo:rerun-if-env-changed=SOAK_NONCE");\n'
        '    if std::env::var_os("CARGO_FEATURE_SLOW").is_some() {\n'
        "        std::thread::sleep(std::time::Duration::from_secs(10));\n"
        "    }\n}\n"
    ),
    "app/src/main.rs": (
        "fn main() {\n"
        '    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };\n'
        '    println!("SOAK {} profile={} extra={}", soak_util::marker(), profile,\n'
        '        cfg!(feature = "extra") as u8);\n}\n'
    ),
    ".gitignore": "target/\n",
}


# --- process driving --------------------------------------------------------


def ensure_inside(root: Path, path: Path) -> Path:
    resolved = path.resolve()
    if not resolved.is_relative_to(root.resolve()):
        raise SoakError(f"refusing to touch {resolved}: outside {root}")
    return resolved


def isolated_environment(root: Path) -> dict[str, str]:
    """Queue, cache, config, temp, and Cargo state all live under ``root``."""
    environment = {
        name: value
        for name, value in os.environ.items()
        if not name.startswith("BUILDQ_")
        and name not in {"CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"}
    }
    environment.update(
        BUILDQ_DIR=str(root / "state"),
        BUILDQ_CONFIG=str(root / "state/config.toml"),
        BUILDQ_CACHE=str(root / "cache"),
        BUILDQ_QUIET="1",
        CARGO_HOME=str(root / "cargo-home"),
        # ripgrep's tests assume TMPDIR is outside any Git checkout.
        TMPDIR=str(root / "tmp"),
        RUSTC_WRAPPER="",
    )
    return environment


def render_config(slots: int, budget: str, min_free: str) -> str:
    return (
        f"slots = {slots}\npoll_ms = 200\n\n"
        f'[cache]\nbudget = "{budget}"\nmin_free = "{min_free}"\n'
    )


class JsonlLog:
    """Append-only JSONL, one fsynced line per record."""

    def __init__(self, path: Path) -> None:
        self.path = path
        self._lock = threading.Lock()

    def append(self, record: Mapping[str, object]) -> None:
        line = json.dumps(record, sort_keys=True)
        with self._lock, self.path.open("a") as output:
            output.write(line + "\n")
            output.flush()
            os.fsync(output.fileno())

    def read(self) -> list[dict]:
        if not self.path.exists():
            return []
        records = []
        for line in self.path.read_text().splitlines():
            with suppress(json.JSONDecodeError):  # a torn last line after a crash
                records.append(json.loads(line))
        return records


class ProcessRegistry:
    """Track only the process groups this runner started."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._live: dict[int, subprocess.Popen[bytes]] = {}
        self._dead: dict[int, float] = {}

    def register(self, process: subprocess.Popen[bytes]) -> None:
        with self._lock:
            self._live[process.pid] = process

    def release(self, process: subprocess.Popen[bytes]) -> None:
        with self._lock:
            self._live.pop(process.pid, None)
            self._dead[process.pid] = time.monotonic()

    def groups(self) -> tuple[list[int], list[int]]:
        cutoff = time.monotonic() - DEAD_GROUP_WINDOW_SECONDS
        with self._lock:
            self._dead = {g: t for g, t in self._dead.items() if t >= cutoff}
            return sorted(self._live), sorted(self._dead)

    def terminate_all(self) -> None:
        with self._lock:
            live = list(self._live.values())
        for process in live:
            stop_command(process)


def process_rows() -> list[dict[str, object]]:
    result = subprocess.run(
        ["ps", "-axo", "pid=,ppid=,pgid=,rss=,comm="],
        capture_output=True,
        text=True,
        check=False,
    )
    return parse_ps(result.stdout)


def directory_kib(path: Path) -> int | None:
    if not path.exists():
        return 0
    result = subprocess.run(
        ["du", "-sk", str(path)], capture_output=True, text=True, check=False
    )
    first = result.stdout.split()
    return int(first[0]) if first and first[0].isdigit() else None


def page_cache_kib() -> int | None:
    with suppress(OSError):
        for line in Path("/proc/meminfo").read_text().splitlines():
            if line.startswith("Cached:"):
                return int(line.split()[1])
    return None


@dataclass
class JobResult:
    command: list[str]
    cwd: str
    exit: int
    duration_s: float
    stdout: str
    stderr: str
    peak_rss_bytes: int = 0
    signal_sent: str | None = None
    leftover_pids: list[int] = field(default_factory=list)
    hung: bool = False
    timed_out: bool = False


@dataclass
class Options:
    buildq: Path
    root: Path
    duration_s: float
    repos: list[RepoSpec]
    worktrees: int = 3
    budget: str = "20G"
    min_free: str = "5G"
    slots: int = 2
    seed: int = 0
    sample_interval_s: float = 60.0
    stranded_seconds: float = 1800.0
    job_timeout_s: float = 7200.0
    gc_every: int = 12
    local_fixtures: bool = False


class Soak:
    def __init__(self, options: Options) -> None:
        self.o = options
        self.root = options.root.resolve()
        self.buildq = options.buildq.resolve()
        self.env = isolated_environment(self.root)
        self.log = JsonlLog(self.root / "logs/soak.jsonl")
        self.registry = ProcessRegistry()
        self.stop = threading.Event()
        self.limits = Limits(
            options.slots, options.stranded_seconds, parse_size(options.min_free)
        )
        self.repos = {r.name: r for r in options.repos}

    # paths
    def clone_path(self, repo: str) -> Path:
        return self.root / "repos" / repo

    def worktree_path(self, repo: str, index: int) -> Path:
        return self.root / "worktrees" / f"{repo}-{index}"

    # git helpers (always inside root)
    def git(self, path: Path, *args: str) -> str:
        ensure_inside(self.root, path)
        result = subprocess.run(
            [
                "git",
                "-C",
                str(path),
                "-c",
                "user.name=soak",
                "-c",
                "user.email=soak@example.invalid",
                *args,
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode:
            raise SoakError(
                f"git {' '.join(args)} failed in {path}: {result.stderr.strip()}"
            )
        return result.stdout.strip()

    def revision(self, path: Path) -> dict[str, object]:
        with suppress(SoakError):
            return {
                "head": self.git(path, "rev-parse", "HEAD"),
                "dirty": bool(self.git(path, "status", "--porcelain")),
            }
        return {"head": None, "dirty": None}

    def prepare(self) -> None:
        for sub in (
            "state",
            "cache",
            "tmp",
            "logs/jobs",
            "repos",
            "worktrees",
            "cargo-home",
        ):
            (self.root / sub).mkdir(parents=True, exist_ok=True)
        config = self.root / "state/config.toml"
        config.write_text(render_config(self.o.slots, self.o.budget, self.o.min_free))
        for repo in self.repos.values():
            self.prepare_repo(repo)

    def prepare_repo(self, repo: RepoSpec) -> None:
        clone = self.clone_path(repo.name)
        if not clone.exists():
            if repo.is_fixture:
                for name, content in FIXTURE_FILES.items():
                    target = clone / name
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.write_text(content.replace("@MARKER@", f"{repo.name}/main"))
                subprocess.run(
                    ["git", "-C", str(clone), "init", "-q", "-b", "main"], check=True
                )
                self.git(clone, "add", "-A")
                self.git(clone, "commit", "-q", "-m", "fixture")
            else:
                subprocess.run(
                    [
                        "git",
                        "clone",
                        "--depth",
                        "1",
                        "--branch",
                        str(repo.ref),
                        str(repo.url),
                        str(clone),
                    ],
                    check=True,
                )
        for index in range(self.o.worktrees):
            path = self.worktree_path(repo.name, index)
            if path.exists():
                continue
            if repo.is_fixture:
                self.git(
                    clone, "worktree", "add", "-q", "-b", f"soak-{index}", str(path)
                )
                lib = path / repo.edit_file
                lib.write_text(
                    lib.read_text().replace(
                        f"{repo.name}/main", f"{repo.name}/wt{index}"
                    )
                )
                self.git(path, "commit", "-q", "-am", f"worktree {index} marker")
            else:
                self.git(clone, "worktree", "add", "-q", "--detach", str(path))

    # running jobs
    def run_job(
        self,
        name: str,
        command: Sequence[str],
        cwd: Path,
        *,
        env: Mapping[str, str] | None = None,
        cancel: tuple[float, str] | None = None,
    ) -> JobResult:
        jobs = self.root / "logs/jobs"
        environment = {**self.env, **(env or {})}
        started = time.monotonic()
        result = JobResult(list(command), str(cwd), -1, 0.0, "", "")
        with (
            (jobs / f"{name}.out").open("wb") as out,
            (jobs / f"{name}.err").open("wb") as err,
        ):
            process = subprocess.Popen(
                command,
                cwd=cwd,
                env=environment,
                stdout=out,
                stderr=err,
                start_new_session=True,
            )
            self.registry.register(process)
            with ProcessSampler(process.pid) as sampler:
                self.wait(process, result, started, cancel)
            result.peak_rss_bytes = sampler.stats.peak_rss_bytes
            result.exit = process.returncode
            result.leftover_pids = self.reap_group(process.pid)
            self.registry.release(process)
        result.duration_s = round(time.monotonic() - started, 3)
        result.stdout = (jobs / f"{name}.out").read_text(errors="replace")
        result.stderr = (jobs / f"{name}.err").read_text(errors="replace")
        return result

    def wait(
        self,
        process: subprocess.Popen[bytes],
        result: JobResult,
        started: float,
        cancel: tuple[float, str] | None,
    ) -> None:
        signalled_at: float | None = None
        while process.poll() is None:
            elapsed = time.monotonic() - started
            if cancel and result.signal_sent is None and elapsed >= cancel[0]:
                result.signal_sent = cancel[1]
                signalled_at = time.monotonic()
                send = os.killpg if cancel[1] == "SIGINT" else os.kill
                # A terminal's Ctrl-C reaches the whole foreground group; SIGTERM
                # goes to buildq alone, as `kill <pid>` would.
                send(process.pid, getattr(signal, cancel[1]))
            if signalled_at is not None and time.monotonic() - signalled_at > 60:
                result.hung = True
                stop_command(process)
            elif elapsed > self.o.job_timeout_s:
                result.timed_out = True
                stop_command(process)
            elif self.stop.is_set():
                stop_command(process)
            time.sleep(0.1)

    def reap_group(self, pgid: int) -> list[int]:
        """Pids still in the finished job's own group; kill them (they are ours)."""
        deadline = time.monotonic() + 15
        while True:
            left = [int(r["pid"]) for r in process_rows() if r["pgid"] == pgid]  # type: ignore[call-overload]
            if not left or time.monotonic() > deadline:
                break
            time.sleep(0.5)
        if left:
            with suppress(ProcessLookupError):
                os.killpg(pgid, signal.SIGKILL)
        return left

    # sampling
    def buildq_json(self, *args: str) -> object | None:
        try:
            result = subprocess.run(
                [str(self.buildq), *args],
                env=self.env,
                capture_output=True,
                text=True,
                timeout=120,
                check=False,
            )
            return json.loads(result.stdout) if result.returncode == 0 else None
        except (subprocess.TimeoutExpired, json.JSONDecodeError, OSError):
            return None

    def take_sample(self) -> dict[str, object]:
        live, dead = self.registry.groups()
        status = self.buildq_json("status", "--json")
        cache = self.buildq_json("cache", "ls", "--json")
        du = directory_kib(self.root / "cache")
        sample: dict[str, object] = {
            "type": "sample",
            "t": time.time(),
            "status": status,
            "cache": cache,
            "du_bytes": None if du is None else du * 1024,
            "free_disk": shutil.disk_usage(self.root).free,
            "idle": is_idle(status),
            "page_cache_kb": page_cache_kib(),
            "processes": relevant_processes(process_rows(), [*live, *dead]),
            "live_groups": live,
            "dead_groups": dead,
        }
        sample["violations"] = evaluate_sample(sample, self.limits)
        sample["accounting_mismatch"] = accounting_mismatch(sample)
        if status is None or cache is None:
            sample["note"] = "buildq status or cache ls was unavailable"
        return sample

    def sampler_loop(self) -> None:
        while not self.stop.is_set():
            self.log.append(self.take_sample())
            self.stop.wait(self.o.sample_interval_s)

    # actions
    def execute(self, action: Action) -> dict[str, object]:
        event: dict[str, object] = {
            "type": "action",
            "t": time.time(),
            "iso": datetime.now(UTC).isoformat(timespec="seconds"),
            **action.to_dict(),
            "config": {
                "slots": self.o.slots,
                "budget": self.o.budget,
                "min_free": self.o.min_free,
            },
            "env": {
                "TMPDIR": self.env["TMPDIR"],
                "BUILDQ_CONFIG": self.env["BUILDQ_CONFIG"],
            },
        }
        repo = self.repos[action.repo]
        path = self.worktree_path(action.repo, action.worktree)
        event["cwd"] = str(path)
        event["revision"] = self.revision(path)
        handler = {
            "edit": self.do_edit,
            "revert": self.do_revert,
            "burst": self.do_burst,
            "cancel": self.do_cancel,
            "gc": self.do_gc,
            "gc_pressure": self.do_gc,
        }.get(action.kind, self.do_cargo)
        event.update(handler(action, repo, path))
        return event

    def job_fields(self, result: JobResult) -> dict[str, object]:
        return {
            "cmd": result.command,
            "exit": result.exit,
            "duration_s": result.duration_s,
            "stderr_tail": result.stderr[-STDERR_TAIL_CHARS:],
            "peak_tree_rss_bytes": result.peak_rss_bytes,
            "leftover_pids": result.leftover_pids,
            "timed_out": result.timed_out,
        }

    def check_output(
        self,
        repo: RepoSpec,
        path: Path,
        kind: str,
        result: JobResult,
        marker: str | None,
    ) -> dict[str, object]:
        if not repo.is_fixture or kind not in BUILD_KINDS or result.exit != 0:
            return {}
        if marker is None:
            return {"output_ok": False, "output_note": "marker missing from source"}
        expected = expected_fixture_output(marker, kind)
        ok = output_matches(result.stdout, expected)
        return {
            "output_ok": ok,
            "output_note": None
            if ok
            else f"expected {expected!r}, got {result.stdout[-300:]!r}",
        }

    def current_marker(self, repo: RepoSpec, path: Path) -> str | None:
        return (
            read_marker((path / repo.edit_file).read_text())
            if repo.is_fixture
            else None
        )

    def do_cargo(self, action: Action, repo: RepoSpec, path: Path) -> dict[str, object]:
        command = [str(self.buildq), "cargo", *cargo_arguments(repo, action.kind)]
        marker = self.current_marker(repo, path)
        result = self.run_job(
            f"{action.index:06d}",
            command,
            path,
            env={"BUILDQ_LABEL": f"soak-{action.index}"},
        )
        interrupted = self.stop.is_set()
        return {
            **self.job_fields(result),
            "expect_success": not interrupted,
            **self.check_output(repo, path, action.kind, result, marker),
        }

    def do_burst(self, action: Action, repo: RepoSpec, path: Path) -> dict[str, object]:
        def submit(i: int) -> dict[str, object]:
            kind = BURST_KINDS[i % len(BURST_KINDS)]
            wt = self.worktree_path(repo.name, (action.worktree + i) % self.o.worktrees)
            marker = self.current_marker(repo, wt)
            command = [str(self.buildq), "cargo", *cargo_arguments(repo, kind)]
            result = self.run_job(
                f"{action.index:06d}-{i}",
                command,
                wt,
                env={"BUILDQ_LABEL": f"soak-{action.index}-{i}"},
            )
            return {
                "kind": kind,
                "cwd": str(wt),
                **self.job_fields(result),
                **self.check_output(repo, wt, kind, result, marker),
            }

        with ThreadPoolExecutor(max_workers=action.burst_size) as pool:
            jobs = list(pool.map(submit, range(action.burst_size)))
        failed = [j for j in jobs if j["exit"] != 0]
        mismatched = [j for j in jobs if j.get("output_ok") is False]
        event: dict[str, object] = {
            "jobs": jobs,
            "exit": 0 if not failed else 1,
            "duration_s": max(float(j["duration_s"]) for j in jobs),  # type: ignore[arg-type]
            "expect_success": not self.stop.is_set(),
            "leftover_pids": [p for j in jobs for p in j["leftover_pids"]],  # type: ignore[union-attr]
            "cmd": [str(self.buildq), "cargo", "...", f"x{action.burst_size}"],
        }
        if mismatched:
            event["output_ok"] = False
            event["output_note"] = (
                f"{len(mismatched)} burst job(s) printed a stale marker"
            )
        return event

    def do_cancel(
        self, action: Action, repo: RepoSpec, path: Path
    ) -> dict[str, object]:
        command = [str(self.buildq), "cargo", *cargo_arguments(repo, "cancel")]
        result = self.run_job(
            f"{action.index:06d}",
            command,
            path,
            env={
                "BUILDQ_LABEL": f"soak-{action.index}",
                "SOAK_NONCE": str(action.index),
            },
            cancel=(action.cancel_delay_s, action.cancel_signal),
        )
        return {
            **self.job_fields(result),
            "expect_success": False,
            "signal_sent": result.signal_sent,
            "cancel_hung": result.hung,
        }

    def do_gc(self, action: Action, repo: RepoSpec, path: Path) -> dict[str, object]:
        command = [str(self.buildq), "gc", "--json"]
        if action.kind == "gc_pressure":
            command += ["--budget", "1M"]  # evict everything idle: forces cold rebuilds
        result = self.run_job(f"{action.index:06d}", command, path)
        problems: list[str] = []
        summary: dict[str, object] = {}
        try:
            parsed = json.loads(result.stdout)
            problems = gc_problems(parsed)
            summary = {
                k: parsed.get(k)
                for k in (
                    "freed",
                    "before",
                    "after",
                    "over_budget",
                    "failed_deletions",
                    "warnings",
                    "skipped",
                )
            }
        except json.JSONDecodeError:
            problems.append("gc --json output was not valid JSON")
        return {
            **self.job_fields(result),
            "expect_success": not self.stop.is_set(),
            "gc": summary,
            "gc_problems": problems,
        }

    def do_edit(self, action: Action, repo: RepoSpec, path: Path) -> dict[str, object]:
        target = ensure_inside(self.root, path / repo.edit_file)
        text = target.read_text()
        if repo.is_fixture:
            marker = read_marker(text)
            text = text.replace(f'"{marker}"', f'"{marker}+edit{action.index}"', 1)
        else:
            text += f"\n// soak-edit {action.index}\n"
        target.write_text(text)
        return {
            "cmd": ["edit", str(target)],
            "exit": 0,
            "duration_s": 0.0,
            "expect_success": True,
        }

    def do_revert(
        self, action: Action, repo: RepoSpec, path: Path
    ) -> dict[str, object]:
        self.git(path, "checkout", "--", repo.edit_file)
        return {
            "cmd": ["git", "checkout", "--", repo.edit_file],
            "exit": 0,
            "duration_s": 0.0,
            "expect_success": True,
        }

    # main loop
    def start_record(self) -> dict[str, object]:
        version = subprocess.run(
            [str(self.buildq), "--version"], capture_output=True, text=True, check=False
        ).stdout.strip()
        return {
            "type": "start",
            "t": time.time(),
            "iso": datetime.now(UTC).isoformat(timespec="seconds"),
            "seed": self.o.seed,
            "duration_s": self.o.duration_s,
            "buildq": str(self.buildq),
            "buildq_version": version,
            "repos": [
                {"name": r.name, "url": r.url, "ref": r.ref} for r in self.o.repos
            ],
            "worktrees": self.o.worktrees,
            "slots": self.o.slots,
            "budget": self.o.budget,
            "min_free": self.o.min_free,
            "min_free_bytes": parse_size(self.o.min_free),
            "local_fixtures": self.o.local_fixtures,
            "host": {"platform": sys.platform, "python": sys.version.split()[0]},
        }

    def run(self) -> int:
        self.prepare()
        records = self.log.read()
        previous = next((r for r in records if r["type"] == "start"), None)
        if previous is None:
            previous = self.start_record()
            self.log.append(previous)
        else:
            if previous["seed"] != self.o.seed:
                raise SoakError(
                    f"log used seed {previous['seed']}; not resuming with {self.o.seed}"
                )
            self.log.append(
                {
                    "type": "resume",
                    "t": time.time(),
                    "actions_done": sum(r["type"] == "action" for r in records),
                }
            )
        deadline = float(previous["t"]) + self.o.duration_s
        dirty = set(dirty_after(records))
        index = 1 + max(
            (int(r["index"]) for r in records if r["type"] == "action"), default=-1
        )

        for signum in (signal.SIGINT, signal.SIGTERM):
            signal.signal(
                signum, lambda *_: (self.stop.set(), self.registry.terminate_all())
            )
        sampler = threading.Thread(target=self.sampler_loop, daemon=True)
        sampler.start()
        try:
            while not self.stop.is_set() and time.time() < deadline:
                action = plan_action(
                    self.o.seed,
                    index,
                    list(self.repos),
                    self.o.worktrees,
                    frozenset(dirty),
                    self.o.gc_every,
                )
                event = self.execute(action)
                self.log.append(event)
                if event.get("exit") == 0:
                    key = (action.repo, action.worktree)
                    if action.kind == "edit":
                        dirty.add(key)
                    elif action.kind == "revert":
                        dirty.discard(key)
                index += 1
        finally:
            self.stop.set()
            sampler.join()
            self.registry.terminate_all()
        return self.finish(interrupted=time.time() < deadline)

    def finish(self, *, interrupted: bool) -> int:
        self.log.append(self.take_sample())
        history = self.buildq_json("history", "-n", "100000", "--json")
        if isinstance(history, list):
            self.log.append(
                {
                    "type": "history_stats",
                    "t": time.time(),
                    "stats": wait_stats(history),
                }
            )
        summary = summarize(self.log.read(), time.time())
        summary["interrupted"] = interrupted
        (self.root / "logs/summary.json").write_text(
            json.dumps(summary, indent=2, sort_keys=True) + "\n"
        )
        print(json.dumps(summary, indent=2, sort_keys=True), flush=True)
        return 0 if summary["passed"] else 1


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--buildq", type=Path, required=True)
    parser.add_argument(
        "--root", type=Path, required=True, help="everything lives under this directory"
    )
    parser.add_argument(
        "--duration",
        default="24h",
        help="e.g. 10m, 24h, 1d12h (from the original start)",
    )
    parser.add_argument(
        "--repos", help="comma-separated names (default: the pinned OSS list)"
    )
    parser.add_argument(
        "--local-fixtures",
        action="store_true",
        help="offline generated Cargo workspaces",
    )
    parser.add_argument("--worktrees", type=int, default=3)
    parser.add_argument("--budget", default="20G")
    parser.add_argument("--min-free", default="5G")
    parser.add_argument("--slots", type=int, default=2)
    parser.add_argument("--seed", type=int, default=0)
    parser.add_argument("--sample-interval", default="60s")
    parser.add_argument(
        "--stranded-after", default="30m", help="waiter age that counts as stranded"
    )
    parser.add_argument("--job-timeout", default="2h")
    parser.add_argument(
        "--gc-every", type=int, default=12, help="every Nth action is a GC (0 disables)"
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        options = Options(
            buildq=args.buildq,
            root=args.root,
            duration_s=parse_duration(args.duration),
            repos=resolve_repos(args.repos, local_fixtures=args.local_fixtures),
            worktrees=args.worktrees,
            budget=args.budget,
            min_free=args.min_free,
            slots=args.slots,
            seed=args.seed,
            sample_interval_s=parse_duration(args.sample_interval),
            stranded_seconds=parse_duration(args.stranded_after),
            job_timeout_s=parse_duration(args.job_timeout),
            gc_every=args.gc_every,
            local_fixtures=args.local_fixtures,
        )
        parse_size(options.budget)
        parse_size(options.min_free)
        if options.worktrees < 1 or options.slots < 1:
            raise ValueError("--worktrees and --slots must be at least 1")
        if not options.buildq.is_file():
            raise ValueError(f"--buildq {options.buildq} is not a file")
        return Soak(options).run()
    except (ValueError, SoakError) as error:
        parser.error(str(error))
    return 2
