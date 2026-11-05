#!/usr/bin/env python3
"""Linux-only harness init: reap adopted descendants without altering buildq."""
import ctypes
import os
import subprocess
import sys
import time

libc = ctypes.CDLL(None, use_errno=True)
if libc.prctl(36, 1, 0, 0, 0):
    raise OSError(ctypes.get_errno(), 'PR_SET_CHILD_SUBREAPER')
child = subprocess.Popen(sys.argv[1:])
result = None
reaped = 0
while result is None:
    while True:
        try:
            pid, status = os.waitpid(-1, os.WNOHANG)
        except ChildProcessError:
            break
        if not pid:
            break
        if pid == child.pid:
            result = os.waitstatus_to_exitcode(status)
        else:
            reaped += 1
    time.sleep(.05)
print(f'QA init reaped {reaped} adopted children', file=sys.stderr)
raise SystemExit(result)
