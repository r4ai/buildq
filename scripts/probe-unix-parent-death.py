#!/usr/bin/env python3
"""Reproduce a Unix queue-slot/child-lifetime mismatch with a real Cargo build.

Uses a new, isolated queue/cache under --root; does not signal unrelated processes.
Run after heavy benchmarks, as this intentionally leaves then cleans up children.
Requires psutil and the activated Rust toolchain.
"""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import time
import psutil

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--root', type=Path, required=True)
parser.add_argument('--workspace', type=Path, required=True)
parser.add_argument('--buildq', type=Path, required=True)
args = parser.parse_args()
root = args.root.resolve()
root.mkdir(exist_ok=False, parents=True)
(root / 'state').mkdir()
(root / 'state/config.toml').write_text('slots=1\npoll_ms=50\n[cache]\nmax_members=1\ngc_after_job=false\n')
env = {key: value for key, value in os.environ.items()
       if not key.startswith('BUILDQ_') and key != 'CARGO_TARGET_DIR'}
env.update(BUILDQ_DIR=str(root / 'state'), BUILDQ_CACHE=str(root / 'cache'),
           BUILDQ_GC='0', CARGO_BUILD_JOBS='2', RUSTC_WRAPPER='')
bq = str(args.buildq.resolve())
known = []
report = {}
try:
    with (root / 'holder.log').open('w') as log:
        holder = subprocess.Popen([bq, 'cargo', 'build', '--locked', '--offline'],
                                  cwd=args.workspace, env=env, stdout=log, stderr=log)
        known.append(psutil.Process(holder.pid))
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            children = known[0].children(recursive=True)
            known.extend(p for p in children if p not in known)
            cargos = []
            compiling = False
            for child in children:
                try:
                    if child.name() == 'cargo':
                        cargos.append(child)
                    if child.name() == 'rustc' and '--crate-name' in child.cmdline():
                        compiling = True
                except psutil.NoSuchProcess:
                    pass
            if cargos and compiling:
                break
            if holder.poll() is not None:
                raise RuntimeError('holder exited before compiling')
            time.sleep(.05)
        else:
            raise RuntimeError('no compiler observed')
        cargo = cargos[0]
        os.kill(holder.pid, signal.SIGKILL)
        holder.wait()
        # The replacement acquires the released buildq slot/member, but the
        # original Cargo is still using that same target directory.
        with (root / 'replacement.log').open('w') as replacement_log:
            replacement = subprocess.Popen([bq, 'cargo', 'build', '--locked', '--offline'],
                                           cwd=args.workspace, env=env,
                                           stdout=replacement_log, stderr=replacement_log)
            replacement_ps = psutil.Process(replacement.pid)
            known.append(replacement_ps)
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                known.extend(p for p in replacement_ps.children(recursive=True) if p not in known)
                if 'Blocking waiting for file lock' in (root / 'replacement.log').read_text():
                    break
                time.sleep(.05)
            status = json.loads(subprocess.check_output([bq, 'status', '--json'], env=env))
            report = {'holder_exit': holder.returncode, 'orphan_cargo_pid': cargo.pid,
                      'orphan_cargo_alive': cargo.is_running(),
                      'replacement_blocked_on_cargo_lock': 'Blocking waiting for file lock' in (root / 'replacement.log').read_text(),
                      'status': status}
finally:
    # Only the captured children of these two test parents are signalled.
    for process in list(known):
        try:
            known.extend(p for p in process.children(recursive=True) if p not in known)
        except psutil.NoSuchProcess:
            pass
    for process in reversed(known):
        try:
            process.kill()
        except psutil.NoSuchProcess:
            pass
    psutil.wait_procs(known, timeout=3)
(root / 'result.json').write_text(json.dumps(report, indent=2))
print(json.dumps(report, indent=2))
assert report['orphan_cargo_alive'] and report['replacement_blocked_on_cargo_lock'], report
