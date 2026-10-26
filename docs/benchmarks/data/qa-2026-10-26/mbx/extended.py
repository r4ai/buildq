#!/usr/bin/env python3
import run, pathlib, json, os, subprocess, time
R=run.ROOT; O=run.OUT; RG=run.RG;TK=run.TK; RA=R/'inputs/rust-analyzer-2025-06-02'
# Isolated cache roots make backend pairs independent; all dependency downloads warm.
run.ENV['BUILDQ_CACHE']=str(R/'controlled-cache')
run.ENV['BUILDQ_DIR']=str(R/'controlled-state')
(R/'controlled-state').mkdir(exist_ok=True)
def config(backend):
 (R/'controlled-state/config.toml').write_text(f'''slots=2\n[cache]\nbudget="6G"\nmin_free="0"\nmax_members=2\ngc_mode="inline"\nseed_members=true\n[cargo]\nbackend="{backend}"\nmbx="{os.environ.get('QA_MBX',str(R/'mbx'))}"\nnested_mbx=true\nmirror_bins=false\n''')
config('mbx')
# Fixed lockfiles recorded next to evidence.
(O/'tokio-generated-Cargo.lock').write_bytes((TK/'Cargo.lock').read_bytes())
run.run('tokio-full-fixed',TK,['cargo','check','--locked','-p','tokio@1.45.1','--all-features'])
run.run('tokio-minimal-fixed',pathlib.Path(str(TK)+'-wt1'),['cargo','check','--locked','-p','tokio@1.45.1','--no-default-features'])
run.run('tokio-full-reuse',pathlib.Path(str(TK)+'-wt2'),['cargo','check','--locked','-p','tokio@1.45.1','--all-features'])
run.run('tokio-lib-tests',TK,['cargo','test','--locked','-p','tokio@1.45.1','--all-features','--lib'])
# Plain-backend control of known sandbox Git-ancestor-sensitive ripgrep test.
config('cargo')
run.run('ripgrep-f1414-cargo-control',RG,['cargo','test','--locked','--test','integration','f1414_no_require_git','--','--exact'])
config('mbx')
run.run('ripgrep-f1414-mbx-control',RG,['cargo','test','--locked','--test','integration','f1414_no_require_git','--','--exact'])
# Full target discard preserves mbx store; backend pair is repeatedly cold Cargo.
for i in range(3):
 for backend in ['cargo','mbx'] if i%2==0 else ['mbx','cargo']:
  config(backend)
  run.run(f'paired-{i}-{backend}-clean',RG,['cache','clean','--all'])
  run.run(f'paired-{i}-{backend}-rebuild',pathlib.Path(str(RG)+'-wt1'),['cargo','build','--locked','--offline'])
  run.run(f'paired-{i}-{backend}-warm',pathlib.Path(str(RG)+'-wt1'),['cargo','build','--locked','--offline'])
config('mbx')
run.run('rust-analyzer-cold-check',RA,['cargo','check','--locked','-p','rust-analyzer'],timeout=1200)
run.run('rust-analyzer-worktree-check',pathlib.Path(str(RA)+'-wt1'),['cargo','check','--locked','-p','rust-analyzer'],timeout=1200)
run.run('extended-final-cache',RG,['cache','ls','--refresh','--json'])
run.run('extended-final-gc',RG,['gc','--json'])
run.run('extended-final-status',RG,['status','--json'])
