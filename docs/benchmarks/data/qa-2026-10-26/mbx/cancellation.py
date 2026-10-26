#!/usr/bin/env python3
"""Cancel a real release compilation through buildq, then verify store/recovery."""
import run,os,pathlib,signal,subprocess,time,json,psutil
R=run.ROOT/'bevy';A=R/'a';O=run.OUT
# The concurrent source-rotation scenario must finish before using its pool.
while not (O/'rotation-correctness.json').exists():time.sleep(.5)
run.ENV.update(BUILDQ_DIR=str(R/'state'),BUILDQ_CACHE=str(R/'cache'),MBX_CACHE_DIR=str(R/'mbx-cache'),CARGO_BUILD_JOBS='2')
argv=[str(run.BQ),'cargo','build','--release','--locked','--offline']
start=time.monotonic()
with (O/'cancel-bevy-release.stdout').open('w') as out,(O/'cancel-bevy-release.stderr').open('w') as err:
 p=subprocess.Popen(argv,cwd=A,env=run.ENV,stdout=out,stderr=err,start_new_session=True)
 deadline=time.monotonic()+30
 while time.monotonic()<deadline and p.poll() is None:
  if 'Compiling ' in (O/'cancel-bevy-release.stderr').read_text():break
  time.sleep(.05)
 time.sleep(.3)
 children=[]
 if p.poll() is None:
  for child in psutil.Process(p.pid).children(recursive=True):
   try:children.append({'pid':child.pid,'created':child.create_time(),'name':child.name()})
   except psutil.Error:pass
  at=time.monotonic();os.kill(p.pid,signal.SIGTERM)
  try:code=p.wait(timeout=30)
  except subprocess.TimeoutExpired:os.killpg(p.pid,signal.SIGKILL);code=p.wait()
  cancel_s=time.monotonic()-at
 else:code=p.returncode;cancel_s=None
 live=[];zombies=[];deadline=time.monotonic()+5
 while time.monotonic()<deadline:
  live=[];zombies=[]
  for child in children:
   try:
    process=psutil.Process(child['pid'])
    if process.create_time()!=child['created']:continue
    if process.status()==psutil.STATUS_ZOMBIE:zombies.append(child)
    else:live.append(child)
   except psutil.Error:pass
  if not live:break
  time.sleep(.1)
 record={'argv':argv,'cwd':str(A),'signal':'SIGTERM sent to buildq parent PID only','exit':code,'wall_s':round(time.monotonic()-start,3),'cancel_to_exit_s':cancel_s,'observed_children':children,'live_children_after':live,'zombie_children_after':zombies,'passed':code!=0 and not live and cancel_s is not None}
 (O/'cancellation-summary.json').write_text(json.dumps(record,indent=2)+'\n');print(json.dumps(record),flush=True)
run.run('cancel-recovery-debug-A',A,['cargo','run','--offline','--quiet'])
run.run('cancel-verify-mbx-store',A,['run','--',str(run.ROOT/'mbx'),'cache','verify'],timeout=120)
run.run('cancel-final-status',A,['status','--json'])
