#!/usr/bin/env python3
"""Real mbx artifacts + second real Cargo member isolate stale sharing rebates."""
import argparse,json,os,subprocess,time
from pathlib import Path
p=argparse.ArgumentParser(description=__doc__);p.add_argument('--root',type=Path,required=True);p.add_argument('--buildq',required=True);p.add_argument('--mbx',required=True);p.add_argument('--lockfile',type=Path);a=p.parse_args()
r=a.root.resolve();r.mkdir(parents=True,exist_ok=False);(r/'state').mkdir();(r/'logs').mkdir();repo=r/'repo';(repo/'src').mkdir(parents=True)
(repo/'Cargo.toml').write_text('[package]\nname="rebate-probe"\nversion="0.1.0"\nedition="2021"\n[dependencies]\nregex="1"\n');(repo/'src/main.rs').write_text('fn main(){assert!(regex::Regex::new("^rebate-ok$").unwrap().is_match("rebate-ok"));println!("rebate-ok");}\n')
if a.lockfile:(repo/'Cargo.lock').write_bytes(a.lockfile.read_bytes())
e=os.environ.copy()
for key in ['BUILDQ_CONFIG','BUILDQ_HELD','CARGO_TARGET_DIR','RUSTC_WRAPPER']:e.pop(key,None)
e.update(BUILDQ_DIR=str(r/'state'),BUILDQ_CACHE=str(r/'cache'),MBX_CACHE_DIR=str(r/'mbx'),MBX_GC_AUTO='0',BUILDQ_GC='0',CARGO_BUILD_JOBS='1')
config=r/'state/config.toml';base=f'''slots=2\n[cache]\nbudget="32M"\nmin_free="0"\naccounting="logical"\ndedupe="off"\ngc_after_job=false\n[cargo]\nbackend="mbx"\nmbx={json.dumps(a.mbx)}\njobs=1\n''';config.write_text(base)
for cmd in [['git','init','-q'],['git','add','.'],['git','-c','user.name=QA','-c','user.email=qa@example.invalid','commit','-qm','fixture']]:subprocess.run(cmd,cwd=repo,env=e,check=True)
rows=[]
def run(name,args):
 begin=time.monotonic();z=subprocess.run(args,cwd=repo,env=e,capture_output=True,text=True,timeout=180);(r/'logs'/f'{name}.stdout').write_text(z.stdout);(r/'logs'/f'{name}.stderr').write_text(z.stderr)
 row=dict(name=name,argv=args,exit=z.returncode,seconds=time.monotonic()-begin,stdout=z.stdout,stderr=z.stderr)
 try:row['json']=json.loads(z.stdout)
 except ValueError:pass
 rows.append(row);(r/'results.json').write_text(json.dumps(rows,indent=2)+'\n');return row
run('populate',[a.buildq,'cargo','build','--offline']);run('clean',[a.buildq,'cache','clean','--all']);run('restore',[a.buildq,'cargo','build','--offline'])
pool=next((r/'cache/pools').iterdir());plain=pool/'m1'
run('plain_second_member',['cargo','build','--offline','--target-dir',str(plain)])
now=int(time.time()*1000)
(pool/'m1.json').write_text(json.dumps(dict(id=1,created_ms=now,last_used_ms=now)))
meta=json.loads((pool/'m0.json').read_text());meta['last_used_ms']=now-10000;(pool/'m0.json').write_text(json.dumps(meta))
initial=run('initial_inventory',[a.buildq,'cache','ls','--refresh','--json'])['json']
assert initial['mbx']['shared_with_mbx_bytes'] > 1024*1024, 'Need real mbx-shared dependencies to isolate the rebate'
private=next(m['size'] for pool_info in initial['pools'] for m in pool_info['members'] if m['id']==1)
budget=initial['mbx']['combined_bytes']+private-1024*1024
config.write_text(base.replace('budget="32M"',f'budget="{budget}"'))
run('fresh_before',[a.buildq,'cache','ls','--refresh','--json'])
(r/'cached_before.json').write_bytes((r/'cache/mbx-usage.json').read_bytes())
run('mbx_before',[a.mbx,'cache','stats','--json'])
run('gc',[a.buildq,'gc','--json'])
(r/'cached_after_gc.json').write_bytes((r/'cache/mbx-usage.json').read_bytes())
run('mbx_after',[a.mbx,'cache','stats','--json'])
run('fresh_after',[a.buildq,'cache','ls','--refresh','--json'])
for row in rows:
 j=row.get('json',{});print(row['name'],row['exit'],{k:j[k] for k in ['total','after','effective_budget','over_budget','mbx_budget_bytes','mbx','combined_total_bytes'] if k in j})
