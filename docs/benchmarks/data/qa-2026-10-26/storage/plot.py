#!/usr/bin/env python3
"""Render the retained storage QA measurements as standalone SVG and PNG."""
import json
import os
from pathlib import Path
import tempfile

os.environ.setdefault('MPLCONFIGDIR',str(Path(tempfile.gettempdir())/'buildq-qa-matplotlib'))
os.environ.setdefault('XDG_CACHE_HOME',str(Path(tempfile.gettempdir())/'buildq-qa-fontcache'))
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
from matplotlib.ticker import MaxNLocator

ROOT=Path(__file__).resolve().parent
OUT=ROOT/'assets'
OUT.mkdir(exist_ok=True)
MIB=1024**2
INK='#162b43';BLUE='#386ea8';ORANGE='#da8554';GREY='#8c9cad';TEAL='#327b72'
plt.rcParams.update({'font.family':'DejaVu Sans','font.size':10,'axes.edgecolor':'#d0d8e0','axes.labelcolor':INK,'text.color':INK,'xtick.color':INK,'ytick.color':INK,'svg.fonttype':'none','figure.facecolor':'#f7f9fc','axes.facecolor':'white','axes.titleweight':'bold'})

def read(name): return json.loads((ROOT/name).read_text())
def beautify(ax):
    ax.spines[['top','right']].set_visible(False)
    ax.grid(axis='y',alpha=.22,zorder=0)
    ax.set_axisbelow(True)
def export(fig,name):
    fig.savefig(OUT/f'{name}.svg',bbox_inches='tight')
    fig.savefig(OUT/f'{name}.png',dpi=180,bbox_inches='tight')
    plt.close(fig)
def bars(ax,labels,values,colors,unit='MiB'):
    ax.bar(range(len(values)),values,color=colors,width=.58,zorder=3)
    ax.set_xticks(range(len(labels)),labels)
    max_value=max(values) or 1
    ax.set_ylim(0,max_value*1.32)
    for x,v in enumerate(values):
        label=(f'{v:,d}' if isinstance(v,int) else f'{v:,.1f}') if v else '0'
        ax.text(x,v+max_value*.035,label,ha='center',va='bottom',weight='bold',fontsize=11)
    ax.set_ylabel(unit)
    beautify(ax)

probes={r['name']:r for r in read('probe-logs/results.json')}
oversize=[probes['oversize-cache']['json']['total']/MIB,probes['oversize-gc']['json']['after']/MIB]
busy=[probes['busy-cache']['json']['total']/MIB,probes['busy-gc']['json']['after']/MIB,probes['busy-after-gc']['json']['after']/MIB]
fig,axes=plt.subplots(1,2,figsize=(11.2,5.5))
fig.subplots_adjust(top=.74,bottom=.25,wspace=.32,left=.08,right=.97)
fig.text(.065,.94,'Storage budget under real Cargo workloads',fontsize=19,weight='bold')
fig.text(.065,.875,'A 64 MiB retention budget allows a larger build; active members remain protected.',fontsize=11)
bars(axes[0],['Build finished','GC finished'],oversize,[BLUE,TEAL])
axes[0].set_title('One oversized build',loc='left',pad=15)
bars(axes[1],['Build active','GC while active','GC after release'],busy,[BLUE,ORANGE,TEAL])
axes[1].set_title('A busy member survives GC',loc='left',pad=15)
for ax in axes:
    ax.axhline(64,color='#b65049',linestyle=(0,(5,3)),linewidth=1.5)
    ax.text(.97,64+3,'64 MiB budget',color='#a34640',ha='right',transform=ax.get_yaxis_transform(),fontsize=9)
fig.text(.065,.125,'Concurrent stress: 120 builds + 120 manual GC calls across 60 cycles.',weight='bold',fontsize=11)
fig.text(.065,.08,'All 60 post-cycle inspections returned 0 retained bytes. Both inline and background automatic GC were exercised.',fontsize=9.4)
fig.text(.065,.035,'buildq 1391d00 · Linux overlayfs · Rust 1.89.0 · real build scripts writing 96 / 128 MiB payloads · MiB = 1,048,576 bytes',fontsize=8,color='#617085')
export(fig,'storage-gc')

