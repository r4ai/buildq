# Real mbx/buildq dual-GC churn

Real mbx1.27.0 and buildq1391d00 ran for **1800.228 seconds**, editing two worktrees of a dependency-free controlled Rust fixture. Source markers, feature flags and data bytes are checked in every executable output; every third edit's mtime is backdated to2000. The payload is256KiB of runtime data, not a large OSS graph. Other tracks supply the large-OSS coverage.

- **8,235/8,235 outputs correct**, zero nonzero command exits
- Both automatic collectors enabled; buildq inline mode and mbx1-second GC interval
- Buildq32MiB budget; mbx16MiB action-store /24MiB total /4MiB incremental limits
- 60 foreground buildqGC calls and20 foreground mbxGC calls, plus final calls
- An additional **30 pressure cycles** forced mbx's action-store cap to128KiB and buildq's budget to512KiB without changing running jobs' configuration
- All60 pressure calls exited0; reported removal2,048,944mbx action-store bytes and42,655,744buildq bytes cumulatively, not peak footprint or unique lifetime bytes
- Final build/run status idle, zero waiters

[summary.json](summary.json) retains the original runner summary. [derived-summary.json](derived-summary.json) retains final status/inventory and independently calculated GC/exit counts. Budget flags during pressure can be true while busy artifacts are protected; this is a soft-budget test, not a hard storage-quota claim.

The fixture ran under the storage track's Linux subreaper. This result does not cover arbitrary nested-codegen layouts; #142's intentionally failing nested fixture remains separate.

## Reproduce

Activate the tested Rust toolchain and use a new root:

```sh
python3 ../storage/subreaper.py python3 run.py \
  --root /tmp/new-buildq-dual-gc --buildq /absolute/path/buildq \
  --mbx /absolute/path/mbx --duration 1800
```

While it is running, apply pressure from another terminal:

```sh
python3 pressure.py --root /tmp/new-buildq-dual-gc \
  --buildq /absolute/path/buildq --mbx /absolute/path/mbx --duration 900
```

Our pressure run started about10minutes after the main loop and ran for15minutes. Exact per-command timestamps, argv, stdout and stderr are retained in `events.jsonl.gz` and `pressure.jsonl`. `config.toml` records the effective fixture settings and original absolute paths; the runner generates new paths for a new root.

Read compressed events with `gzip -dc events.jsonl.gz`. The log has8,693records, including output assertions, status, store stats, fresh inventory, cached usage snapshots and GC reports. The summary, not this prose, is authoritative for action counts.
