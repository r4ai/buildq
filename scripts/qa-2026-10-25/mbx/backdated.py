#!/usr/bin/env python3
"""Reproduce a Git stat-cache false-clean input and check buildq's output.

All Git/index mutations are confined to new disposable fixture repositories.
After warming an ALPHA binary, refresh the index with ALPHA in a new second,
then write equal-sized DELTA with the same backdated mtime in that same second.
Save Git/index/blob/file stat/pool-manifest evidence before and after the edit.
"""

import argparse
import json
import os
import time
from pathlib import Path

from qa_support import fresh_paths, run

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--root", type=Path, required=True)
parser.add_argument("--out", type=Path, required=True)
parser.add_argument("--binary", default=os.environ.get("BUILDQ_BIN"))
parser.add_argument("--mbx", default=os.environ.get("MBX_BIN"))
parser.add_argument("--repetitions", type=int, default=3)
args = parser.parse_args()
if not args.binary or not args.mbx:
    parser.error("buildq and mbx paths are required")
root, out = fresh_paths(args.root, args.out)
base = {
    key: value
    for key, value in os.environ.items()
    if not key.startswith(("BUILDQ_", "MBX_"))
    and key not in {"CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR", "RUSTC_WRAPPER"}
}
base.update(
    CARGO_INCREMENTAL="0",
    CARGO_PROFILE_DEV_DEBUG="0",
    CARGO_BUILD_JOBS="1",
    MBX_DISPLAY="plain",
    MBX_SUMMARY="full",
    MBX_GC_AUTO="0",
)
rows = []
for backend in ["cargo", "mbx"]:
    for repeat in range(args.repetitions):
        name = f"{backend}-{repeat}"
        local = root / name
        repo = local / "repo"
        (repo / "src").mkdir(parents=True)
        (repo / "Cargo.toml").write_text(
            '[package]\nname="qa-backdated-input"\nversion="0.1.0"\nedition="2024"\n'
        )
        source = repo / "src/main.rs"
        source.write_text('fn main(){println!("{}",include_str!("../marker.txt"));}\n')
        marker = repo / "marker.txt"
        fixed = 946684800000000000
        marker.write_text("ALPHA")
        os.utime(marker, ns=(fixed, fixed))

        def git(*argv, repo=repo):
            result = run(
                ["git", "-C", str(repo), *argv],
                capture_output=True,
                text=True,
                timeout=10,
            )
            return dict(
                exit=result.returncode, stdout=result.stdout, stderr=result.stderr
            )

        git("init", "-q")
        run(
            ["cargo", "generate-lockfile", "--offline"],
            cwd=repo,
            env=base,
            capture_output=True,
            text=True,
            check=True,
            timeout=15,
        )
        git("add", ".")
        git(
            "-c",
            "user.name=QA",
            "-c",
            "user.email=qa@example.invalid",
            "commit",
            "-qm",
            "backdated fixture",
        )
        q = local / "q"
        q.mkdir()
        (q / "config.toml").write_text(f"""slots=1
run_slots=1
poll_ms=10
[cache]
dir={json.dumps(str(local / "pool"))}
max_members=1
budget="512MiB"
min_free="0"
gc_after_job=false
[cargo]
backend={json.dumps(backend)}
mbx={json.dumps(str(Path(args.mbx).resolve()))}
""")
        env = base | dict(
            BUILDQ_DIR=str(q),
            BUILDQ_CACHE=str(local / "pool"),
            MBX_CACHE_DIR=str(local / "mbx-cache"),
        )
        warm = run(
            [args.binary, "cargo", "run", "--offline", "--locked"],
            cwd=repo,
            env=env,
            capture_output=True,
            text=True,
            check=True,
            timeout=30,
        )
        (out / (name + "-warm.log")).write_text(warm.stdout + warm.stderr)
        if warm.stdout.strip() != "ALPHA":
            raise RuntimeError("warm output incorrect")
        old_second = marker.stat().st_ctime_ns // 1000000000
        # Place the fresh ALPHA stat entry and DELTA edit well within one new
        # second. The subsequent job may run later; the cached stat tuple stays
        # equal until another filesystem metadata change is made.
        deadline = time.monotonic() + 3
        while not (int(time.time()) > old_second and time.time() % 1 < 0.15):
            if time.monotonic() > deadline:
                raise RuntimeError("could not align stat-cache second")
            time.sleep(0.01)
        marker.write_text("ALPHA")
        os.utime(marker, ns=(fixed, fixed))
        git("update-index", "--refresh")

        def snapshot(marker=marker, local=local, git=git):
            stat = marker.stat()
            return dict(
                marker_content=marker.read_text(),
                marker_stat=dict(
                    ctime_ns=stat.st_ctime_ns,
                    mtime_ns=stat.st_mtime_ns,
                    inode=stat.st_ino,
                    device=stat.st_dev,
                    size=stat.st_size,
                ),
                git_status=git("status", "--porcelain", "--untracked-files=all"),
                git_index=git("ls-files", "-s"),
                git_index_debug=git("ls-files", "--debug", "marker.txt"),
                git_index_flags=git("ls-files", "-v", "marker.txt"),
                git_marker_hash=git("hash-object", "marker.txt"),
                git_source_hash=git("hash-object", "src/main.rs"),
                git_core_trustctime=git(
                    "config", "--show-origin", "--get", "core.trustctime"
                ),
                git_core_checkstat=git(
                    "config", "--show-origin", "--get", "core.checkstat"
                ),
                pool_manifests={
                    str(path.relative_to(local)): path.read_text()
                    for path in (local / "pool").rglob("m*.manifest")
                },
            )

        before = snapshot()
        marker.write_text("DELTA")
        os.utime(marker, ns=(fixed, fixed))
        after = snapshot()
        result = run(
            [args.binary, "cargo", "run", "--offline", "--locked"],
            cwd=repo,
            env=env,
            capture_output=True,
            text=True,
            timeout=30,
        )
        after_job = snapshot()
        (out / (name + "-edited.log")).write_text(result.stdout + result.stderr)
        row = dict(
            name=name,
            backend=backend,
            repetition=repeat,
            expected="DELTA",
            actual=result.stdout.strip(),
            exit=result.returncode,
            passed=result.returncode == 0 and result.stdout.strip() == "DELTA",
            same_ctime_second=before["marker_stat"]["ctime_ns"] // 1000000000
            == after["marker_stat"]["ctime_ns"] // 1000000000,
            git_false_clean=after["git_status"]["stdout"] == "",
            before=before,
            after_edit=after,
            after_job=after_job,
        )
        # Advancing the ordinary mtime should restore both Git dirty detection
        # and package invalidation, separating the false-clean manifest from
        # a missing package owner or include_str! dependency.
        os.utime(marker, None)
        recovery = run(
            [args.binary, "cargo", "run", "--offline", "--locked"],
            cwd=repo,
            env=env,
            capture_output=True,
            text=True,
            timeout=30,
        )
        (out / (name + "-recovered.log")).write_text(recovery.stdout + recovery.stderr)
        row["recovery"] = dict(
            exit=recovery.returncode,
            output=recovery.stdout.strip(),
            pool_manifest_marker_entries={
                str(path.relative_to(local)): [
                    line
                    for line in path.read_text().splitlines()
                    if line.endswith(" marker.txt")
                ]
                for path in (local / "pool").rglob("m*.manifest")
            },
        )
        rows.append(row)
        (out / "results.json").write_text(json.dumps(rows, indent=2) + "\n")
        print(
            json.dumps(
                {
                    key: value
                    for key, value in row.items()
                    if key not in {"before", "after_edit", "after_job", "recovery"}
                }
            ),
            flush=True,
        )
provenance = dict(
    rustc=run(
        ["rustc", "-Vv"], env=base, capture_output=True, text=True, check=True
    ).stdout,
    git=run(
        ["git", "--version", "--build-options"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout,
    git_optional_locks=os.environ.get("GIT_OPTIONAL_LOCKS"),
    mbx=run([args.mbx, "--version"], capture_output=True, text=True, check=True).stdout,
)
(out / "provenance.json").write_text(json.dumps(provenance, indent=2) + "\n")
