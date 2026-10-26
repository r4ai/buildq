#!/usr/bin/env python3
"""Capture argv during real Cargo probes in a many-member workspace burst."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import subprocess
import time
import psutil

p=argparse.ArgumentParser()
p.add_argument('--root',type=Path,required=True)
p.add_argument('--buildq',type=Path,required=True)
p.add_argument('--output',type=Path,required=True)
p.add_argument('--members',type=int,default=1000)
p.add_argument('--jobs',type=int,default=12)
a=p.parse_args();root=a.root.resolve();repo=root/'repo'
repo.mkdir(parents=True,exist_ok=True);(root/'state').mkdir(exist_ok=True);a.output.mkdir(parents=True,exist_ok=True)
(repo/'Cargo.toml').write_text('[workspace]\nresolver="2"\nmembers=["parts/*"]\n')
for i in range(a.members):
 c=repo/'parts'/f'part{i:04d}';(c/'src').mkdir(parents=True,exist_ok=True)
 (c/'Cargo.toml').write_text(f'[package]\nname="part{i:04d}"\nversion="0.1.0"\nedition="2021"\n')
 (c/'src/lib.rs').write_text('pub fn value()->u32 { 42 }\n')
for cmd in (['git','init','-q',str(repo)],['git','-C',str(repo),'add','.'],['git','-C',str(repo),'-c','user.name=QA','-c','user.email=qa@example.invalid','commit','-qm','many-member fixture']):subprocess.run(cmd,check=True)
(root/'state/config.toml').write_text('slots=2\nmax_per_worktree=0\n[cache]\nbudget="200M"\nmin_free="1G"\ngc_mode="inline"\ndedupe="off"\n')
env={k:v for k,v in os.environ.items() if not k.startswith('BUILDQ_') and k not in ('CARGO_TARGET_DIR','CARGO_BUILD_TARGET_DIR')}
env.update(BUILDQ_DIR=str(root/'state'),BUILDQ_CACHE=str(root/'cache'),BUILDQ_CONFIG=str(root/'state/config.toml'),BUILDQ_REAL_CARGO='/workspace/.buildq-env/cargo/bin/cargo',CARGO_BUILD_JOBS='1',RUSTC_WRAPPER='',BUILDQ_GC='0')
jobs=[]
for i in range(a.jobs):
 out=(a.output/f'job-{i}.stdout').open('wb');err=(a.output/f'job-{i}.stderr').open('wb')
 proc=subprocess.Popen([str(a.buildq),'cargo','build','--offline','-p','part0000'],cwd=repo,env=env,stdout=out,stderr=err,start_new_session=True)
 jobs.append((proc,out,err))
samples=[];max_cargos=0;max_compilers=0
while any(job[0].poll() is None for job in jobs):
 children=[]
 for job,_,_ in jobs:
  try:children.extend(psutil.Process(job.pid).children(recursive=True))
  except psutil.NoSuchProcess:pass
 cargos=[];compilers=[]
 for child in children:
  try:
   row=dict(pid=child.pid,ppid=child.ppid(),comm=child.name(),argv=child.cmdline(),state=child.status())
   if row['comm']=='cargo':cargos.append(row)
   if row['comm']=='rustc':compilers.append(row)
  except (psutil.NoSuchProcess,psutil.ZombieProcess):pass
 max_cargos=max(max_cargos,len(cargos));max_compilers=max(max_compilers,len(compilers))
 if len(cargos)>4:
  status=subprocess.run([str(a.buildq),'status','--json'],cwd=repo,env=env,capture_output=True,text=True)
  samples.append(dict(t=time.time(),cargos=cargos,compilers=compilers,status=json.loads(status.stdout)))
 time.sleep(.01)
for proc,out,err in jobs:proc.wait();out.close();err.close()
result=dict(members=a.members,jobs=a.jobs,slots=2,max_cargos=max_cargos,max_compilers=max_compilers,exits=[p.returncode for p,_,_ in jobs],samples=samples)
(a.output/'result.json').write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps(result,indent=2))
