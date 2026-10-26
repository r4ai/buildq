#!/usr/bin/env python3
"""Render source-backed figures. Run beside measurements.jsonl (matplotlib)."""
from pathlib import Path
import json
import statistics
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
ROOT=Path(__file__).resolve().parent
rows=[json.loads(line) for line in (ROOT/'measurements.jsonl').read_text().splitlines()]
colors=['#577ca7','#7360a8']
plt.rcParams.update({'font.family':'DejaVu Sans','axes.spines.top':False,'axes.spines.right':False,'font.size':11,'figure.facecolor':'white','savefig.facecolor':'white'})
fig,axes=plt.subplots(1,2,figsize=(10,4.8))
for ax,key,scale,title in [(axes[0],'wall_s',1,'Wall time (seconds)'),(axes[1],'peak_tree_rss',2**20,'Sampled peak process-tree RSS (MiB)')]:
 for index,backend in enumerate(['cargo','mbx']):
  subset=[r for r in rows if r['name'].startswith('paired-') and r['name'].endswith(f'-{backend}-rebuild')]
  vals=[r[key]/scale for r in subset]
  median=statistics.median(vals)
  ax.bar(index,median,color=colors[index],width=.55,alpha=.85)
  ax.scatter([index+(.07*(i-1)) for i in range(len(vals))],vals,c='#233343',s=20,zorder=3)
  ax.text(index,max(vals)*1.05,f'{median:.2f}',ha='center',fontweight='bold')
 ax.set_xticks([0,1],['buildq + Cargo','buildq + mbx'])
 ax.set_title(title,pad=14)
 ax.set_ylim(0,max(ax.get_ylim()[1]*1.12,1))
 ax.grid(axis='y',alpha=.15);ax.set_axisbelow(True)
fig.suptitle('ripgrep 14.1.1 — empty target, prewarmed mbx store',fontsize=15,fontweight='bold')
fig.text(.5,.015,'n=3 alternating backend order; --locked --offline; 4-CPU shared QA host; dots are individual runs.\n100 ms RSS samples can miss short-lived processes. Store population cost is excluded.',ha='center',fontsize=9,color='#4b5563')
fig.tight_layout(rect=(0,.105,1,.93))
for ext in ['png','svg']:fig.savefig(ROOT/f'ripgrep-cache-restore.{ext}',dpi=160)
plt.close(fig)
# A chronological output-correctness matrix, not a performance graph.
results=json.loads((ROOT/'nested-portable/results.json').read_text())
names=['A','B','A-again','B-clean'];fig,ax=plt.subplots(figsize=(10,3.4));ax.set_xlim(-.5,3.5);ax.set_ylim(-.5,1.5)
for y,backend in enumerate(['cargo','mbx']):
 for x,name in enumerate(names):
  r=next(r for r in results if r['backend']==backend and r['name']==name)
  ok=r['passed'];ax.add_patch(plt.Rectangle((x-.45,y-.35),.9,.7,color='#e0efe7' if ok else '#f5dce0',lw=0))
  ax.text(x,y,f"{r['output']}\n{'correct' if ok else 'STALE'}",ha='center',va='center',color='#1f5f46' if ok else '#9b1c32',fontsize=11)
ax.set_xticks(range(4),['Build A','Switch to B','Return to A','Clear pool, build B']);ax.set_yticks([0,1],['Cargo backend','mbx backend']);ax.tick_params(length=0)
for spine in ax.spines.values():spine.set_visible(False)
ax.set_title('Nested helper worktree switch — expected B when building B',fontsize=14,fontweight='bold',pad=15)
fig.text(.5,.015,'Both worktrees and their different sources exist before the first build. No timestamp rewriting.\nThe mbx execution cache preserves the stale B result after the shared target is cleared.',ha='center',fontsize=9,color='#4b5563')
fig.tight_layout(rect=(0,.13,1,1))
for ext in ['png','svg']:fig.savefig(ROOT/f'nested-output-correctness.{ext}',dpi=160)
bevy=[r for r in rows if r['name'].startswith('bevy-')]
if bevy:
 cold=next(r['wall_s'] for r in bevy if r['name']=='bevy-mbx-cold')
 restores=[r['wall_s'] for r in bevy if r['name'] in [f'bevy-restore-{i}' for i in range(3)]]
 warm=[r['wall_s'] for r in bevy if r['name'] in [f'bevy-restore-{i}-warm' for i in range(3)]]
 fig,ax=plt.subplots(figsize=(9,4.5));vals=[cold,statistics.median(restores),statistics.median(warm)]
 ax.barh(range(3),vals,color=['#577ca7','#7360a8','#4d8a78'],height=.55)
 ax.set_yticks(range(3),['First build + store population (n=1)','Empty target, warm store (n=3)','Warm target (n=3)'])
 ax.set_xscale('log');ax.set_xlim(.1,1000);ax.set_xlabel('Wall seconds (log scale)');ax.grid(axis='x',alpha=.15);ax.set_axisbelow(True)
 for i,val in enumerate(vals):ax.text(val*1.12,i,f'{val:.3f}s',va='center',fontweight='bold')
 ax.set_title('Bevy 0.16.1 PBR/PNG/multithread fixture',fontweight='bold',pad=18)
 fig.text(.5,.015,'Initial run includes dependency fetching; three later restores each report269mbx cache hits.\nDifferent initial cache states, not a Cargo-vs-mbx comparison. All app markers checked.',ha='center',fontsize=9,color='#4b5563')
 fig.tight_layout(rect=(0,.12,1,1))
 for ext in ['png','svg']:fig.savefig(ROOT/f'bevy-cache-states.{ext}',dpi=160)
