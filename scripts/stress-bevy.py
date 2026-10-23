#!/usr/bin/env python3
import os, pathlib, subprocess, json, runpy, time, concurrent.futures, psutil
import argparse, shutil
parser=argparse.ArgumentParser(description="Isolated Bevy 0.16.1 worktree, cache and concurrency benchmark (Linux; psutil required).")
parser.add_argument('--root',type=pathlib.Path,required=True,help='New output directory; needs roughly 12 GiB free')
parser.add_argument('--buildq',type=pathlib.Path,required=True,help='Release buildq executable')
options=parser.parse_args()
R=options.root.resolve()
if R.exists(): parser.error('--root must not exist; existing caches are never deleted')
R.mkdir(parents=True)
BQ=str(options.buildq.resolve())
SCRIPT_DIR=pathlib.Path(__file__).resolve().parent
measure=runpy.run_path(str(SCRIPT_DIR/'benchmark-heavy.py'))['measure']
shutil.copytree(SCRIPT_DIR/'fixtures/bevy-heavy',R/'wt1')
(R/'state').mkdir()
(R/'state/config.toml').write_text('slots = 2\npoll_ms = 50\n[cache]\nmax_members = 2\nbudget = "12G"\nmin_free = "3G"\ngc_after_job = false\n[cargo]\njobs = 2\n')
env={k:v for k,v in os.environ.items() if not k.startswith('BUILDQ_') and k != 'CARGO_TARGET_DIR'}
env.update(BUILDQ_DIR=str(R/'state'),BUILDQ_CACHE=str(R/'cache'),BUILDQ_GC='0',BUILDQ_TRACE='1',CARGO_BUILD_JOBS='2',RUSTC_WRAPPER='')
def run(name,wt,args,bq=True,extra=None,expected=0):
    e=env.copy();e.update(extra or {});e['BUILDQ_LABEL']=name
    cmd=[BQ,'cargo']+args if bq else ['cargo']+args
    row=measure(name,R/wt,R/'logs',cmd,e)
    assert row['exit']==expected,(name,row)
    if '-warm-' in name or name == 'worktree2-first':
        assert row['compiling_lines'] == 0, (name, row)
    return row

def git(*a):
    subprocess.run(['git','-C',str(R/'wt1'),'-c','user.name=benchmark','-c','user.email=benchmark@example.invalid',*a],check=True,stdout=subprocess.DEVNULL)
# Dependencies are fetched outside all timed compile comparisons.
with (R/'fetch.log').open('w') as f:
    subprocess.run(['cargo','fetch','--locked'],cwd=R/'wt1',env=env,stdout=f,stderr=f,check=True)
run('baseline-cold','wt1',['build','--offline','--locked','--target-dir',str(R/'baseline')],False)
git('init','-q');git('add','.');git('commit','-qm','Pinned Bevy heavy probe')
for i in range(2,5): git('worktree','add','--detach',str(R/f'wt{i}'))
run('buildq-cold','wt1',['build','--offline','--locked'])
for i in range(10):
    run(f'direct-warm-{i}','wt1',['build','--offline','--locked','--target-dir',str(R/'baseline')],False)
    run(f'buildq-warm-{i}','wt1',['build','--offline','--locked'])
