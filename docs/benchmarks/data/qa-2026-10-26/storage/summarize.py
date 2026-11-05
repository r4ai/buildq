#!/usr/bin/env python3
"""Derive explicitly scoped counts from retained, unmodified soak JSONL."""
import argparse
from collections import Counter
import gzip
import json
from pathlib import Path
import statistics

p=argparse.ArgumentParser()
p.add_argument('--log',type=Path,required=True)
p.add_argument('--output',type=Path,required=True)
a=p.parse_args()
raw=a.log.read_bytes()
if a.log.suffix=='.gz':raw=gzip.decompress(raw)
records=[json.loads(line) for line in raw.splitlines()]
start=next(r for r in records if r['type']=='start')
actions=[r for r in records if r['type']=='action']
samples=[r for r in records if r['type']=='sample']
history=[r for r in records if r['type']=='history_stats']
def group_rss(sample):
    groups=set(sample['live_groups'])|set(sample['dead_groups'])
    return sum(p['rss_kb'] for p in sample['processes'] if p['pgid'] in groups)
checks=[job['output_ok'] for action in actions for job in [action,*action.get('jobs',[])] if 'output_ok' in job]
cargos=[]
for action in actions:
    for job in action.get('jobs',[action]):
        cmd=job.get('cmd',[])
        if len(cmd)>1 and cmd[1]=='cargo':cargos.append(job)
gc_actions=[r for r in actions if r['kind'] in ('gc','gc_pressure')]
gc_times=sorted(r['duration_s'] for r in gc_actions)
result={
    'revision':'1391d00',
    'started_utc_epoch':start['t'],
    'last_record_utc_epoch':records[-1]['t'],
    'observed_elapsed_s':round(records[-1]['t']-start['t'],3),
    'seed':start['seed'],
    'repo_count':len(start['repos']),
    'worktrees_per_repo':start['worktrees'],
    'slots':start['slots'],
    'budget':start['budget'],
    'resumes':[r for r in records if r['type']=='resume'],
    'actions':len(actions),
    'actions_by_kind':dict(Counter(r['kind'] for r in actions)),
    'actions_by_repo':dict(Counter(r['repo'] for r in actions)),
    'cargo_commands_including_burst_children':len(cargos),
    'cancellations':sum(r['kind']=='cancel' for r in actions),
    'output_marker_checks':len(checks),
    'output_marker_failures':checks.count(False),
    'unexpected_action_exit_failures':[r['index'] for r in actions if r.get('expect_success') and r.get('exit')!=0],
    'leftover_process_actions':[r['index'] for r in actions if r.get('leftover_pids')],
    'gc_invocations':len(gc_actions),
    'gc_failed_actions':[r['index'] for r in gc_actions if r.get('gc_problems')],
    'gc_cumulative_freed_bytes':sum(r.get('gc',{}).get('freed',0) or 0 for r in gc_actions),
    'gc_duration_s':{'median':statistics.median(gc_times),'p95_nearest_rank':gc_times[max(0,(95*len(gc_times)+99)//100-1)],'max':max(gc_times)} if gc_times else None,
    'gc_timing_note':'Runner end-to-end command duration with a 100ms poll loop; not isolated collector CPU time or sub-100ms latency.',
    'samples':len(samples),
    'sample_violations':dict(Counter(v['invariant'] for r in samples for v in r.get('violations',[]))),
    'slot_holder_limit_exceeding_samples':[r['t'] for r in samples if r.get('status') and any(sum(h['busy'] for h in c['holders'])>c['slots'] for c in r['status']['classes'])],
    'cargo_process_count_alarms':[{'t':r['t'],'detail':v['detail']} for r in samples for v in r.get('violations',[]) if v['invariant']=='slot_limit' and 'cargo processes' in v['detail']],
    'peak_allocated_cache_du_bytes':max(r['du_bytes'] or 0 for r in samples),
    'peak_trash_bytes':max(r['cache']['trash_bytes'] for r in samples if r.get('cache')),
    'minimum_global_free_disk_bytes':min(r['free_disk'] for r in samples),
    'peak_owned_group_rss_kib':max(map(group_rss,samples)),
    'rss_scope_note':'Only owned live/dead PGIDs, not an exact full descendant tree. New-group watchdogs may be omitted. Raw runner max_tree_rss includes unrelated host compiler processes.',
    'queue_wait':history[-1]['stats'] if history else None,
}
a.output.write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps(result,indent=2))
