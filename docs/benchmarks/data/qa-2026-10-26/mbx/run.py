#!/usr/bin/env python3
"""Pinned real-OSS mbx integration QA. No writes to upstream repositories."""
import concurrent.futures, json, os, pathlib, re, subprocess, time, shutil
import psutil
ROOT=pathlib.Path(os.environ.get('QA_ROOT','/workspace/qa-mbx'))
OUT=pathlib.Path(os.environ.get('QA_OUTPUT','/workspace/buildq/docs/benchmarks/data/qa-2026-10-26/mbx'))
OUT.mkdir(parents=True,exist_ok=True)
BQ=pathlib.Path(os.environ.get('QA_BUILDQ','/workspace/qa-bin/buildq'))
ENV=os.environ.copy()
ENV.update(BUILDQ_DIR=str(ROOT/'state'),BUILDQ_CACHE=str(ROOT/'cache'),MBX_CACHE_DIR=str(ROOT/'mbx-cache'),MBX_GC_AUTO='0',BUILDQ_REAL_CARGO=os.environ.get('BUILDQ_REAL_CARGO') or shutil.which('cargo') or 'cargo',BUILDQ_TRACE='1',CARGO_BUILD_JOBS='2')
ENV.pop('CARGO_TARGET_DIR',None)
ENV.pop('BUILDQ_HELD',None)
LOCK=__import__('threading').Lock()
def config(backend):
 (ROOT/'state/config.toml').write_text(f'''slots=2\n[cache]\nbudget="6G"\nmin_free="0"\nmax_members=2\ngc_mode="inline"\nseed_members=true\n[cargo]\nbackend="{backend}"\nmbx="{os.environ.get('QA_MBX',str(ROOT/'mbx'))}"\nnested_mbx=true\nmirror_bins=false\n''')
def run(name,cwd,args,timeout=900,env=None):
 cmd=[str(BQ),*args]
 start=time.monotonic(); peak=0; cpu={}; samples=0
 with (OUT/f'{name}.stdout').open('w') as out,(OUT/f'{name}.stderr').open('w') as err:
  p=subprocess.Popen(cmd,cwd=cwd,env=env or ENV,stdout=out,stderr=err,start_new_session=True)
  while p.poll() is None:
   try:
    children=[psutil.Process(p.pid),*psutil.Process(p.pid).children(recursive=True)]
    rss=0
    for child in children:
     try:
      rss+=child.memory_info().rss; ct=child.cpu_times();cpu[(child.pid,child.create_time())]=ct.user+ct.system
     except psutil.Error: pass
    peak=max(peak,rss);samples+=1
   except psutil.Error: pass
   if time.monotonic()-start>timeout:
    os.killpg(p.pid,15);p.wait();break
   time.sleep(.1)
  rc=p.wait()
 stderr=(OUT/f'{name}.stderr').read_text(errors='replace')
 record=dict(name=name,cmd=cmd,cwd=str(cwd),exit=rc,wall_s=round(time.monotonic()-start,3),peak_tree_rss=peak,cpu_s=round(sum(cpu.values()),3),samples=samples,cache_hits=[int(x) for x in re.findall(r'mbx\[cache\]: (\d+) hits',stderr)],compiling=stderr.count('Compiling '))
 with LOCK:
  with (OUT/'measurements.jsonl').open('a') as f:f.write(json.dumps(record)+'\n')
  print(json.dumps(record),flush=True)
 return record
RG=ROOT/'inputs/ripgrep-14.1.1';TK=ROOT/'inputs/tokio-tokio-1.45.1'
if __name__=='__main__':
 config('cargo')
 run('cargo-ripgrep-cold',RG,['run','--','cargo','build','--locked'])
 run('cargo-ripgrep-warm',RG,['run','--','cargo','build','--locked'])
 config('mbx')
 run('mbx-ripgrep-populate',RG,['run','--','cargo','build','--locked'])
 run('clean-before-restore',RG,['cache','clean','--all'])
 run('mbx-ripgrep-restore',pathlib.Path(str(RG)+'-wt1'),['run','--','cargo','build','--locked'])
 run('mbx-ripgrep-version',pathlib.Path(str(RG)+'-wt1'),['run','--','cargo','run','--locked','--','--version'])
 run('mbx-ripgrep-tests',RG,['run','--','cargo','test','--locked','--test','integration'])
 run('mbx-tokio-full-check',TK,['run','--','cargo','check','--locked','-p','tokio@1.45.1','--all-features'])
 run('mbx-tokio-minimal-check',pathlib.Path(str(TK)+'-wt1'),['run','--','cargo','check','--locked','-p','tokio@1.45.1','--no-default-features'])
 run('mbx-tokio-full-check-again',pathlib.Path(str(TK)+'-wt2'),['run','--','cargo','check','--locked','-p','tokio@1.45.1','--all-features'])
 run('mbx-ripgrep-release',RG,['run','--','cargo','build','--locked','--release'])
 run('mbx-ripgrep-release-wt2',pathlib.Path(str(RG)+'-wt2'),['run','--','cargo','build','--locked','--release'])
 with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
  jobs=[]
  for i in range(6):
   repo=RG if i%2==0 else TK
   wt=pathlib.Path(str(repo)+f'-wt{1+i%2}')
   args=['run','--','cargo','check','--locked']
   if repo==TK: args+=['-p','tokio@1.45.1','--all-features']
   jobs.append(pool.submit(run,f'mixed-burst-{i}',wt,args))
  for job in jobs:job.result()
 run('final-cache',RG,['cache','ls','--refresh','--json'])
 run('final-gc',RG,['gc','--json'])
 run('final-status',RG,['status','--json'])
 run('final-mbx-stats',RG,['run','--',str(ROOT/'mbx'),'cache','stats','--json'])
