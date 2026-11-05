#!/usr/bin/env python3
"""Standalone reproduction of stale build-script helper output across worktrees.

Example: python3 nested-switch.py --root /tmp/new-nested-qa \
    --buildq /path/to/buildq --mbx /path/to/mbx
Requires Git, Rust and Python 3.10+. Creates only files below a NEW --root.
Returns 1 when an output is stale; all command logs and fixtures are preserved.
No source timestamp is rewritten. No remote repository is changed.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import time

FILES = {
    'Cargo.toml': '[package]\nname="nested-app"\nversion="0.1.0"\nedition="2021"\n[workspace]\nexclude=["tools/helper"]\n',
    'src/main.rs': 'fn main(){println!("generated={}",include!(concat!(env!("OUT_DIR"),"/generated.rs")));}\n',
    'build.rs': '''use std::{process::Command,path::PathBuf};
fn main(){let out=PathBuf::from(std::env::var("OUT_DIR").unwrap());
let r=Command::new("cargo").args(["run","--manifest-path","tools/helper/Cargo.toml","--offline","--quiet"]).env("CARGO_TARGET_DIR",out.join("helper-target")).output().unwrap();
assert!(r.status.success(),"{}",String::from_utf8_lossy(&r.stderr));
std::fs::write(out.join("helper.stderr"),&r.stderr).unwrap();
std::fs::write(out.join("generated.rs"),format!("{:?}",String::from_utf8(r.stdout).unwrap().trim())).unwrap();
println!("cargo:rerun-if-changed=tools/helper/src/main.rs");}
''',
    'tools/helper/Cargo.toml': '[package]\nname="nested-codegen"\nversion="0.1.0"\nedition="2021"\n[workspace]\n',
    'tools/helper/src/main.rs': 'fn main(){println!("A");}\n',
    '.gitignore': '/target\n**/target\n',
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--buildq', type=Path, required=True)
    parser.add_argument('--mbx', type=Path, required=True)
    parser.add_argument('--cargo', type=Path, default=Path(shutil.which('cargo') or 'cargo'))
    args = parser.parse_args()
    root = args.root.resolve()
    if root.exists():
        parser.error('--root must not exist; existing data is never deleted')
    root.mkdir(parents=True)
    records = []
    original_path = os.environ['PATH']
    for backend in ['cargo', 'mbx']:
        base = root / backend
        a = base / 'a'
        a.mkdir(parents=True)
        for relative, content in FILES.items():
            file = a / relative
            file.parent.mkdir(parents=True, exist_ok=True)
            file.write_text(content)
        def git(*command):
            subprocess.run(['git', '-c', 'user.name=buildq QA', '-c', 'user.email=qa@example.invalid', '-C', str(a), *command], check=True, stdout=subprocess.DEVNULL)
        git('init', '-q')
        git('add', '.')
        git('commit', '-qm', 'fixture A')
        b = base / 'b'
        git('worktree', 'add', '--detach', str(b))
        (b / 'tools/helper/src/main.rs').write_text('fn main(){println!("B");}\n')
        # Both differing helper sources predate ALL compilation. Ordinary editor
        # writes suffice: no old-mtime manipulation or concurrent writer.
        time.sleep(1)
        state = base / 'state'
        state.mkdir()
        (state / 'config.toml').write_text(f'''slots=1
[cache]
budget="1G"
min_free="0"
gc_mode="inline"
[cargo]
backend={json.dumps(backend)}
mbx={json.dumps(str(args.mbx.resolve()))}
nested_mbx=true
mirror_bins=false
''')
        env = os.environ.copy()
        for variable in ['BUILDQ_HELD', 'BUILDQ_CONFIG', 'BUILDQ_GC', 'CARGO_TARGET_DIR', 'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER']:
            env.pop(variable, None)
        env.update(BUILDQ_DIR=str(state), BUILDQ_CACHE=str(base / 'cache'), MBX_CACHE_DIR=str(base / 'mbx-cache'), MBX_GC_AUTO='0', BUILDQ_REAL_CARGO=str(args.cargo.absolute()), CARGO_BUILD_JOBS='1')
        shim = base / 'shim'
        subprocess.run([str(args.buildq.resolve()), 'shim', 'install', 'cargo', '--into', str(shim)], env=env, check=True, stdout=subprocess.DEVNULL)
        env['PATH'] = str(shim) + os.pathsep + original_path
        def command(name, cwd, argv, expected=None):
            start = time.monotonic()
            result = subprocess.run([str(args.buildq.resolve()), *argv], cwd=cwd, env=env, capture_output=True, text=True, timeout=120)
            (base / f'{name}.stdout').write_text(result.stdout)
            (base / f'{name}.stderr').write_text(result.stderr)
            record = dict(backend=backend, name=name, cwd=str(cwd), command=argv, wall_s=round(time.monotonic()-start, 3), exit=result.returncode, output=result.stdout.strip(), expected=expected)
            record['passed'] = result.returncode == 0 and (expected is None or record['output'] == expected)
            records.append(record)
            print(json.dumps(record), flush=True)
        command('A', a, ['cargo', 'run', '--offline'], 'generated=A')
        command('B', b, ['cargo', 'run', '--offline'], 'generated=B')
        command('A-again', a, ['cargo', 'run', '--offline'], 'generated=A')
        command('clean', a, ['cache', 'clean', '--all'])
        command('B-clean', b, ['cargo', 'run', '--offline'], 'generated=B')
        (base / 'timestamps.json').write_text(json.dumps({str(p.relative_to(base)):p.stat().st_mtime_ns for p in [a/'tools/helper/src/main.rs',b/'tools/helper/src/main.rs']}, indent=2)+'\n')
    (root / 'results.json').write_text(json.dumps(records, indent=2)+'\n')
    return int(any(not record['passed'] for record in records))


if __name__ == '__main__':
    raise SystemExit(main())
