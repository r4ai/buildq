#!/usr/bin/env python3
"""Linux heavy-build measurement helper. Requires psutil; never deletes caches.

Run in an activated Rust environment. Arguments: NAME CWD LOG_DIR -- COMMAND...
Records aggregate sampled process-tree RSS (not PSS), CPU time, output and status.
Output is JSONL, suitable for comparing isolated and concurrent command batches.
"""
import argparse
import json
import pathlib
import subprocess
import threading
import time

import psutil


def measure(name, cwd, logs, command, env=None):
    logs = pathlib.Path(logs)
    logs.mkdir(parents=True, exist_ok=True)
    stats = dict(peak_rss=0, peak_processes=0)
    cpu = {}
    stop = threading.Event()
    start = time.monotonic()
    with (logs / f'{name}.stdout').open('w') as out, (logs / f'{name}.stderr').open('w') as err:
        process = subprocess.Popen(command, cwd=cwd, env=env, stdout=out, stderr=err)
        root = psutil.Process(process.pid)

        def sample():
            while not stop.is_set():
                try:
                    processes = [root] + root.children(recursive=True)
                except psutil.NoSuchProcess:
                    processes = []
                rss = 0
                for child in processes:
                    try:
                        rss += child.memory_info().rss
                        t = child.cpu_times()
                        cpu[(child.pid, child.create_time())] = t.user + t.system
                    except (psutil.NoSuchProcess, psutil.AccessDenied):
                        pass
                stats['peak_rss'] = max(stats['peak_rss'], rss)
                stats['peak_processes'] = max(stats['peak_processes'], len(processes))
                stop.wait(0.05)

        sampler = threading.Thread(target=sample, daemon=True)
        sampler.start()
        try:
            process.wait()
            wall = time.monotonic() - start
        finally:
            stop.set()
            sampler.join()
    stderr = (logs / f'{name}.stderr').read_text()
    row = dict(name=name, command=command, cwd=str(cwd), wall_s=wall,
               exit=process.returncode, peak_tree_rss_bytes=stats['peak_rss'],
               sampled_cpu_s=sum(cpu.values()), peak_processes=stats['peak_processes'],
               compiling_lines=sum(line.lstrip().startswith('Compiling ') for line in stderr.splitlines()))
    with (logs / 'measurements.jsonl').open('a') as f:
        f.write(json.dumps(row) + '\n')
    print(json.dumps(row), flush=True)
    return row


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('name')
    parser.add_argument('cwd')
    parser.add_argument('logs')
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ['--'] else args.command
    if not command:
        parser.error('command is required')
    result = measure(args.name, args.cwd, args.logs, command)
    raise SystemExit(result['exit'])
