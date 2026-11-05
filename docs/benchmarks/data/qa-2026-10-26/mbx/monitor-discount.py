import pathlib,time,json,subprocess,os
R=pathlib.Path('/workspace/qa-mbx/bevy');OUT=pathlib.Path('/workspace/buildq/docs/benchmarks/data/qa-2026-10-26/mbx');old=None;deadline=time.monotonic()+600
with (OUT/'rotation-discount-transitions.jsonl').open('w') as out:
 while time.monotonic()<deadline and not (OUT/'rotation-correctness.json').exists():
  try:
   usage=json.loads((R/'cache/mbx-usage.json').read_text());config=(R/'state/config.toml').read_text()
   members=sorted(str(p.relative_to(R/'cache')) for p in (R/'cache/pools').glob('*/m[0-9]*') if p.is_dir())
   key=(config,tuple(members),json.dumps(usage,sort_keys=True))
   if key!=old:
    env=os.environ.copy();env['MBX_CACHE_DIR']=str(R/'mbx-cache')
    stats=subprocess.run(['/workspace/qa-mbx/mbx','cache','stats','--json'],env=env,capture_output=True,text=True,timeout=30)
    row={'at':time.time(),'config':config,'members':members,'cached_usage':usage,'direct_mbx_stats':json.loads(stats.stdout) if stats.returncode==0 else None}
    out.write(json.dumps(row)+'\n');out.flush();old=key
  except (ValueError,FileNotFoundError):pass
  time.sleep(.1)
