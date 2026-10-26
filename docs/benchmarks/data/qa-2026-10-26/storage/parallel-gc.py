#!/usr/bin/env python3
"""Race completing oversize jobs, automatic GC, and foreground GC safely."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import subprocess
import time

p=argparse.ArgumentParser()
p.add_argument('--root',type=Path,required=True)
p.add_argument('--output',type=Path,required=True)
p.add_argument('--buildq',type=Path,required=True)
p.add_argument('--cycles',type=int,default=30)
a=p.parse_args()
a.output.mkdir(parents=True,exist_ok=True)
root=a.root.resolve()
repo=root/'repo'
wt=root/'wt'
if not wt.exists(): subprocess.run(['git','-C',str(repo),'worktree','add','--detach',str(wt)],check=True,capture_output=True)
env={k:v for k,v in os.environ.items() if not k.startswith('BUILDQ_') and k not in ('CARGO_TARGET_DIR','CARGO_BUILD_TARGET_DIR')}
env.update(BUILDQ_DIR=str(root/'state'),BUILDQ_CACHE=str(root/'cache'),BUILDQ_CONFIG=str(root/'state/config.toml'),BUILDQ_REAL_CARGO='/workspace/.buildq-env/cargo/bin/cargo',CARGO_BUILD_JOBS='1',RUSTC_WRAPPER='',PAYLOAD_MB='96',HOLD_SECONDS='2',TMPDIR=str(root/'tmp'))
config=Path(env['BUILDQ_CONFIG'])
records=[]
def run(name,cmd,cwd,extra=None):
    start=time.monotonic()
    proc=subprocess.run([str(a.buildq),*cmd],cwd=cwd,env=env|dict(extra or {}),capture_output=True,timeout=90)
    (a.output/f'{name}.stdout').write_bytes(proc.stdout)
    (a.output/f'{name}.stderr').write_bytes(proc.stderr)
    data=dict(name=name,exit=proc.returncode,seconds=round(time.monotonic()-start,4))
    try: data['json']=json.loads(proc.stdout)
    except ValueError: pass
    return data
for mode in ('inline','background'):
    config.write_text(f'slots=2\nmax_per_worktree=0\n[cache]\nbudget="64M"\nmin_free="1G"\ngc_mode="{mode}"\ndedupe="off"\n')
    for cycle in range(a.cycles):
        if os.statvfs(root).f_bavail*os.statvfs(root).f_frsize<6*1024**3: raise RuntimeError('global free-space floor reached')
        with ThreadPoolExecutor(4) as ex:
            jobs=[ex.submit(run,f'{mode}-{cycle}-build-{i}',['cargo','build','--offline'],cwd) for i,cwd in enumerate((repo,wt))]
            time.sleep(.6)
            jobs += [ex.submit(run,f'{mode}-{cycle}-gc-{i}',['gc','--json'],repo) for i in range(2)]
            records.extend(j.result() for j in jobs)
        time.sleep(.3)
        records.append(run(f'{mode}-{cycle}-after',['cache','ls','--refresh','--json'],repo))
        (a.output/'results.json').write_text(json.dumps(records,indent=2)+'\n')
config.write_text('slots=2\n[cache]\nbudget="64M"\nmin_free="64G"\ngc_mode="inline"\n')
records.append(run('minfree-admission',['cargo','build','--offline'],repo))
records.append(run('minfree-status',['status','--json'],repo))
(a.output/'results.json').write_text(json.dumps(records,indent=2)+'\n')
print(json.dumps(records,indent=2))
