#!/usr/bin/env python3
"""Runtime probes for mbx budget reporting; only touches --root."""
import argparse, json, os, pathlib, subprocess, time
p=argparse.ArgumentParser(); p.add_argument('--root',type=pathlib.Path,required=True); p.add_argument('--buildq',required=True); p.add_argument('--mbx',required=True); a=p.parse_args()
r=a.root.resolve(); r.mkdir(parents=True,exist_ok=True); logs=r/'logs'; logs.mkdir(exist_ok=True)
e=os.environ.copy(); e.update(BUILDQ_DIR=str(r/'queue'),BUILDQ_CACHE=str(r/'cache'),MBX_CACHE_DIR=str(r/'mbx-a'),MBX_GC_AUTO='0',CARGO_BUILD_JOBS='1',BUILDQ_GC='0'); e.pop('CARGO_TARGET_DIR',None)
(r/'queue').mkdir(exist_ok=True)
(r/'queue/config.toml').write_text(f'''slots=1\n[cache]\nbudget="1K"\nmin_free="0"\naccounting="logical"\ndedupe="off"\ngc_after_job=false\n[cargo]\nbackend="mbx"\nmbx={json.dumps(a.mbx)}\njobs=1\n''')
repo=r/'repo'; (repo/'src').mkdir(parents=True,exist_ok=True)
(repo/'Cargo.toml').write_text('[package]\nname="accounting-probe"\nversion="0.1.0"\nedition="2021"\n')
(repo/'src/main.rs').write_text('fn main() { println!("accounting-ok"); }\n')
for cmd in [['git','init'],['git','add','.'],['git','-c','user.name=QA','-c','user.email=qa@example.invalid','commit','-m','fixture']]: subprocess.run(cmd,cwd=repo,env=e,capture_output=True,check=True)
rows=[]
def run(name,cmd,extra=None):
    env=e.copy(); env.update(extra or {}); start=time.monotonic(); z=subprocess.run(cmd,cwd=repo,env=env,capture_output=True,text=True,timeout=180)
    (logs/(name+'.stdout')).write_text(z.stdout); (logs/(name+'.stderr')).write_text(z.stderr)
    row={'name':name,'argv':cmd,'seconds':time.monotonic()-start,'code':z.returncode};
    try: row['json']=json.loads(z.stdout)
    except ValueError: pass
    rows.append(row); (r/'results.json').write_text(json.dumps(rows,indent=2)); return row
run('build',[a.buildq,'cargo','build','--offline'])
run('fresh_before_gc',[a.buildq,'cache','ls','--refresh','--json'])
run('gc',[a.buildq,'gc','--json'])
run('fresh_after_gc',[a.buildq,'cache','ls','--refresh','--json'])
run('gc_again',[a.buildq,'gc','--json'])
# Change MBX_CACHE_DIR while retaining the machine-wide buildq cache.
run('store_b_gc_cached',[a.buildq,'gc','--json'],{'MBX_CACHE_DIR':str(r/'mbx-b')})
run('store_b_fresh',[a.buildq,'cache','ls','--json'],{'MBX_CACHE_DIR':str(r/'mbx-b')})
run('store_a_gc_cached',[a.buildq,'gc','--json'])
run('store_a_fresh',[a.buildq,'cache','ls','--json'])
# A 6 MiB budget fits either the ordinary target or store A alone. Store B is
# empty: using A's cached measurement while collecting B causes needless loss.
config=r/'queue/config.toml'
original=config.read_text()
config.write_text(original.replace('budget="1K"','budget="6M"').replace('backend="mbx"','backend="cargo"'))
run('identity_plain_build',[a.buildq,'cargo','build','--offline'])
config.write_text(original.replace('budget="1K"','budget="6M"'))
run('identity_a_fresh',[a.buildq,'cache','ls','--refresh','--json'])
run('identity_b_stats',[a.mbx,'cache','stats','--json'],{'MBX_CACHE_DIR':str(r/'mbx-b')})
run('identity_b_gc',[a.buildq,'gc','--json'],{'MBX_CACHE_DIR':str(r/'mbx-b')})
run('identity_b_fresh_after',[a.buildq,'cache','ls','--refresh','--json'],{'MBX_CACHE_DIR':str(r/'mbx-b')})
for row in rows:
    j=row.get('json',{}); print(row['name'],row['code'],{k:j[k] for k in ('total','after','budget','effective_budget','over_budget','over_budget_bytes','mbx_budget_bytes','mbx') if k in j})
