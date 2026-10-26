#!/usr/bin/env python3
"""Bounded real-mbx/buildq dual-GC churn with output assertions.

Source Rust first. Run under storage/subreaper.py in containers without init.
This generates only local fixtures; creates a fresh --root, never upstream writes.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import signal
import subprocess
import threading
import time


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--root',type=Path,required=True);p.add_argument('--buildq',type=Path,required=True);p.add_argument('--mbx',type=Path,required=True)
    p.add_argument('--duration',type=float,default=1800);args=p.parse_args()
    root=args.root.resolve();root.mkdir(parents=True,exist_ok=False)
    bq=str(args.buildq.resolve());mbx=str(args.mbx.resolve());out=root/'logs';out.mkdir()
    env=os.environ.copy()
    for key in ['BUILDQ_HELD','BUILDQ_CONFIG','CARGO_TARGET_DIR','RUSTC_WRAPPER','RUSTC_WORKSPACE_WRAPPER','BUILDQ_GC']:env.pop(key,None)
    env.update(BUILDQ_DIR=str(root/'state'),BUILDQ_CACHE=str(root/'cache'),MBX_CACHE_DIR=str(root/'mbx'),MBX_GC_AUTO='1',MBX_GC_INTERVAL='1s',MBX_GC_MAX_SIZE='16MiB',MBX_GC_MAX_TOTAL_SIZE='24MiB',MBX_GC_INCREMENTAL_MAX_SIZE='4MiB',MBX_GC_MIN_FREE_SIZE='0',MBX_REMOTE_URL='',CARGO_BUILD_JOBS='1',CARGO_INCREMENTAL='0')
    (root/'state').mkdir();(root/'state/config.toml').write_text(f'''slots=2\n[cache]\nbudget="32M"\nmin_free="0"\nmax_members=2\ngc_mode="inline"\nseed_members=true\n[cargo]\nbackend="mbx"\nmbx={json.dumps(mbx)}\nnested_mbx=true\njobs=1\nmirror_bins=false\n''')
    repo=root/'repo';(repo/'src').mkdir(parents=True)
    (repo/'Cargo.toml').write_text('[package]\nname="dual-gc-probe"\nversion="0.1.0"\nedition="2021"\n[features]\nextra=[]\n[profile.dev]\ndebug=0\n')
    (repo/'src/main.rs').write_text('fn main(){println!("seed");}\n')
    (repo/'.gitignore').write_text('/target\n')
    for cmd in [['git','init','-q'],['git','add','.'],['git','-c','user.name=QA','-c','user.email=qa@example.invalid','commit','-qm','fixture']]:subprocess.run(cmd,cwd=repo,check=True,env=env)
    wts=[]
    for i in range(2):
        wt=root/f'wt{i}';subprocess.run(['git','worktree','add','--detach',str(wt)],cwd=repo,env=env,check=True,stdout=subprocess.DEVNULL);wts.append(wt)
    lock=threading.Lock();rows=[];start=time.monotonic();deadline=start+args.duration
    def emit(row):
        row['elapsed_s']=round(time.monotonic()-start,3)
        with lock:
            rows.append(row)
            with (out/'events.jsonl').open('a') as f:f.write(json.dumps(row)+'\n')
    def command(kind,argv,cwd=repo,timeout=120):
        begin=time.monotonic();z=subprocess.run(argv,cwd=cwd,env=env,capture_output=True,text=True,timeout=timeout)
        row=dict(kind=kind,argv=argv,cwd=str(cwd),exit=z.returncode,wall_s=round(time.monotonic()-begin,3),stdout=z.stdout,stderr=z.stderr)
        return row
    def worker(index):
        wt=wts[index];n=0
        while time.monotonic()<deadline:
            marker=f'worker-{index}-edit-{n}'
            text=f'fn main(){{let x=std::hint::black_box([({n}%251) as u8;262144]);println!("{marker} extra={{}} data={{}}",cfg!(feature="extra"),x[0]);}}\n'
            src=wt/'src/main.rs';src.write_text(text)
            if n%3==0:os.utime(src,(946684800,946684800))
            argv=[bq,'cargo','run','--offline','--quiet']
            if n%2:argv+=['--features','extra']
            row=command('job',argv,wt)
            expected=f'{marker} extra={str(bool(n%2)).lower()} data={n%251}'
            row.update(worker=index,index=n,expected=expected,passed=row['exit']==0 and row['stdout'].strip()==expected)
            emit(row);n+=1
        return n
    def sampler():
        n=0
        while time.monotonic()<deadline:
            if os.statvfs(root).f_bavail*os.statvfs(root).f_frsize<6*1024**3:
                emit(dict(kind='disk_stop',passed=False));break
            emit(command('status',[bq,'status','--json']))
            emit(command('mbx_stats',[mbx,'cache','stats','--json']))
            cached=root/'cache/mbx-usage.json'
            if cached.exists():
                try:emit(dict(kind='cached_usage',data=json.loads(cached.read_text())))
                except (OSError,ValueError):pass
            if n%2==0:emit(command('buildq_gc',[bq,'gc','--json']))
            if n%6==0:emit(command('mbx_gc',[mbx,'gc','--json']))
            if n%8==0:emit(command('fresh_inventory',[bq,'cache','ls','--refresh','--json']))
            n+=1
            for _ in range(15):
                if time.monotonic()>=deadline:break
                time.sleep(1)
    try:
        with ThreadPoolExecutor(max_workers=3) as pool:
            futures=[pool.submit(worker,i) for i in range(2)];sample=pool.submit(sampler)
            for f in futures:f.result()
            sample.result()
    finally:
        emit(command('final_mbx_gc',[mbx,'gc','--json']))
        emit(command('final_buildq_gc',[bq,'gc','--json']))
        emit(command('final_inventory',[bq,'cache','ls','--refresh','--json']))
        emit(command('final_status',[bq,'status','--json']))
        jobs=[x for x in rows if x['kind']=='job'];bad=[x for x in jobs if not x['passed']]
        summary=dict(seconds=time.monotonic()-start,jobs=len(jobs),bad_jobs=len(bad),passed=not bad,action_counts={k:sum(x['kind']==k for x in rows) for k in sorted({x['kind'] for x in rows})},environment={k:env[k] for k in ['MBX_GC_AUTO','MBX_GC_INTERVAL','MBX_GC_MAX_SIZE','MBX_GC_MAX_TOTAL_SIZE','MBX_GC_INCREMENTAL_MAX_SIZE']})
        (out/'summary.json').write_text(json.dumps(summary,indent=2)+'\n');print(json.dumps(summary),flush=True)
    return int(bool(bad))


if __name__=='__main__':raise SystemExit(main())
