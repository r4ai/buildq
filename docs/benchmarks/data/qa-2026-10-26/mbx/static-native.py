#!/usr/bin/env python3
"""Rust plus bundled PCRE2 C compilation and cross-member restore/feature churn."""
import run, concurrent.futures, pathlib, json, subprocess, threading, time, os
R=run.ROOT/'static-native-static';R.mkdir(exist_ok=True);(R/'state').mkdir(exist_ok=True)
(R/'state/config.toml').write_text(f'''slots=2\n[cache]\nbudget="4G"\nmin_free="512M"\nmax_members=2\ngc_mode="inline"\nseed_members=true\n[cargo]\nbackend="mbx"\nmbx="{os.environ.get('QA_MBX',str(run.ROOT/'mbx'))}"\nmirror_bins=false\n''')
run.ENV.update(BUILDQ_DIR=str(R/'state'),BUILDQ_CACHE=str(R/'cache'),MBX_CACHE_DIR=str(R/'mbx-cache'),CARGO_BUILD_JOBS='1',PCRE2_SYS_STATIC='1')
RG=run.RG
run.run('static-native-pcre2-cold',RG,['cargo','build','--locked','--features','pcre2'],timeout=900)
run.run('static-native-pcre2-version',RG,['cargo','run','--locked','--features','pcre2','--','--pcre2-version'])
run.run('static-native-pcre2-before-cache',RG,['cache','ls','--refresh','--json'])
run.run('static-native-pcre2-clean',RG,['cache','clean','--all'])
stop=threading.Event()
def sample():
 with (run.OUT/'static-native-burst-status.jsonl').open('w') as out:
  while not stop.is_set():
   p=subprocess.run([str(run.BQ),'status','--json'],env=run.ENV,capture_output=True,text=True,timeout=10)
   try:
    obj=json.loads(p.stdout);out.write(json.dumps({'at':time.time(),'status':obj})+'\n');out.flush()
   except ValueError: pass
   stop.wait(.25)
t=threading.Thread(target=sample);t.start()
try:
 with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
  jobs=[]
  for i in range(8):
   wt=pathlib.Path(str(RG)+f'-wt{1+i%2}')
   args=['cargo','build','--locked','--offline']
   if i%3!=0:args+=['--features','pcre2']
   if i%4==0:args+=['--no-default-features']
   jobs.append(pool.submit(run.run,f'static-native-burst-{i}',wt,args))
  for job in jobs:job.result()
finally:stop.set();t.join()
run.run('static-native-final-cache',RG,['cache','ls','--refresh','--json'])
run.run('static-native-final-gc',RG,['gc','--json'])
run.run('static-native-final-status',RG,['status','--json'])
