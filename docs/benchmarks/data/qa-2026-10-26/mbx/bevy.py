#!/usr/bin/env python3
"""Bevy PBR/PNG fixture, cold store, cold targets with hot store, source changes."""
import run, pathlib, subprocess, json, os, shutil, concurrent.futures
R=run.ROOT/'bevy';O=run.OUT
A=R/'a'
for command in [['init','-q'],['add','.'],['-c','user.name=buildq QA','-c','user.email=qa@example.invalid','commit','-qm','Bevy 0.16.1 PBR PNG multithread fixture'],['worktree','add','--detach',str(R/'b')],['worktree','add','--detach',str(R/'c')]]:
 subprocess.run(['git','-C',str(A),*command],check=True,stdout=subprocess.DEVNULL)
(R/'state').mkdir(exist_ok=True)
(R/'state/config.toml').write_text(f'''slots=2\n[cache]\nbudget="10G"\nmin_free="512M"\nmax_members=2\ngc_mode="inline"\nseed_members=true\n[cargo]\nbackend="mbx"\nmbx="{os.environ.get('QA_MBX',str(run.ROOT/'mbx'))}"\nmirror_bins=false\n''')
run.ENV.update(BUILDQ_DIR=str(R/'state'),BUILDQ_CACHE=str(R/'cache'),MBX_CACHE_DIR=str(R/'mbx-cache'))
# Record exact fixture and dependency resolution, no source download in timings below.
shutil.copytree(A,O/'bevy-fixture',ignore=shutil.ignore_patterns('.git','target'),dirs_exist_ok=True)
run.run('bevy-mbx-cold',A,['cargo','build','--locked'],timeout=1500)
run.run('bevy-mbx-marker-A',A,['cargo','run','--offline','--quiet'])
for i in range(3):
 run.run(f'bevy-restore-{i}-clean',A,['cache','clean','--all'])
 run.run(f'bevy-restore-{i}',R/'b',['cargo','build','--locked','--offline'],timeout=600)
 run.run(f'bevy-restore-{i}-warm',R/'b',['cargo','build','--locked','--offline'])
# Worktree C changes program output before switching to it; A remains original.
p=R/'c/src/main.rs';p.write_text(p.read_text().replace('version-A','version-C'))
run.run('bevy-changed-C',R/'c',['cargo','run','--offline','--quiet'],timeout=600)
run.run('bevy-back-A',A,['cargo','run','--offline','--quiet'],timeout=600)
# Burst exceeds slots and member cap; one same-source worktree and one edited.
with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
 futures=[pool.submit(run.run,f'bevy-burst-{i}',R/['a','b','c'][i%3],['cargo','run','--offline','--quiet'],600) for i in range(6)]
 for future in futures:future.result()
run.run('bevy-final-cache',A,['cache','ls','--refresh','--json'])
run.run('bevy-final-gc',A,['gc','--json'])
run.run('bevy-final-status',A,['status','--json'])
