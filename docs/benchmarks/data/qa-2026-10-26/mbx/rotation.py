#!/usr/bin/env python3
"""Bounded mbx project rotation: real OSS, edited worktree, storage pressure."""
import run, pathlib, os, time, concurrent.futures, json
R=run.ROOT/'bevy';A=R/'a';C=R/'c';source=C/'src/main.rs';original=source.read_text()
run.ENV.update(BUILDQ_DIR=str(R/'state'),BUILDQ_CACHE=str(R/'cache'),MBX_CACHE_DIR=str(R/'mbx-cache'),CARGO_BUILD_JOBS='1')
start=time.monotonic();iteration=0;checks=[]
def config(budget):
 (R/'state/config.toml').write_text(f'''slots=2\n[cache]\nbudget="{budget}"\nmin_free="512M"\nmax_members=2\ngc_mode="inline"\nseed_members=true\n[cargo]\nbackend="mbx"\nmbx="{os.environ.get('QA_MBX',str(run.ROOT/'mbx'))}"\nmirror_bins=false\n''')
try:
 while time.monotonic()-start<720:
  budget='768M' if iteration%3==2 else '4G'
  config(budget)
  marker='version-C' if iteration%2==0 else 'version-D'
  source.write_text(original.replace('version-C',marker))
  specs=[('bevy-A',A,['cargo','run','--offline','--quiet'],'version-A:entities=1'),('bevy-edited',C,['cargo','run','--offline','--quiet'],marker+':entities=1'),('ripgrep',run.RG,['cargo','build','--locked','--offline'],None),('ra',run.ROOT/'inputs/rust-analyzer-2025-06-02',['cargo','check','--locked','--offline','-p','rust-analyzer'],None),('tokio',run.TK,['cargo','check','--locked','--offline','-p','tokio@1.45.1','--all-features'],None)]
  with concurrent.futures.ThreadPoolExecutor(max_workers=5) as pool:
   jobs=[(name,expected,pool.submit(run.run,f'rotation-{iteration}-{name}',cwd,args,900)) for name,cwd,args,expected in specs]
   for name,expected,future in jobs:
    record=future.result();actual=(run.OUT/(record['name']+'.stdout')).read_text().strip()
    checks.append({'iteration':iteration,'project':name,'budget':budget,'expected':expected,'actual':actual if expected else None,'exit':record['exit'],'passed':record['exit']==0 and (expected is None or actual==expected)})
  run.run(f'rotation-{iteration}-gc',A,['gc','--json'])
  run.run(f'rotation-{iteration}-cache',A,['cache','ls','--refresh','--json'])
  run.run(f'rotation-{iteration}-status',A,['status','--json'])
  iteration+=1
finally:
 source.write_text(original)
 config('10G')
 (run.OUT/'rotation-correctness.json').write_text(json.dumps({'duration_s':round(time.monotonic()-start,3),'iterations':iteration,'checks':checks,'passed':all(c['passed'] for c in checks)},indent=2)+'\n')
run.run('rotation-final-gc',A,['gc','--json'])
run.run('rotation-final-status',A,['status','--json'])
