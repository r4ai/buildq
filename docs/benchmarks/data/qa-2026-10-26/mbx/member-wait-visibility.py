#!/usr/bin/env python3
"""Show alive member waiters omitted from status. Requires Git, Cargo, Python3.
Creates only NEW --root; prints and saves exact PIDs/argv/status/stderr snapshot.
"""
import argparse,json,os,shutil,subprocess,time
from pathlib import Path
p=argparse.ArgumentParser(description=__doc__);p.add_argument('--root',type=Path,required=True);p.add_argument('--buildq',type=Path,required=True);p.add_argument('--cargo',type=Path,default=Path(shutil.which('cargo') or 'cargo'));a=p.parse_args()
root=a.root.resolve()
if root.exists():p.error('--root must not exist')
ws=root/'ws';ws.mkdir(parents=True);(ws/'src').mkdir()
(ws/'Cargo.toml').write_text('[package]\nname="member-wait-probe"\nversion="0.1.0"\nedition="2021"\n')
(ws/'src/main.rs').write_text('fn main(){}\n')
(ws/'build.rs').write_text('fn main(){ std::thread::sleep(std::time::Duration::from_secs(10)); println!("cargo:rerun-if-changed=build.rs"); }\n')
for cmd in [['init','-q'],['add','.'],['commit','-qm','fixture']]:subprocess.run(['git','-c','user.name=QA','-c','user.email=qa@example.invalid','-C',str(ws),*cmd],check=True)
state=root/'state';state.mkdir();(state/'config.toml').write_text('slots=2\n[cache]\nmax_members=2\nbudget="1G"\nmin_free="0"\n')
env=os.environ.copy()
for name in ['BUILDQ_HELD','BUILDQ_CONFIG','CARGO_TARGET_DIR','RUSTC_WRAPPER']:env.pop(name,None)
env.update(BUILDQ_DIR=str(state),BUILDQ_CACHE=str(root/'cache'),BUILDQ_GC='0',BUILDQ_REAL_CARGO=str(a.cargo.absolute()),BUILDQ_TRACE='1',CARGO_BUILD_JOBS='1')
processes=[];files=[];snapshot=None
try:
 for i in range(8):
  out=(root/f'job-{i}.stdout').open('w');err=(root/f'job-{i}.stderr').open('w');files.extend([out,err]);argv=[str(a.buildq.resolve()),'cargo','build','--offline']
  proc=subprocess.Popen(argv,cwd=ws,env=env,stdout=out,stderr=err,start_new_session=True);processes.append((proc,argv))
 deadline=time.monotonic()+8
 while time.monotonic()<deadline:
  status=subprocess.run([str(a.buildq.resolve()),'status','--json'],env=env,capture_output=True,text=True,check=True)
  data=json.loads(status.stdout);build=next(c for c in data['classes'] if c['class']=='build')
  jobs=[{'pid':proc.pid,'argv':argv,'poll':proc.poll(),'stderr':(root/f'job-{i}.stderr').read_text()} for i,(proc,argv) in enumerate(processes)]
  alive=sum(job['poll'] is None for job in jobs);busy=sum(h['busy'] for h in build['holders']);member_waiters=sum('all cache members busy' in job['stderr'] for job in jobs)
  if alive==8 and busy==2 and member_waiters==6:
   snapshot={'at_unix':time.time(),'cwd':str(ws),'submitted':8,'alive':alive,'busy_build_holders':busy,'pool_member_waiters_from_stderr':member_waiters,'visible_queue_waiters':len(build['waiters']),'jobs':jobs,'status':data};break
  time.sleep(.1)
 if snapshot is None:raise RuntimeError('did not observe expected eight-job state')
 (root/'snapshot.json').write_text(json.dumps(snapshot,indent=2)+'\n');print(json.dumps({k:v for k,v in snapshot.items() if k not in ['jobs','status']},indent=2),flush=True)
 for proc,_ in processes:proc.wait(timeout=60)
 (root/'exits.json').write_text(json.dumps([proc.returncode for proc,_ in processes])+'\n')
finally:
 for proc,_ in processes:
  if proc.poll() is None:os.killpg(proc.pid,15);proc.wait()
 for f in files:f.close()
