#!/usr/bin/env python3
"""Exercise the running-build free-space guard without filling the volume."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import time

p=argparse.ArgumentParser()
p.add_argument('--root',type=Path,required=True)
p.add_argument('--buildq',type=Path,required=True)
p.add_argument('--output',type=Path,required=True)
a=p.parse_args()
root=a.root.resolve();repo=root/'repo'
(repo/'src').mkdir(parents=True,exist_ok=True)
(root/'state').mkdir(exist_ok=True);(root/'tmp').mkdir(exist_ok=True)
a.output.mkdir(parents=True,exist_ok=True)
(repo/'Cargo.toml').write_text('[package]\nname="minfree-probe"\nversion="0.1.0"\nedition="2021"\n')
(repo/'src/main.rs').write_text('fn main() {}\n')
(repo/'build.rs').write_text('''use std::{env,fs::File,io::Write,thread,time::Duration};
fn main(){let mut f=File::create(format!("{}/payload",env::var("OUT_DIR").unwrap())).unwrap();let b=vec![0xabu8;8*1024*1024];for _ in 0..96{f.write_all(&b).unwrap();f.sync_data().unwrap();thread::sleep(Duration::from_millis(100));}thread::sleep(Duration::from_secs(30));}
''')
for cmd in (['git','init','-q',str(repo)],['git','-C',str(repo),'add','.'],['git','-C',str(repo),'-c','user.name=QA','-c','user.email=qa@example.invalid','commit','-qm','guard fixture']):subprocess.run(cmd,check=True)
env={k:v for k,v in os.environ.items() if not k.startswith('BUILDQ_') and k not in ('CARGO_TARGET_DIR','CARGO_BUILD_TARGET_DIR')}
env.update(BUILDQ_DIR=str(root/'state'),BUILDQ_CACHE=str(root/'cache'),BUILDQ_CONFIG=str(root/'state/config.toml'),BUILDQ_REAL_CARGO='/workspace/.buildq-env/cargo/bin/cargo',CARGO_BUILD_JOBS='1',RUSTC_WRAPPER='',TMPDIR=str(root/'tmp'),BUILDQ_GC='0')
free=os.statvfs(root).f_bavail*os.statvfs(root).f_frsize
if free<6*1024**3:raise RuntimeError('global free floor reached')
reserve=free-256*1024**2
(root/'state/config.toml').write_text(f'slots=2\n[cache]\nbudget="2G"\nmin_free="{reserve}"\ngc_mode="inline"\n')
start=time.monotonic()
r=subprocess.run([str(a.buildq),'cargo','build','--offline'],cwd=repo,env=env,capture_output=True,timeout=70)
(a.output/'build.stdout').write_bytes(r.stdout);(a.output/'build.stderr').write_bytes(r.stderr)
record=dict(exit=r.returncode,seconds=time.monotonic()-start,free_before=free,reserve=reserve,free_after=os.statvfs(root).f_bavail*os.statvfs(root).f_frsize,payload_sizes=[p.stat().st_size for p in (root/'cache').glob('pools/**/payload')])
for name,args in [('status',['status','--json']),('cache',['cache','ls','--refresh','--json']),('gc',['gc','--json'])]:
 r=subprocess.run([str(a.buildq),*args],cwd=repo,env=env,capture_output=True,timeout=30)
 (a.output/f'{name}.stdout').write_bytes(r.stdout);(a.output/f'{name}.stderr').write_bytes(r.stderr)
 record[name]=dict(exit=r.returncode,json=json.loads(r.stdout))
(a.output/'result.json').write_text(json.dumps(record,indent=2)+'\n')
print(json.dumps(record,indent=2))
