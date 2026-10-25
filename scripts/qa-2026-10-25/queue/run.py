#!/usr/bin/env python3
"""Exercise the real buildq CLI with many short process jobs (not rustc load).

--root must not already exist. Child cleanup runs on normal exit, exceptions,
SIGINT and SIGTERM. SIGKILL of the Python harness cannot run cleanup.
"""

import argparse
import atexit
import fcntl
import json
import os
import resource
import signal
import statistics
import subprocess
import sys
import time
from pathlib import Path


def event(path, kind, job):
    with open(path, "a") as stream:
        fcntl.flock(stream, fcntl.LOCK_EX)
        stream.write(
            json.dumps(
                {"time": time.monotonic(), "kind": kind, "job": job, "pid": os.getpid()}
            )
            + "\n"
        )
        stream.flush()


def alive(pid):
    try:
        return Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0] != "Z"
    except FileNotFoundError:
        return False


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--buildq", type=Path, default=Path("target/debug/buildq"))
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--clients", type=int, nargs="+", default=[8, 32, 128])
    parser.add_argument("--poll-ms", type=int, nargs="+", default=[250, 10])
    parser.add_argument("--seconds", type=float, default=0.12)
    args = parser.parse_args()
    args.buildq = args.buildq.resolve()
    root = args.root.resolve()
    root.mkdir(parents=True, exist_ok=False)
    # Track only processes started by this script; never scan/kill other QA jobs.
    owned_children = []

    def cleanup_children():
        for child in owned_children:
            if child.poll() is None:
                try:
                    # Each Popen owns a fresh session, so this group cannot include
                    # unrelated jobs. SIGKILL avoids the SIGTERM tree leak (#111).
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        deadline = time.monotonic() + 3
        for child in owned_children:
            try:
                child.wait(timeout=max(0, deadline - time.monotonic()))
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        for child in owned_children:
            try:
                child.wait(timeout=1)
            except subprocess.TimeoutExpired:
                pass

    def interrupted(signum, _frame):
        raise SystemExit(128 + signum)

    atexit.register(cleanup_children)
    signal.signal(signal.SIGTERM, interrupted)

    output = root / "summary.json"
    result = {
        "buildq_commit": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], text=True
        ).strip(),
        "measurement": "short Python CPU/IO/sleep jobs, not Rust compilation",
        "batches": [],
        "cancellation": {},
        "priority": {},
    }
    script = str(Path(__file__).resolve())
    repos = []
    for n in range(8):
        repo = root / "repos" / str(n)
        repo.mkdir(parents=True, exist_ok=True)
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        repos.append(repo)

    def config(name, slots, poll, kill_tree=True):
        state = root / name
        state.mkdir(exist_ok=False)
        (state / "config.toml").write_text(
            f"slots = {slots}\npoll_ms = {poll}\nmax_per_worktree = 0\n"
            f"kill_tree = {'true' if kill_tree else 'false'}\n"
            '[cache]\ngc_mode = "inline"\nmin_free = "0"\n'
        )
        env = dict(
            os.environ,
            BUILDQ_DIR=str(state),
            BUILDQ_CACHE=str(state / "cache"),
            BUILDQ_POOL="0",
            BUILDQ_QUIET="1",
        )
        env.pop("BUILDQ_HELD", None)
        return state, env

    def start(env, log, job, seconds, priority=0, gate=None):
        cmd = [
            str(args.buildq),
            "run",
            "-p",
            str(priority),
            "--label",
            job,
            "--",
            sys.executable,
            script,
            "payload",
            str(log),
            job,
            str(seconds),
        ]
        if gate:
            cmd.append(str(gate))
        child = subprocess.Popen(
            cmd,
            env=env,
            cwd=repos[int(job.split("-")[-1]) % 8],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        owned_children.append(child)
        return child

    def status(env):
        before = time.monotonic()
        obj = json.loads(
            subprocess.check_output(
                [str(args.buildq), "status", "--json"], env=env, text=True
            )
        )
        return obj, time.monotonic() - before

    def history(env):
        return json.loads(
            subprocess.check_output(
                [str(args.buildq), "history", "--json", "-n", "10000"],
                env=env,
                text=True,
            )
        )

    def queue_part(obj):
        return next(c for c in obj["classes"] if c["class"] == "build")

    def save():
        output.write_text(json.dumps(result, indent=2) + "\n")

    for poll in args.poll_ms:
        for clients in args.clients:
            name = f"batch-p{poll}-n{clients}"
            state, env = config(name, 4, poll)
            log = state / "events.jsonl"
            before = resource.getrusage(resource.RUSAGE_CHILDREN)
            started = time.monotonic()
            children = [
                start(env, log, f"batch-{i}", args.seconds) for i in range(clients)
            ]
            snapshot = []
            while any(p.poll() is None for p in children):
                obj, latency = status(env)
                q = queue_part(obj)
                snapshot.append(
                    {
                        "t": time.monotonic() - started,
                        "latency": latency,
                        "busy": sum(h["busy"] for h in q["holders"]),
                        "waiters": len(q["waiters"]),
                    }
                )
                time.sleep(0.1)
                if time.monotonic() - started > 240:
                    raise TimeoutError(name)
            elapsed = time.monotonic() - started
            exits = [p.wait() for p in children]
            after = resource.getrusage(resource.RUSAGE_CHILDREN)
            events = [json.loads(line) for line in log.read_text().splitlines()]
            concurrent = peak = 0
            for e in events:
                concurrent += 1 if e["kind"] == "start" else -1
                peak = max(peak, concurrent)
            hist = history(env)
            waits = [(h["started_ms"] - h["enqueued_ms"]) / 1000 for h in hist]
            item = {
                "name": name,
                "clients": clients,
                "slots": 4,
                "poll_ms": poll,
                "elapsed_seconds": elapsed,
                "payload_seconds": args.seconds,
                "aggregate_child_cpu_seconds": after.ru_utime
                + after.ru_stime
                - before.ru_utime
                - before.ru_stime,
                "exit_codes": sorted(set(exits)),
                "history_records": len(hist),
                "start_events": sum(e["kind"] == "start" for e in events),
                "end_events": sum(e["kind"] == "end" for e in events),
                "peak_payload_concurrency": peak,
                "wait_median_seconds": statistics.median(waits),
                "wait_max_seconds": max(waits),
                "status_max_latency_seconds": max(s["latency"] for s in snapshot),
                "peak_waiters_sampled": max(s["waiters"] for s in snapshot),
                "tickets_after": len(list((state / "build" / "tickets").glob("*"))),
            }
            (state / "status.json").write_text(json.dumps(snapshot, indent=2))
            result["batches"].append(item)
            save()
            print(json.dumps(item), flush=True)

    # Hold every slot so priority and cancellation do not race admission.
    state, env = config("priority", 1, 10)
    log, gate = state / "events.jsonl", state / "gate"
    holder = start(env, log, "holder-0", 0, gate=gate)
    deadline = time.monotonic() + 5
    while not log.exists():
        assert time.monotonic() < deadline
        time.sleep(0.01)
    queued = []
    for i, priority in enumerate([0, 0, 1, 1, 5, 0, 5, 1]):
        queued.append(start(env, log, f"priority-{i}", 0.03, priority))
        deadline = time.monotonic() + 5
        while len(queue_part(status(env)[0])["waiters"]) < len(queued):
            assert time.monotonic() < deadline
            time.sleep(0.01)
    expected = [x["label"] for x in queue_part(status(env)[0])["waiters"]]
    gate.touch()
    exits = [holder.wait(timeout=10)] + [p.wait(timeout=10) for p in queued]
    actual = [
        json.loads(line)["job"]
        for line in log.read_text().splitlines()
        if json.loads(line)["kind"] == "start"
        and json.loads(line)["job"].startswith("priority")
    ]
    result["priority"] = {
        "expected": expected,
        "actual": actual,
        "matches_priority_then_fifo": actual == expected,
        "exit_codes": sorted(set(exits)),
    }
    save()
    print(json.dumps(result["priority"]), flush=True)

    state, env = config("cancellation", 2, 10)
    log, gate = state / "events.jsonl", state / "gate"
    holders = [start(env, log, f"holder-{i}", 30, gate=gate) for i in range(2)]
    deadline = time.monotonic() + 10
    while not log.exists() or len(log.read_text().splitlines()) < 2:
        assert time.monotonic() < deadline
        time.sleep(0.02)
    waiters = [start(env, log, f"waiter-{i}", 0.03) for i in range(32)]
    deadline = time.monotonic() + 10
    while len(queue_part(status(env)[0])["waiters"]) < 32:
        assert time.monotonic() < deadline
        time.sleep(0.02)
    cancelled = []
    for i, p in enumerate(waiters):
        if i % 2 == 0:
            os.kill(p.pid, signal.SIGKILL if i % 4 == 0 else signal.SIGTERM)
            p.wait(timeout=5)
            cancelled.append(f"waiter-{i}")
    holder_pids = [json.loads(line)["pid"] for line in log.read_text().splitlines()]
    kill_at = time.monotonic()
    for p in holders:
        p.kill()
        p.wait(timeout=5)
    exits = [p.wait(timeout=15) for i, p in enumerate(waiters) if i % 2]
    after, _ = status(env)
    q = queue_part(after)
    events = [json.loads(line) for line in log.read_text().splitlines()]
    starts = [e["job"] for e in events if e["kind"] == "start"]
    recovery = start(env, log, "recovery-0", 0.01).wait(timeout=5)
    result["cancellation"] = {
        "queued_clients": 32,
        "queued_cancelled": 16,
        "cancelled_jobs_started": sorted(set(starts) & set(cancelled)),
        "survivor_exit_codes": sorted(set(exits)),
        "holder_children_running_after": [pid for pid in holder_pids if alive(pid)],
        "recovery_exit": recovery,
        "elapsed_after_holder_kill_seconds": time.monotonic() - kill_at,
        "busy_after": sum(h["busy"] for h in q["holders"]),
        "waiters_after": len(q["waiters"]),
        "tickets_after": len(list((state / "build" / "tickets").glob("*"))),
        "history_records": len(history(env)),
    }
    save()
    print(json.dumps(result["cancellation"]), flush=True)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "payload":
        log, job, seconds = sys.argv[2:5]
        event(log, "start", job)
        if len(sys.argv) > 5:
            while not Path(sys.argv[5]).exists():
                time.sleep(0.01)
        # Exercise a small amount of CPU and IO without hiding queue overhead.
        data = bytes((i * 17) % 256 for i in range(4096))
        Path(log).with_name(f"payload-{job}.bin").write_bytes(data)
        time.sleep(float(seconds))
        event(log, "end", job)
    else:
        main()