metadata=read('metadata-burst-logs/result.json')
snapshot=max(metadata['samples'],key=lambda s:len(s['cargos']))
holders=sum(h['busy'] for c in snapshot['status']['classes'] for h in c['holders'])
rss=read('rss-scope-evidence.json')
fig,axes=plt.subplots(1,2,figsize=(11.2,5.8))
fig.subplots_adjust(top=.70,bottom=.28,wspace=.4,left=.08,right=.97)
fig.text(.065,.945,'Separate preflight work from compilation and host load',fontsize=17,weight='bold')
fig.text(.065,.883,'Live Cargo probes and process ownership snapshots expose two soak-runner measurement errors.',fontsize=10.7)
bars(axes[0],['Cargo metadata\nprocesses','Occupied build\nslots'],[len(snapshot['cargos']),holders],[ORANGE,BLUE],unit='Count in one snapshot')
axes[0].yaxis.set_major_locator(MaxNLocator(integer=True))
axes[0].set_title('1,000-member workspace · 12 requests',loc='left',fontsize=11,pad=15)
axes[0].axhline(2,color='#617085',linestyle=(0,(5,3)),linewidth=1)
axes[0].text(.97,2.4,'Configured build slots: 2',ha='right',transform=axes[0].get_yaxis_transform(),fontsize=8.5,color='#617085')
host=rss['host_wide_peak_kb']/1024
owned=rss['owned_rss_at_host_peak_kb']/1024
bars(axes[1],["Runner's mixed-scope\nprocess list",'Owned experiment\nprocess groups'],[host,owned],[GREY,TEAL],unit='Summed RSS (MiB)')
axes[1].set_title('Same instant · different ownership scopes',loc='left',fontsize=11,pad=15)
fig.text(.065,.145,'Left: every captured Cargo command is metadata --no-deps; all 12 requested builds later exit 0.',fontsize=9.5)
fig.text(.065,.10,'Right: the runner labels host-wide RSS as tree RSS. Unrelated experiments contribute most of this sample.',fontsize=9.5)
fig.text(.065,.035,'buildq 1391d00 · retained JSON and plotting source accompany this figure · RSS can count shared pages more than once',fontsize=8,color='#617085')
export(fig,'measurement-scope')

if (ROOT/'bevy-budget-exact-logs/results.json').exists():
    bevy={r['name']:r for r in read('bevy-budget-exact-logs/results.json')}
    required={'cold-64M','cold-rebuild-64M','warm-2G','cold-cache','after-evict','after-retaining-gc'}
    if required<=bevy.keys():
        fig,axes=plt.subplots(1,2,figsize=(11.2,5.8))
        fig.subplots_adjust(top=.72,bottom=.27,wspace=.35,left=.08,right=.97)
        fig.text(.065,.945,'Bevy ECS: an undersized budget makes the next build cold',fontsize=17,weight='bold')
        fig.text(.065,.883,'Bevy v0.16.1 · cargo build -p bevy_ecs · dev artifacts for the ECS crate and dependencies',fontsize=10.7)
        times=[bevy[n]['seconds'] for n in ('cold-64M','cold-rebuild-64M','warm-2G')]
        bars(axes[0],['Initial cold\nbuild','Cold rebuild\nafter eviction','Warm reuse\nwith 2 GiB budget'],times,[BLUE,ORANGE,TEAL],unit='Command wall time (seconds)')
        axes[0].set_title('Sequential lifecycle probe · one run per stage',loc='left',fontsize=11,pad=15)
        footprint=[bevy[n]['json']['total']/MIB for n in ('cold-cache','after-evict','after-retaining-gc')]
        bars(axes[1],['Built member','GC with\n64 MiB budget','GC with\n2 GiB budget'],footprint,[BLUE,ORANGE,TEAL])
        axes[1].set_title('Manual GC evicts or retains the same workload',loc='left',fontsize=11,pad=15)
        axes[1].axhline(64,color='#b65049',linestyle=(0,(5,3)),linewidth=1.4)
        axes[1].text(.5,90,'64 MiB budget',ha='center',transform=axes[1].get_yaxis_transform(),fontsize=9,color='#a34640')
        fig.text(.065,.145,'After the first build, GC reclaims 644,550,656 bytes. A 2 GiB budget later retains that complete member.',fontsize=9.5)
        fig.text(.065,.10,'The warm command reuses the preceding rebuild. Raising the budget preserves reuse; it does not create a cache.',fontsize=9.5)
        fig.text(.065,.035,'buildq 1391d00 · Rust 1.89.0 · 1 Cargo job · shared CPU / filesystem · lifecycle timings are not an isolated speed benchmark',fontsize=8,color='#617085')
        export(fig,'bevy-budget')
print(OUT)
