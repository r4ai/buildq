#!/usr/bin/env bash
set -euo pipefail
# Run from repository root. Supply an existing root only to resume the same seed.
: "${QA_ROOT:?set QA_ROOT to a fresh writable directory}"
: "${QA_BUILDQ:?set QA_BUILDQ to the buildq executable}"
export CARGO_BUILD_JOBS=1
export PYTHONPATH="$PWD/scripts${PYTHONPATH:+:$PYTHONPATH}"
# Select the installed toolchain externally (the measured run used Rust 1.89.0).
mkdir -p "$QA_ROOT/logs"
python3 scripts/soak.py --root "$QA_ROOT" --buildq "$QA_BUILDQ" --duration 35m \
  --worktrees 3 --slots 2 --budget 5G --min-free 6G --seed 2718 \
  --sample-interval 10s --job-timeout 15m --gc-every 8 \
  > "$QA_ROOT/logs/runner.out" 2> "$QA_ROOT/logs/runner.err" &
runner_pid=$!
# Wait for all clones and worktrees to exist before launching cross-repository burst.
for attempt in $(seq 1 120); do
  if test -d "$QA_ROOT/worktrees/rust-analyzer-2" && test -f "$QA_ROOT/logs/soak.jsonl"; then break; fi
  sleep 1
done
python3 docs/benchmarks/data/qa-2026-10-26/oss/burst.py --root "$QA_ROOT" --buildq "$QA_BUILDQ" \
  > "$QA_ROOT/logs/burst-runner.out" 2> "$QA_ROOT/logs/burst-runner.err" &
burst_pid=$!
python3 docs/benchmarks/data/qa-2026-10-26/oss/real-edit.py --root "$QA_ROOT" --buildq "$QA_BUILDQ" \
  > "$QA_ROOT/logs/real-edit-runner.out" 2> "$QA_ROOT/logs/real-edit-runner.err" &
edit_pid=$!
python3 docs/benchmarks/data/qa-2026-10-26/oss/full-application.py --root "$QA_ROOT" --buildq "$QA_BUILDQ" \
  > "$QA_ROOT/logs/full-runner.out" 2> "$QA_ROOT/logs/full-runner.err" &
full_pid=$!
python3 docs/benchmarks/data/qa-2026-10-26/oss/ripgrep-run.py --root "$QA_ROOT" --buildq "$QA_BUILDQ" \
  > "$QA_ROOT/logs/ripgrep-runner.out" 2> "$QA_ROOT/logs/ripgrep-runner.err" &
run_pid=$!
wait "$burst_pid"
wait "$edit_pid"
wait "$full_pid"
wait "$run_pid"
python3 docs/benchmarks/data/qa-2026-10-26/oss/burst.py --root "$QA_ROOT" --buildq "$QA_BUILDQ" \
  --jobs 64 --label burst64 --timeout 900 \
  > "$QA_ROOT/logs/burst64-runner.out" 2> "$QA_ROOT/logs/burst64-runner.err"
# The current OSS preset is expected to fail Tokio tests without full; retain that result.
wait "$runner_pid"
