#!/usr/bin/env python3
"""Storage lifecycle probes. Run against an isolated root; never touches user caches."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import time

p = argparse.ArgumentParser()
p.add_argument('--root', type=Path, required=True)
p.add_argument('--buildq', type=Path, required=True)
p.add_argument('--output', type=Path, required=True)
a = p.parse_args()
root = a.root.resolve()
root.mkdir(parents=True, exist_ok=True)
a.output.mkdir(parents=True, exist_ok=True)
env = dict(os.environ)
for key in list(env):
    if key.startswith('BUILDQ_') or key in ('CARGO_TARGET_DIR', 'CARGO_BUILD_TARGET_DIR'):
        env.pop(key)
env.update(BUILDQ_DIR=str(root/'state'), BUILDQ_CACHE=str(root/'cache'),
           BUILDQ_CONFIG=str(root/'state/config.toml'),
           BUILDQ_REAL_CARGO='/workspace/.buildq-env/cargo/bin/cargo',
           BUILDQ_GC='0', CARGO_BUILD_JOBS='1', RUSTC_WRAPPER='',
           TMPDIR=str(root/'tmp'))
Path(env['BUILDQ_DIR']).mkdir(exist_ok=True)
Path(env['TMPDIR']).mkdir(exist_ok=True)
Path(env['BUILDQ_CONFIG']).write_text('slots = 2\n[cache]\nbudget = "64M"\nmin_free = "1G"\ngc_mode = "inline"\ndedupe = "off"\n')
repo = root/'repo'
(repo/'src').mkdir(parents=True, exist_ok=True)
(repo/'Cargo.toml').write_text('[package]\nname="storage-probe"\nversion="0.1.0"\nedition="2021"\n')
(repo/'src/main.rs').write_text('fn main() { println!("storage-correct"); }\n')
(repo/'build.rs').write_text('''use std::{env,fs::{self,File},io::Write,thread,time::Duration};
fn main() {
 println!("cargo:rerun-if-env-changed=PAYLOAD_MB");
 println!("cargo:rerun-if-env-changed=HOLD_SECONDS");
 let out = env::var("OUT_DIR").unwrap();
 let size: usize = env::var("PAYLOAD_MB").unwrap_or("96".into()).parse().unwrap();
 let mut f = File::create(format!("{}/payload.bin",out)).unwrap();
 let block = vec![0xa5u8; 1024*1024];
 for _ in 0..size { f.write_all(&block).unwrap(); }
 f.sync_all().unwrap();
 fs::write(format!("{}/ready",out), "ready").unwrap();
 let secs: u64 = env::var("HOLD_SECONDS").unwrap_or("0".into()).parse().unwrap();
 thread::sleep(Duration::from_secs(secs));
}
''')
subprocess.run(['git','init','-q',str(repo)], check=True)
subprocess.run(['git','-C',str(repo),'add','.'], check=True)
subprocess.run(['git','-C',str(repo),'-c','user.name=QA','-c','user.email=qa@example.invalid','commit','-qm','fixture'], check=True)
records=[]
def call(name, args, extra=None, timeout=120):
    started=time.monotonic()
    r=subprocess.run([str(a.buildq),*args],cwd=repo,env=env|dict(extra or {}),capture_output=True,timeout=timeout)
    (a.output/f'{name}.stdout').write_bytes(r.stdout)
    (a.output/f'{name}.stderr').write_bytes(r.stderr)
    record=dict(name=name,args=args,extra=extra,exit=r.returncode,seconds=round(time.monotonic()-started,4))
    try: record['json']=json.loads(r.stdout)
    except (ValueError,UnicodeDecodeError): pass
    records.append(record)
    (a.output/'results.json').write_text(json.dumps(records,indent=2)+'\n')
    return r
def ready_files(): return list((root/'cache').glob('pools/**/ready'))
def wait_ready(process):
    deadline=time.monotonic()+60
    while time.monotonic()<deadline:
        if ready_files(): return
        if process.poll() is not None: raise RuntimeError('build ended before ready')
        time.sleep(.1)
    raise RuntimeError('payload writer not ready')

call('oversize-build',['cargo','build','--offline'])
call('oversize-cache',['cache','ls','--refresh','--json'])
call('oversize-gc-dry',['gc','--dry-run','--json'])
call('oversize-gc',['gc','--json'])
call('oversize-after',['cache','ls','--refresh','--json'])
call('oversize-cold-rebuild',['cargo','build','--offline'])
call('clear-after-oversize',['gc','--json'])

for mode in ('busy','kill'):
    stdout=(a.output/f'{mode}-build.stdout').open('wb')
    stderr=(a.output/f'{mode}-build.stderr').open('wb')
    process=subprocess.Popen([str(a.buildq),'cargo','build','--offline'],cwd=repo,env=env|{'HOLD_SECONDS':'12','PAYLOAD_MB':'128'},stdout=stdout,stderr=stderr,start_new_session=True)
    wait_ready(process)
    call(f'{mode}-status',['status','--json'])
    call(f'{mode}-cache',['cache','ls','--refresh','--json'])
    call(f'{mode}-gc',['gc','--json'])
    payloads=list((root/'cache').glob('pools/**/payload.bin'))
    records.append(dict(name=f'{mode}-busy-payload-protected',sizes=[x.stat().st_size for x in payloads]))
    if mode=='kill':
        os.kill(process.pid,signal.SIGKILL)
    status=process.wait(timeout=40)
    stdout.close(); stderr.close()
    records.append(dict(name=f'{mode}-build-exit',exit=status))
    time.sleep(3)
    call(f'{mode}-after-status',['status','--json'])
    call(f'{mode}-after-gc',['gc','--json'])
    call(f'{mode}-after-cache',['cache','ls','--refresh','--json'])
    call(f'{mode}-recovery-run',['cargo','run','--offline'],{'PAYLOAD_MB':'1','HOLD_SECONDS':'0'})
    call(f'{mode}-final-gc',['gc','--json'])
    # Remove ready files by evicting the small survivor before the next round.
    call(f'{mode}-clear',['cache','clean','--all'])

env['BUILDQ_GC']='1'
for i in range(5):
    call(f'inline-{i}-oversize',['cargo','build','--offline'],{'PAYLOAD_MB':'96','HOLD_SECONDS':'0'})
    call(f'inline-{i}-after',['cache','ls','--refresh','--json'])
call('final-status',['status','--json'])
(a.output/'results.json').write_text(json.dumps(records,indent=2)+'\n')
print(json.dumps(records,indent=2))
