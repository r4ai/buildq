#!/usr/bin/env python3
"""Compare six concurrent Bevy builds with isolated targets and stores.

Requires psutil and the repository's buildq_bench module. Prepare a pinned
fixture with Cargo.toml, Cargo.lock, src/main.rs, and fetch dependencies first.
Every trial uses a new directory; only targets created by this invocation are
removed after collecting inventory (unless --keep-targets is supplied).
"""

import argparse
import concurrent.futures
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "mbx"))
from qa_support import cleanup, stop_owned
from qa_support import popen as owned_popen
from qa_support import run as owned_run

from buildq_bench.measurement import OUTPUT_LOCK, Measurement, ProcessSampler


def command(args, **kwargs):
    return owned_run(
        args, stdout=subprocess.PIPE, check=True, timeout=60, **kwargs
    ).stdout.decode()


def measure(name, cwd, logs, command, env=None, *, timeout=1800):
    """Preserve the published schema and wait-time boundary with owned cleanup."""
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
        process = owned_popen(command, cwd=cwd, env=env, stdout=stdout, stderr=stderr)
        with ProcessSampler(process.pid) as sampler:
            try:
                process.wait(timeout=timeout)
                # Same boundary as buildq_bench.measurement.measure: stop the
                # wall clock immediately after wait, before sampler teardown.
                wall_seconds = time.monotonic() - started
            except BaseException:
                stop_owned(process)
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


def inventory(roots):
    seen = set()
    allocated = logical = files = aliases = 0
    for root in roots:
        for base, _, names in os.walk(root):
            for name in names:
                path = Path(base) / name
                if path.is_symlink():
                    continue
                stat = path.stat()
                key = (stat.st_dev, stat.st_ino)
                files += 1
                if key in seen:
                    aliases += 1
                    continue
                seen.add(key)
                allocated += stat.st_blocks * 512
                logical += stat.st_size
    return dict(
        allocated=allocated,
        logical=logical,
        files=files,
        hardlink_aliases=aliases,
        unique_inodes=len(seen),
    )


def fixture_source_hash(root):
    sources = [root / "Cargo.toml", *sorted((root / "src").rglob("*"))]
    fingerprints = {
        str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
        for path in sources
        if path.is_file()
    }
    return hashlib.sha256(json.dumps(fingerprints, sort_keys=True).encode()).hexdigest()


def mode_config(mode, mbx):
    slots = 4 if mode == "combined4" else 2
    backend = "mbx" if mode.startswith("combined") else "cargo"
    return f'''slots = {slots}
poll_ms = 25
[cache]
max_members = {slots}
budget = "16G"
min_free = "1G"
gc_after_job = false
[cargo]
jobs = 2
backend = "{backend}"
mbx = "{mbx}"
'''


