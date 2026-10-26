#!/usr/bin/env python3
import run, pathlib, os, subprocess, json
R=run.ROOT;O=run.OUT;W=R/'nested-ws';W.mkdir(exist_ok=True)
files={
'Cargo.toml':'[package]\nname="nested-app"\nversion="0.1.0"\nedition="2021"\n[workspace]\nexclude=["tools/helper"]\n',
'src/main.rs':'fn main() { println!("generated={}", include!(concat!(env!("OUT_DIR"), "/generated.rs"))); }\n',
'build.rs':r'''use std::{process::Command,path::PathBuf};
fn main(){
let out=PathBuf::from(std::env::var("OUT_DIR").unwrap());
let target=out.join("helper-target");
let result=Command::new("cargo").args(["run","--manifest-path","tools/helper/Cargo.toml","--offline","--quiet"]).env("CARGO_TARGET_DIR",target).output().unwrap();
assert!(result.status.success(),"nested failed: {}",String::from_utf8_lossy(&result.stderr));
println!("cargo:warning=nested stderr {}",String::from_utf8_lossy(&result.stderr));
std::fs::write(out.join("generated.rs"),format!("{:?}",String::from_utf8(result.stdout).unwrap().trim())).unwrap();
println!("cargo:rerun-if-changed=tools/helper/src/main.rs");
}
''',
 'tools/helper/Cargo.toml':'[package]\nname="nested-codegen"\nversion="0.1.0"\nedition="2021"\n[workspace]\n',
 'tools/helper/src/main.rs':'fn main(){println!("A");}\n',
 '.gitignore':'/target\n**/target\n',
}
for path,content in files.items():
 p=W/path;p.parent.mkdir(parents=True,exist_ok=True);p.write_text(content)
for args in [['init','-q'],['add','.'],['commit','-qm','nested-real-mbx-fixture'],['worktree','add','--detach',str(R/'nested-wt')]]:subprocess.run(['git','-C',str(W),*args],check=True,stdout=subprocess.DEVNULL)
run.ENV.update(BUILDQ_DIR=str(R/'nested-state'),BUILDQ_CACHE=str(R/'nested-cache'))
(R/'nested-state').mkdir(exist_ok=True)
(R/'nested-state/config.toml').write_text('''slots=1\n[cache]\nbudget="1G"\nmin_free="0"\ngc_mode="inline"\n[cargo]\nbackend="mbx"\nmbx="/workspace/qa-mbx/mbx"\nnested_mbx=true\nmirror_bins=false\n''')
subprocess.run([str(run.BQ),'shim','install','cargo','--into',str(R/'nested-shim')],env=run.ENV,check=True,stdout=subprocess.DEVNULL)
run.ENV['PATH']=str(R/'nested-shim')+os.pathsep+run.ENV['PATH']
run.run('nested-first',W,['cargo','run','--offline'],timeout=120)
wt=R/'nested-wt'
run.run('nested-other-worktree',wt,['cargo','run','--offline'],timeout=120)
p=wt/'tools/helper/src/main.rs';old=p.stat().st_mtime_ns;p.write_text('fn main(){println!("B");}\n');os.utime(p,ns=(old,old))
run.run('nested-changed-old-mtime',wt,['cargo','run','--offline'],timeout=120)
run.run('nested-back-to-original',W,['cargo','run','--offline'],timeout=120)
run.run('nested-final-status',W,['status','--json'])
