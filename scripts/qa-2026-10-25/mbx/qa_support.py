"""Isolation and bounded cleanup shared by the disposable QA probes.

No pre-existing fixture root is accepted. Commands run in their own sessions;
cleanup signals only those sessions/descendants, with Linux PID start-time
checks so an exited PID cannot turn into an unrelated cleanup target.
"""

import atexit
import os
import signal
import subprocess
import threading
import time
from pathlib import Path

_owned = {}
_lock = threading.Lock()
_closing = threading.Event()


def fresh_paths(root, output):
    if root.is_symlink() or output.is_symlink():
        raise SystemExit("fixture and evidence paths must not be symlinks")
    root, output = root.resolve(), output.resolve()
    if root == output or root in output.parents or output in root.parents:
        raise SystemExit("fixture root and evidence output must be disjoint")
    if root.exists():
        raise SystemExit(f"refusing to reuse an existing fixture root: {root}")
    if output.exists() and (not output.is_dir() or any(output.iterdir())):
        raise SystemExit(f"refusing to overwrite existing evidence: {output}")
    root.mkdir(parents=True, exist_ok=False)
    output.mkdir(parents=True, exist_ok=True)
    return root, output


def _processes():
    records = {}
    for path in Path("/proc").glob("[0-9]*/stat"):
        try:
            fields = path.read_text().rsplit(")", 1)[1].split()
            records[int(path.parent.name)] = (
                int(fields[1]),
                int(fields[3]),
                fields[19],
            )
        except (OSError, ValueError, IndexError):
            continue
    return records


def _signal_identity(pid, started, kind):
    """Bind the signal to the checked Linux process, even if its PID exits."""
    descriptor = None
    try:
        if hasattr(os, "pidfd_open") and hasattr(signal, "pidfd_send_signal"):
            descriptor = os.pidfd_open(pid)
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        if fields[19] != started:
            return
        if descriptor is not None:
            signal.pidfd_send_signal(descriptor, kind)
        else:
            os.kill(pid, kind)
    except ProcessLookupError:
        pass
    except FileNotFoundError:
        pass
    finally:
        if descriptor is not None:
            os.close(descriptor)


def popen(*args, **kwargs):
    if kwargs.get("start_new_session") is False:
        raise ValueError("QA commands require their own session")
    kwargs["start_new_session"] = True
    # Serialize registration against cancellation so a worker cannot launch
    # an unregistered command after the main thread begins global cleanup.
    with _lock:
        if _closing.is_set():
            raise RuntimeError(
                "QA invocation is shutting down; no new commands allowed"
            )
        process = subprocess.Popen(*args, **kwargs)
        identity = _processes().get(process.pid)
        _owned[process.pid] = (process, identity[2] if identity else None)
    return process


def stop_owned(process):
    """Stop this invocation's session, including reparented descendants."""
    with _lock:
        owned = _owned.get(process.pid)
    if owned is None:
        return
    selected = {}
    if not Path("/proc").is_dir():
        # All supported probe runs are Linux. On other POSIX hosts, retain
        # session isolation and avoid signalling a process whose leader exited.
        if process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
    else:
        for _ in range(8):
            records = _processes()
            leader = records.get(process.pid)
            # Refuse a PID which was reused after our leader exited.
            if leader and owned[1] is not None and leader[2] != owned[1]:
                break
            before = len(selected)
            for pid, (parent, session, started) in records.items():
                if session == process.pid or (
                    parent in selected
                    and records.get(parent, (None, None, None))[2] == selected[parent]
                ):
                    selected[pid] = started
            for pid, started in list(selected.items()):
                current = records.get(pid)
                if current and current[2] == started:
                    _signal_identity(pid, started, signal.SIGSTOP)
            if len(selected) == before:
                break
        # Stopped children cannot fork past the scan. Revalidate identities
        # immediately before each kill and never signal the caller's session.
        records = _processes()
        for pid, started in selected.items():
            if (
                pid != os.getpid()
                and records.get(pid, (None, None, None))[2] == started
            ):
                _signal_identity(pid, started, signal.SIGKILL)
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        pass
    # SIGKILL delivery and parent reaping can complete in either order.
    # Wait for our recorded descendants as well, rather than returning while
    # a signalled child is briefly still executing under CPU contention.
    deadline = time.monotonic() + 3
    while selected and time.monotonic() < deadline:
        running = False
        for pid, started in selected.items():
            try:
                fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
                running |= fields[19] == started and fields[0] != "Z"
            except FileNotFoundError:
                pass
        if not running:
            break
        time.sleep(0.01)


def run(args, *, input=None, capture_output=False, timeout=None, check=False, **kwargs):
    if capture_output:
        if "stdout" in kwargs or "stderr" in kwargs:
            raise ValueError("capture_output conflicts with stdout/stderr")
        kwargs.update(stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if input is not None:
        kwargs["stdin"] = subprocess.PIPE
    process = popen(args, **kwargs)
    try:
        stdout, stderr = process.communicate(input=input, timeout=timeout)
    except BaseException:
        stop_owned(process)
        raise
    result = subprocess.CompletedProcess(args, process.returncode, stdout, stderr)
    if check:
        result.check_returncode()
    return result


def cleanup():
    with _lock:
        _closing.set()
        processes = [value[0] for value in _owned.values()]
    for process in reversed(processes):
        stop_owned(process)


atexit.register(cleanup)


def _terminate(*_):
    raise SystemExit(143)


# Direct script termination should take the same cleanup path as an exception.
if threading.current_thread() is threading.main_thread():
    signal.signal(signal.SIGTERM, _terminate)