run('worktree2-first','wt2',['build','--offline','--locked'])
p=R/'wt2/src/main.rs'; original=p.read_text(); p.write_text(original.replace('version-A','version-B'));os.utime(p,(1_600_000_000,1_600_000_000))
run('naive-old-mtime','wt2',['run','--offline','--locked','--target-dir',str(R/'baseline')],False)
run('buildq-old-mtime','wt2',['run','--offline','--locked'])
assert (R/'logs/buildq-old-mtime.stdout').read_text().strip()=='version-B:entities=1'
run('worktree1-return','wt1',['run','--offline','--locked'])
assert (R/'logs/worktree1-return.stdout').read_text().strip()=='version-A:entities=1'
p.write_text(original+'\nthis is deliberately invalid Rust\n')
run('compile-failure','wt2',['build','--offline','--locked'],expected=101)
p.write_text(original)
run('after-failure','wt2',['run','--offline','--locked'])
assert (R/'logs/after-failure.stdout').read_text().strip()=='version-A:entities=1'
# First concurrent pooled wave: one existing member, a second must build from cold.
def wave(prefix,bq,extra=None):
    start=time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=3) as ex:
        futures=[]
        for i in range(2,5):
            args=['build','--offline','--locked']
            if not bq: args += ['--target-dir',str(R/f'direct-{i}')]
            futures.append(ex.submit(run,f'{prefix}-{i}',f'wt{i}',args,bq,extra))
            time.sleep(.03)
        peak_rss=peak_rustc=0
        while not all(f.done() for f in futures):
            rss=rustc=0
            for child in psutil.Process().children(recursive=True):
                try:
                    rss += child.memory_info().rss
                    rustc += child.name() == 'rustc'
                except psutil.NoSuchProcess: pass
            peak_rss=max(peak_rss,rss);peak_rustc=max(peak_rustc,rustc)
            time.sleep(.05)
        rows=[f.result() for f in futures]
    if bq:
        history=[json.loads(line) for line in (R/'state/history.jsonl').read_text().splitlines()]
        jobs=[job for job in history if (job.get('label') or '').startswith(prefix+'-') and job['class']=='build']
        events=sorted([(j['started_ms'],1) for j in jobs]+[(j['ended_ms'],-1) for j in jobs])
        active=peak=0
        for _,delta in events:
            active+=delta; peak=max(peak,active)
        assert peak <= 2, (prefix, peak)
        cache=pathlib.Path((extra or {}).get('BUILDQ_CACHE',str(R/'cache')))
        assert len(list(cache.glob('pools/*/m[0-9]*.json'))) <= 2
    (R/f'{prefix}-wave.json').write_text(json.dumps({'wall_s':time.monotonic()-start,'peak_tree_rss_bytes':peak_rss,'peak_rustc_processes':peak_rustc,'jobs':rows},indent=2))
wave('pooled-concurrent',True)
wave('pooled-warm-concurrent',True)
# Quantify unconstrained independent target dirs, same -j2 per cargo.
wave('direct-concurrent',False)
# Empty pool versus three independent empty target dirs: both start cold.
wave('pooled-all-cold',True,{'BUILDQ_CACHE':str(R/'cache-all-cold')})
# Run class and target lease must be released while application still sleeps.
with concurrent.futures.ThreadPoolExecutor(max_workers=2) as ex:
    f=ex.submit(run,'sleeping-app','wt1',['run','--offline','--locked'],True,{'PROBE_SLEEP_MS':'5000'})
    deadline=time.monotonic()+30
    while time.monotonic()<deadline:
        status=subprocess.check_output([BQ,'status','--json'],env=env,text=True)
        classes=json.loads(status)['classes']
        if any(c['class']=='run' and any(h['busy'] and (h.get('job') or {}).get('label')=='sleeping-app' for h in c['holders']) for c in classes):
            break
        time.sleep(.05)
    else:
        raise RuntimeError('application did not acquire a run slot within 30s')
    (R/'run-overlap-status.json').write_text(status)
    run('build-during-app','wt2',['build','--offline','--locked'])
    assert not f.done(), 'build must finish while the app is still running'
    f.result()
with (R/'dynamic-metadata.json').open('w') as f:
    subprocess.run(['cargo','metadata','--format-version=1','--features','bevy/dynamic_linking'],cwd=R/'wt1',env=env,stdout=f,check=True)
run('bevy-dynamic-linking','wt1',['run','--offline','--locked','--features','bevy/dynamic_linking'])
assert (R/'logs/bevy-dynamic-linking.stdout').read_text().strip()=='version-A:entities=1'
# Exercise the public PATH-based shim as well as the explicit buildq cargo entrypoint.
subprocess.run([BQ,'shim','install','cargo','--into',str(R/'shim')],env=env,check=True)
shim_env={'PATH':str(R/'shim')+os.pathsep+env['PATH']}
for i in range(10):
    run(f'shim-warm-{i}','wt4',['build','--offline','--locked'],False,shim_env)
run('actual-shim-run','wt4',['run','--offline','--locked'],False,shim_env)
assert (R/'logs/actual-shim-run.stdout').read_text().strip()=='version-A:entities=1'
for command,name in [(['cache','ls','--refresh','--json'],'cache-inventory'),(['history','-n','200','--json'],'history')]:
    (R/f'{name}.json').write_bytes(subprocess.check_output([BQ,*command],env=env))
print(f'Results: {R}',flush=True)
