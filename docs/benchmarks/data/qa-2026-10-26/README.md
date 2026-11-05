# QA evidence index — 2026-10-26

[English report](../../qa-2026-10-26.md) | [日本語レポート](../../qa-2026-10-26.ja.md)

This directory preserves real runtime results for buildq1391d00 and mbx1.27.0. [environment.json](environment.json) pins binary hashes and resource limits; [issues.json](issues.json) maps all16 findings/requests to tagged GitHub Issues. All remote writes were confined to r4ai/buildq.

| Track | Entry point | Principal data |
| :--- | :--- | :--- |
| Baseline | `baseline/` |159 passing normal Rust tests;3 realCargo E2Es; paired target-dir test-environment control |
| OSS | [oss/README.md](oss/README.md) |Four pinned repos; full rust-analyzer;12/64 submissions;35m soak; output edits/runs |
| mbx | [mbx/README.md](mbx/README.md) |352 command measurements; controlled ripgrep pairs; Bevy PBR fixture; nativeC;140-job rotation |
| Sharing | [sharing/README.md](sharing/README.md) |2160 output assertions; metadata scaling; actualmbx hardlinks; injected interrupted seed |
| Storage | [storage/README.md](storage/README.md) |45m resumed soak;1719 assertions;738GCs; oversize/free-space/crash probes; actualBevy budget lifecycle |
| Accounting | [accounting/README.md](accounting/README.md) |Independent repros for overflow, storeidentity, evictionrebate and nested correctness |
| Dual GC | [dual-gc/README.md](dual-gc/README.md) |30m,8235 assertions, both automaticGCs,30 extra pressure cycles |

## Inspect raw evidence

JSON summaries and source fixtures are loose for review. Repeated/raw compiler logs are archived to keep the diff manageable:

- `mbx/raw-logs.tar.gz`: extract within `mbx/`; original relative stdout/stderr names preserved
- `oss/raw-logs.tar.gz`: extract within `oss/logs/`; original jobs/bursts/control paths preserved
- `accounting/nested-raw.tar.gz`: extract within `accounting/`; independent nested/ablation layout preserved
- `storage/soak-jobs.tar.gz` and other storage archives: follow the exact names/layout documented by the storage report
- Gzipped JSONL streams preserve every original event, including raw failed invariant classifications

`SHA256SUMS` covers committed evidence files. It excludes itself and the validation manifest to avoid recursive hashes. Figures have their generating scripts nearby. Images linked from Issues use immutable commit URLs.

## Interpret results

A successful exit is not the same as a correct executable: #142 intentionally demonstrates wrong output with exit0. A raw soak failure is also not automatically a product failure: probes/zombies/test presets/environment assumptions are explicitly classified. No raw failed run was changed to green.

All timings share host contention. Restore comparisons state their store/target cache state. RSS scope, sampling resolution and cumulative reclaimed bytes are documented. Reflink savings and multi-day endurance are unverified in this overlay environment; injected interrupted-seed results are labeled as such.
