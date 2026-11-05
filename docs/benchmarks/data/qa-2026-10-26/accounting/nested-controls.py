#!/usr/bin/env python3
"""Run the standalone nested fixture with explicit ablations.

Usage: nested-controls.py --root NEW --buildq PATH --mbx PATH
Logs each case, including expected nonzero findings, without editing buildq.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root',type=Path,required=True)
    parser.add_argument('--buildq',type=Path,required=True)
    parser.add_argument('--mbx',type=Path,required=True)
    parser.add_argument('--case',choices=['nested-off','incremental-off','local-helper-target',
        'mbx-build-script-off','mbx-shared-out-off','mbx-learned-off','mbx-seed-off'],required=True)
    args=parser.parse_args()
    spec=importlib.util.spec_from_file_location('nested_fixture',Path(__file__).parents[1]/'mbx/nested-switch.py')
    fixture=importlib.util.module_from_spec(spec);spec.loader.exec_module(fixture)
    if args.case=='nested-off':os.environ['BUILDQ_NESTED_MBX']='0'
    if args.case=='incremental-off':os.environ['CARGO_INCREMENTAL']='0'
    mbx_flags={'mbx-build-script-off':'MBX_BUILD_SCRIPT_EXECUTION','mbx-shared-out-off':'MBX_SHARE_OUT_DIR',
        'mbx-learned-off':'MBX_LEARNED_INCREMENTAL','mbx-seed-off':'MBX_SEED_INCREMENTAL'}
    if args.case in mbx_flags:os.environ[mbx_flags[args.case]]='0'
    if args.case=='local-helper-target':
        fixture.FILES['build.rs']=fixture.FILES['build.rs'].replace('out.join("helper-target")','PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("target/helper")')
    sys.argv=[str(spec.origin),'--root',str(args.root),'--buildq',str(args.buildq),'--mbx',str(args.mbx)]
    code=fixture.main()
    (args.root/'ablation.json').write_text(json.dumps({'case':args.case,'exit':code},indent=2)+'\n')
    return code


if __name__=='__main__':raise SystemExit(main())
