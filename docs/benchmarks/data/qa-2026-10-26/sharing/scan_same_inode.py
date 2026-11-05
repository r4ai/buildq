#!/usr/bin/env python3
"""Read-only independent scan of idle member hard-link sharing (Unix)."""
import argparse
import json
from pathlib import Path

ap = argparse.ArgumentParser()
ap.add_argument("--cache", type=Path, required=True)
ap.add_argument("--output", type=Path, required=True)
args = ap.parse_args()
root = args.cache / "pools"
inodes = {}
for path in root.glob("*/m[0-9]*/**/*"):
    if not path.is_file() or path.is_symlink():
        continue
    parts = path.relative_to(root).parts
    if len(parts) < 3 or not parts[1][1:].isdigit():
        continue
    if any(p in ["incremental", ".fingerprint"] or p.startswith(".buildq-dedupe-")
           for p in parts[2:]):
        continue
    md = path.stat()
    if md.st_size < 65536:
        continue
    inodes.setdefault((md.st_dev, md.st_ino), []).append(
        (parts[0], parts[1], path, md))
groups = []
for key, entries in inodes.items():
    members = {(pool, member) for pool, member, _, _ in entries}
    if len(members) <= 1:
        continue
    md = entries[0][3]
    footprint = min(md.st_blocks * 512,
                    ((md.st_size + md.st_blksize - 1)//md.st_blksize)*md.st_blksize)
    groups.append(dict(dev=key[0], ino=key[1], size=md.st_size,
                       members=len(members), allocated_footprint=footprint,
                       paths=[str(path) for _, _, path, _ in entries]))
result = dict(groups=len(groups),
              already_same_inode_duplicate_bytes=sum(
                  x["size"]*(x["members"]-1) for x in groups),
              already_same_inode_duplicate_allocated_bytes=sum(
                  x["allocated_footprint"]*(x["members"]-1) for x in groups),
              examples=groups)
args.output.write_text(json.dumps(result, indent=2)+"\n")
print(json.dumps({k: v for k, v in result.items() if k != "examples"}))
