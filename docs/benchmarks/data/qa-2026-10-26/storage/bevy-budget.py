#!/usr/bin/env python3
"""Run actual Bevy ECS builds with an undersized retention budget."""
import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import threading
import time

p=argparse.ArgumentParser()
p.add_argument('--source',type=Path,required=True)
p.add_argument('--root',type=Path,required=True)
p.add_argument('--buildq',type=Path,required=True)
p.add_argument('--output',type=Path,required=True)
p.add_argument('--lock',type=Path,help='Optional retained Cargo.lock or Cargo.lock.gz to copy into the experiment clone')
a=p.parse_args();root=a.root.resolve();root.mkdir(parents=True,exist_ok=True);a.output.mkdir(parents=True,exist_ok=True)
repo=root/'repo'
if not repo.exists(): subprocess.run(['git','clone','--local','--no-hardlinks',str(a.source),str(repo)],check=True,capture_output=True)
if a.lock:
 lock=a.lock.read_bytes()
 (repo/'Cargo.lock').write_bytes(gzip.decompress(lock) if a.lock.suffix=='.gz' else lock)
(root/'state').mkdir(exist_ok=True);(root/'tmp').mkdir(exist_ok=True)
env={k:v for k,v in os.environ.items() if not k.startswith('BUILDQ_') and k not in ('CARGO_TARGET_DIR','CARGO_BUILD_TARGET_DIR')}
env.update(BUILDQ_DIR=str(root/'state'),BUILDQ_CACHE=str(root/'cache'),BUILDQ_CONFIG=str(root/'state/config.toml'),BUILDQ_REAL_CARGO='/workspace/.buildq-env/cargo/bin/cargo',CARGO_BUILD_JOBS='1',RUSTC_WRAPPER='',TMPDIR=str(root/'tmp'),BUILDQ_GC='0',BUILDQ_TRACE='1')
config=root/'state/config.toml'
def cfg(budget):config.write_text(f'slots=2\n[cache]\nbudget="{budget}"\nmin_free="6G"\ngc_mode="inline"\ndedupe="off"\n[cargo]\njobs=1\n')
results=[]
def call(name,args):
 start=time.monotonic()
 out=(a.output/f'{name}.stdout').open('wb');err=(a.output/f'{name}.stderr').open('wb')
 job=subprocess.Popen([str(a.buildq),*args],cwd=repo,env=env,stdout=out,stderr=err,start_new_session=True)
 limited=[];stop=threading.Event()
 def monitor():
  while not stop.is_set() and job.poll() is None:
   st=os.statvfs(root)
   if st.f_bavail*st.f_frsize<6*1024**3:
    limited.append('global 6GiB free-space floor')
    try:os.kill(job.pid,signal.SIGTERM)
    except ProcessLookupError:pass
    return
   stop.wait(.2)
 watcher=threading.Thread(target=monitor,daemon=True);watcher.start()
 try:status=job.wait(timeout=720)
 except subprocess.TimeoutExpired:
  limited.append('12-minute individual command deadline')
  os.killpg(job.pid,signal.SIGKILL);status=job.wait()
 elapsed=time.monotonic()-start
 stop.set();watcher.join()
 out.close();err.close()
 result=dict(name=name,args=args,exit=status,seconds=round(elapsed,4),harness_limit=limited or None)
 try:result['json']=json.loads((a.output/f'{name}.stdout').read_bytes())
 except ValueError:pass
 results.append(result);(a.output/'results.json').write_text(json.dumps(results,indent=2)+'\n')
 return status
revision=subprocess.check_output(['git','rev-parse','HEAD'],cwd=repo,text=True).strip()
(a.output/'methodology.json').write_text(json.dumps(dict(buildq_revision='1391d00',oss_revision=revision,source=str(a.source),cargo_home=env.get('CARGO_HOME'),cargo_jobs=1,offline=True,automatic_gc=False,real_cargo=env['BUILDQ_REAL_CARGO'],root=str(root)),indent=2)+'\n')
cfg('64M')
if call('cold-64M',['cargo','build','--offline','-p','bevy_ecs'])==0:
 call('cold-cache',['cache','ls','--refresh','--json'])
 call('evict-64M',['gc','--json'])
 call('after-evict',['cache','ls','--refresh','--json'])
 if call('cold-rebuild-64M',['cargo','build','--offline','-p','bevy_ecs'])==0:
  call('rebuilt-cache',['cache','ls','--refresh','--json'])
  cfg('2G')
  call('warm-2G',['cargo','build','--offline','-p','bevy_ecs'])
  call('warm-cache',['cache','ls','--refresh','--json'])
  call('retaining-gc-2G',['gc','--json'])
  call('after-retaining-gc',['cache','ls','--refresh','--json'])
call('final-status',['status','--json'])
if (repo/'Cargo.lock').exists():
 lock=(repo/'Cargo.lock').read_bytes()
 (a.output/'Cargo.lock.gz').write_bytes(gzip.compress(lock,mtime=0))
 path=a.output/'methodology.json'
 method=json.loads(path.read_text())
 method.update(cargo_lock_sha256=hashlib.sha256(lock).hexdigest(),retained_cargo_lock='Cargo.lock.gz',timing_method='Popen.wait(timeout=720), disk guard on separate thread; Python timeout wait loop can add up to approximately 50ms.',oss_scope='ECS crate and dependencies, not the full Bevy renderer')
 path.write_text(json.dumps(method,indent=2)+'\n')
print(json.dumps(results,indent=2))