def remove_owned_tree(path):
    # mbx generated-source directories can intentionally be read-only.
    for directory, _, _ in os.walk(path):
        os.chmod(directory, 0o700)
    shutil.rmtree(path)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--buildq", type=Path, required=True)
    parser.add_argument("--mbx", type=Path, required=True)
    parser.add_argument("--trials", type=int, default=2)
    parser.add_argument("--clients", type=int, default=6)
    parser.add_argument(
        "--modes",
        nargs="+",
        default=["plain", "buildq2", "mbx", "combined2", "combined4"],
    )
    parser.add_argument("--keep-targets", action="store_true")
    parser.add_argument("--resume", action="store_true")
    parser.add_argument("--timeout", type=float, default=1800)
    args = parser.parse_args()
    args.root = args.root.resolve()
    if args.resume:
        if not (args.root / "provenance.json").is_file():
            parser.error("--resume requires a root created by this runner")
    else:
        args.root.mkdir(parents=True, exist_ok=False)
    args.buildq = args.buildq.resolve()
    args.mbx = args.mbx.resolve()
    cargo = shutil.which("cargo")
    environment = {
        k: v
        for k, v in os.environ.items()
        if not k.startswith(("BUILDQ_", "MBX_"))
        and k not in {"CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR", "RUSTC_WRAPPER"}
    }
    environment.update(
        CARGO_BUILD_JOBS="2",
        CARGO_INCREMENTAL="1",
        MBX_TARGET_VIEWS="0",
        MBX_TARGET_LANES="0",
        MBX_GC_AUTO="0",
        MBX_REMOTE_URL="",
        MBX_DISPLAY="plain",
        MBX_SUMMARY="full",
        MBX_SCHEDULER_CPUS="4",
        MBX_SCHEDULER_MEMORY="12GiB",
        CARGO=cargo,
    )
    provenance = dict(
        buildq_sha=command(["git", "rev-parse", "HEAD"]).strip(),
        buildq_sha256=hashlib.sha256(args.buildq.read_bytes()).hexdigest(),
        mbx_sha256=hashlib.sha256(args.mbx.read_bytes()).hexdigest(),
        rustc=command(["rustc", "-Vv"]),
        cargo=command([cargo, "-V"]),
        mbx=command([str(args.mbx), "--version"]),
        fixture_lock_sha256=hashlib.sha256(
            (args.fixture / "Cargo.lock").read_bytes()
        ).hexdigest(),
        clients=args.clients,
        trials=args.trials,
        fixture_source_sha256=fixture_source_hash(args.fixture),
        modes=list(args.modes),
        controlled_environment={
            key: value
            for key, value in environment.items()
            if key.startswith("MBX_")
            or key in {"CARGO", "CARGO_BUILD_JOBS", "CARGO_INCREMENTAL"}
        },
        started_utc=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    )
    provenance_path = args.root / "provenance.json"
    if args.resume:
        previous = json.loads(provenance_path.read_text())
        results = json.loads((args.root / "results.json").read_text())
        for key in (
            "buildq_sha",
            "buildq_sha256",
            "mbx_sha256",
            "rustc",
            "cargo",
            "fixture_lock_sha256",
            "clients",
            "trials",
        ):
            if previous[key] != provenance[key]:
                parser.error(f"--resume provenance differs: {key}")
        missing = []
        for key in ("fixture_source_sha256", "modes", "controlled_environment"):
            if key in previous:
                if previous[key] != provenance[key]:
                    parser.error(f"--resume provenance differs: {key}")
            else:
                missing.append(key)
        if "fixture_source_sha256" in missing:
            snapshots = sorted(args.root.glob("trial*-*/wt0"))
            if (
                not snapshots
                or fixture_source_hash(snapshots[0])
                != provenance["fixture_source_sha256"]
            ):
                parser.error(
                    "--resume fixture sources differ from the saved wt0 snapshot"
                )
        if "modes" in missing:
            if not {record["mode"] for record in results}.issubset(args.modes):
                parser.error("--resume modes omit a previously measured mode")
        if "controlled_environment" in missing:
            for config_path in args.root.glob("trial*-*/state/config.toml"):
                mode = config_path.parents[1].name.split("-", 1)[1]
                if config_path.read_text() != mode_config(mode, args.mbx):
                    parser.error(f"--resume saved mode configuration differs: {mode}")
        if missing:
            # Keep original provenance intact: old metadata did not record the
            # original unexecuted mode plan or all controlled environment values.
            # Saved fixture sources/configs provide a compatible narrower check.
            (args.root / "resume-verification.json").write_text(
                json.dumps(
                    {
                        "missing_from_original_provenance": missing,
                        "fixture_source_sha256": provenance["fixture_source_sha256"],
                        "requested_modes": args.modes,
                        "current_controlled_environment": provenance[
                            "controlled_environment"
                        ],
                        "verification": (
                            "saved wt0 source hash, saved per-mode config, and "
                            "measured mode membership; original unexecuted mode "
                            "plan/environment unavailable"
                        ),
                    },
                    indent=2,
                )
                + "\n"
            )
            print(
                "Resume: verified saved source/config snapshots for legacy provenance; "
                "original unexecuted mode plan/environment were not recorded",
                file=sys.stderr,
            )
    else:
        provenance_path.write_text(json.dumps(provenance, indent=2) + "\n")
        results = []
    for trial in range(args.trials):
        # Reverse order on the second trial to expose order effects.
        modes = args.modes if trial % 2 == 0 else list(reversed(args.modes))
        for mode in modes:
            root = args.root / f"trial{trial + 1}-{mode}"
            existing = {
                record["phase"]
                for record in results
                if record["trial"] == trial + 1 and record["mode"] == mode
            }
            if existing == {"cold", "warm1", "warm2"}:
                if not args.keep_targets:
                    for path in [
                        root / "pool",
                        root / "mbx-cache",
                        *[root / f"target{i}" for i in range(args.clients)],
                    ]:
                        if path.exists():
                            remove_owned_tree(path)
                print(f"Resume: completed trial {trial + 1} {mode}", flush=True)
                continue
            root.mkdir()
            first = root / "wt0"
            shutil.copytree(
                args.fixture, first, ignore=shutil.ignore_patterns("target", ".git")
            )
            command(["git", "init", "-q", str(first)])
            command(["git", "-C", str(first), "add", "."])
            command(
                [
                    "git",
                    "-C",
                    str(first),
                    "-c",
                    "user.name=QA",
                    "-c",
                    "user.email=qa@example.invalid",
                    "commit",
                    "-qm",
                    "Pinned Bevy probe",
                ]
            )
            for i in range(1, args.clients):
                command(
                    [
                        "git",
                        "-C",
                        str(first),
                        "worktree",
                        "add",
                        "--detach",
                        str(root / f"wt{i}"),
                    ],
                    stderr=subprocess.DEVNULL,
                )
            (root / "state").mkdir()
            config = mode_config(mode, args.mbx)
            (root / "state/config.toml").write_text(config)
            env = environment | dict(
                BUILDQ_DIR=str(root / "state"),
                BUILDQ_CACHE=str(root / "pool"),
                MBX_CACHE_DIR=str(root / "mbx-cache"),
                BUILDQ_TRACE="1",
            )
            pooled = mode.startswith(("buildq", "combined"))

            def one(i, phase, *, env=env, pooled=pooled, mode=mode, root=root):
                local = env | {"BUILDQ_LABEL": f"{phase}-{i}"}
                cmd = (
                    [str(args.buildq), "cargo"]
                    if pooled
                    else [str(args.mbx) if mode == "mbx" else cargo]
                )
                cmd += ["build", "--offline", "--locked", "--message-format=json"]
                if not pooled:
                    cmd += ["--target-dir", str(root / f"target{i}")]
                measured = measure(
                    f"{phase}-{i}",
                    root / f"wt{i}",
                    root / "logs",
                    cmd,
                    local,
                    timeout=args.timeout,
                )
                if measured.exit != 0:
                    raise RuntimeError(
                        f"{mode} {phase}-{i} exit {measured.exit}; see logs"
                    )
                messages = []
                for line in (
                    (root / "logs" / f"{phase}-{i}.stdout").read_text().splitlines()
                ):
                    try:
                        messages.append(json.loads(line))
                    except json.JSONDecodeError:
                        pass
                executables = [
                    m["executable"]
                    for m in messages
                    if m.get("reason") == "compiler-artifact"
                    and m.get("executable")
                    and "bevy-heavy-probe" in m.get("package_id", "")
                ]
                if not executables:
                    raise RuntimeError(f"no executable: {mode}/{phase}/{i}")
                output = command([executables[-1]], cwd=root / f"wt{i}").strip()
                if output != "version-A:entities=1":
                    raise RuntimeError(f"incorrect output {output}")
                return measured.to_dict() | dict(
                    output=output,
                    fresh=sum(m.get("fresh", False) for m in messages),
                    nonfresh=sum(
                        not m.get("fresh", False)
                        for m in messages
                        if m.get("reason") == "compiler-artifact"
                    ),
                )

            for phase in ("cold", "warm1", "warm2"):
                started = time.monotonic()
                with ProcessSampler(os.getpid(), include_root=False) as sampler:
                    with concurrent.futures.ThreadPoolExecutor(
                        max_workers=args.clients
                    ) as workers:
                        try:
                            jobs = list(
                                workers.map(
                                    lambda i, phase=phase: one(i, phase),
                                    range(args.clients),
                                )
                            )
                        except BaseException:
                            # SIGINT/TERM arrive on the main thread. Stop all
                            # owned sessions before the executor joins workers.
                            cleanup()
                            raise
                record = dict(
                    trial=trial + 1,
                    mode=mode,
                    phase=phase,
                    wall_s=time.monotonic() - started,
                    jobs=jobs,
                    peak_rss_bytes=sampler.stats.peak_rss_bytes,
                    peak_rustc=sampler.stats.peak_rustc_processes,
                    sampled_cpu_s=sampler.stats.sampled_cpu_seconds,
                    storage=inventory(
                        [
                            root / "pool",
                            root / "mbx-cache",
                            *[root / f"target{i}" for i in range(args.clients)],
                        ]
                    ),
                )
                results.append(record)
                (args.root / "results.json").write_text(
                    json.dumps(results, indent=2) + "\n"
                )
                print(
                    json.dumps({k: v for k, v in record.items() if k != "jobs"}),
                    flush=True,
                )
            for cmd, filename in [
                (["cache", "ls", "--refresh", "--json"], "buildq-cache"),
                (["history", "-n", "100", "--json"], "history"),
            ]:
                if pooled:
                    (root / (filename + ".json")).write_text(
                        command([str(args.buildq), *cmd], env=env)
                    )
            if mode != "plain" and mode != "buildq2":
                for cmd, filename in [
                    (["cache", "stats", "--json"], "mbx-cache"),
                    (["stats", "--json"], "mbx-stats"),
                ]:
                    p = owned_run(
                        [str(args.mbx), *cmd],
                        env=env,
                        capture_output=True,
                        text=True,
                        timeout=60,
                    )
                    (root / (filename + ".json")).write_text(p.stdout)
                    (root / (filename + ".stderr")).write_text(p.stderr)
                    p.check_returncode()
            if not args.keep_targets:
                for path in [
                    root / "pool",
                    root / "mbx-cache",
                    *[root / f"target{i}" for i in range(args.clients)],
                ]:
                    if path.exists():
                        remove_owned_tree(path)


if __name__ == "__main__":
    main()
