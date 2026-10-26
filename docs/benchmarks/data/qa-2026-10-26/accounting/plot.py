#!/usr/bin/env python3
"""Regenerate accounting/correctness figures from retained experiment records."""
import json
import os
from pathlib import Path
os.environ.setdefault('MPLCONFIGDIR','/tmp/buildq-qa-mpl')
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt

ROOT=Path(__file__).resolve().parent
OUT=ROOT/'assets';OUT.mkdir(exist_ok=True)
plt.rcParams.update({'font.family':'DejaVu Sans','font.size':11,'svg.fonttype':'none','axes.spines.top':False,'axes.spines.right':False})

def save(fig,name):
    fig.savefig(OUT/(name+'.png'),dpi=170,bbox_inches='tight')
    fig.savefig(OUT/(name+'.svg'),bbox_inches='tight',metadata={'Date':None})
    plt.close(fig)

records=json.loads((ROOT/'accounting/results.json').read_text())
lookup={r['name']:r.get('json',{}) for r in records}
before=lookup['fresh_before_gc'];after=lookup['fresh_after_gc'];mib=1024**2
fig,ax=plt.subplots(figsize=(9,5.2),layout='constrained')
pool=[before['total']/mib,after['total']/mib]
store=[before['mbx']['combined_bytes']/mib,after['mbx']['combined_bytes']/mib]
ax.bar(['Before GC','After GC'],pool,color='#497ca8',label='buildq pool (logical bytes)')
ax.bar(['Before GC','After GC'],store,bottom=pool,color='#e3a94b',label='mbx private/store bytes')
ax.axhline(after['budget']/mib,color='#bf3b42',linestyle='--',label='Configured budget: 1 KiB')
ax.set_ylabel('Combined footprint (MiB, logical)')
ax.set_title('Pool eviction leaves mbx storage above budget')
ax.text(1,store[1]+.2,f"{after['mbx']['combined_bytes']:,} bytes remain\nJSON over_budget = false",ha='center',color='#ad2835',fontweight='bold')
ax.set_ylim(0,9);ax.legend(loc='upper right',frameon=False)
fig.text(.04,-.03,'Real mbx 1.27.0 · buildq 1391d00 · no shared inodes in this fixture · issue #138',fontsize=10)
save(fig,'mbx-budget-overflow')

variants=[('Default','nested-independent'),('Nested mbx off','nested-nested-off'),('Rust incremental off','nested-incremental-off'),('mbx script execution off','nested-mbx-build-script-off'),('Worktree-local helper target','nested-local-helper-target')]
rows=[]
for label,directory in variants:
    directory=ROOT/directory if directory=='nested-independent' else ROOT/'nested-controls'/directory
    if not (directory/'results.json').exists():continue
    data=json.loads((directory/'results.json').read_text())
    values=[]
    for backend,name in [('cargo','B'),('cargo','B-clean'),('mbx','B'),('mbx','B-clean')]:
        row=next(x for x in data if x['backend']==backend and x['name']==name)
        values.append(row['output'].removeprefix('generated='))
    rows.append([label,*values])
fig,ax=plt.subplots(figsize=(11,4.4),layout='constrained');ax.axis('off')
table=ax.table(cellText=rows,colLabels=['Control','Cargo B','Cargo after clean','mbx B','mbx after clean'],cellLoc='center',loc='center',colWidths=[.34,.14,.18,.14,.2])
table.auto_set_font_size(False);table.set_fontsize(10);table.scale(1,2.1)
for (r,c),cell in table.get_celld().items():
    cell.set_edgecolor('white')
    if r==0:cell.set_facecolor('#e3e9ef');cell.set_text_props(weight='bold')
    elif c>0:
        good=rows[r-1][c]=='B';cell.set_facecolor('#d8eddf' if good else '#f5d2d5');cell.set_text_props(color='#17663c' if good else '#a32435',weight='bold')
    else:cell.set_facecolor('#f3f5f7')
ax.set_title('Nested codegen after switching to worktree B\nExpected output in every result cell: B',pad=18)
fig.text(.03,.015,'All Cargo invocations exit 0. Both checkouts predate compilation; no mtime rewriting. Issue #142.',fontsize=10)
save(fig,'nested-correctness')

rebate=json.loads((ROOT/'eviction-rebate/results.json').read_text())
rb={x['name']:x.get('json',{}) for x in rebate}
fig,ax=plt.subplots(figsize=(9,4.8),layout='constrained')
vals=[rb['gc']['effective_budget']/mib,rb['fresh_after']['effective_budget']/mib,rb['fresh_after']['total']/mib]
ax.barh(['GC cached pool allowance','Fresh pool allowance','Retained private member'],vals,color=['#8ca7bc','#497ca8','#d25057'])
for y,value in enumerate(vals):ax.text(value+.8,y,f'{value:.2f} MiB',va='center')
ax.set_xlim(0,max(vals)*1.2);ax.set_xlabel('Bytes available / retained (MiB)');ax.invert_yaxis()
ax.set_title('Eviction invalidates the sharing rebate, but GC keeps using it')
fig.text(.025,-.03,'Real mbx store unchanged before/after GC: 76,782,744 bytes. Refreshed overflow: exactly 1 MiB. Issue #151.',fontsize=9)
save(fig,'stale-sharing-rebate')

burst=json.loads((ROOT.parent/'oss/logs/burst64/results.json').read_text())
fig,ax=plt.subplots(figsize=(10,5.1),layout='constrained')
colors={'bevy':'#497ca8','tokio':'#499471','ripgrep':'#b38b3f','rust-analyzer':'#946ca6'}
for repo,color in colors.items():
 points=[(i,j['finished_s']) for i,j in enumerate(burst['jobs']) if j['name'].startswith(repo+'-') and j['exit']==0]
 ax.scatter([p[0] for p in points],[p[1] for p in points],label=repo,color=color,s=35,alpha=.85)
for i,j in enumerate(burst['jobs']):
 if j['exit']!=0:
  ax.scatter([i],[j['finished_s']],marker='x',s=120,color='#bb2736',linewidth=3,label='Harness deadline (SIGTERM)')
  ax.annotate(f'Submission {i:02d}: 793 s before queue\n+ 56.8 s queue + 50.4 s run',(i,j['finished_s']),xytext=(14,750),fontsize=9,color='#9b2130',arrowprops={'arrowstyle':'->','color':'#9b2130'})
ax.axhline(900,color='#bb2736',linestyle='--',alpha=.5);ax.set_xlim(-2,66);ax.set_ylim(0,960)
ax.set_xlabel('Submission index (all submitted as one burst)');ax.set_ylabel('End-to-end completion time (s)')
ax.set_title('64 real OSS submissions: 63 successes and one 900 s deadline')
ax.legend(loc='lower right',frameon=False,ncol=2,fontsize=9)
fig.text(.02,-.03,'Two slots · shared host contention and GC-induced cold rebuilds · completions sampled every 2 s · issue #153.',fontsize=9)
save(fig,'pool-admission')
