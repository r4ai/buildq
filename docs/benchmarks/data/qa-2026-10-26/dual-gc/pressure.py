#!/usr/bin/env python3
"""Apply foreground pressure to a running dual-GC fixture without editing config."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import time

p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--root',type=Path,required=True);p.add_argument('--buildq',type=Path,required=True);p.add_argument('--mbx',type=Path,required=True);p.add_argument('--duration',type=float,default=900);p.add_argument('--interval',type=float,default=30)
a=p.parse_args();root=a.root.resolve();env=os.environ.copy()
env.update(BUILDQ_DIR=str(root/'state'),BUILDQ_CACHE=str(root/'cache'),MBX_CACHE_DIR=str(root/'mbx'),MBX_GC_MAX_TOTAL_SIZE='24MiB',MBX_GC_MAX_SIZE='16MiB',MBX_GC_MIN_FREE_SIZE='0',MBX_GC_AUTO='1',MBX_GC_INTERVAL='1s',MBX_REMOTE_URL='')
env.pop('BUILDQ_CONFIG',None);env.pop('BUILDQ_HELD',None)
start=time.monotonic();deadline=start+a.duration
with (root/'logs/pressure.jsonl').open('a') as log:
    n=0
    while time.monotonic()<deadline:
        for kind,cmd in [('mbx_gc_pressure',[str(a.mbx.resolve()),'gc','--max-size','128KiB','--json']),('buildq_gc_pressure',[str(a.buildq.resolve()),'gc','--budget','512K','--json'])]:
            begin=time.monotonic();z=subprocess.run(cmd,cwd=root/'repo',env=env,text=True,capture_output=True,timeout=90)
            row=dict(kind=kind,index=n,unix_ms=int(time.time()*1000),elapsed_s=time.monotonic()-start,wall_s=time.monotonic()-begin,argv=cmd,exit=z.returncode,stdout=z.stdout,stderr=z.stderr)
            log.write(json.dumps(row)+'\n');log.flush()
        n+=1
        until=min(deadline,time.monotonic()+a.interval)
        while time.monotonic()<until:time.sleep(min(1,until-time.monotonic()))
print(json.dumps({'cycles':n,'seconds':time.monotonic()-start}))
