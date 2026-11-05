# Independent accounting and correctness probes

Release buildq `1391d00`, mbx1.27.0, Rust1.89.0/Linux. Every generated target/store is outside the source checkout. Source scripts require a fresh `--root`.

| Artifact | Experiment / Issue |
| :--- | :--- |
| `probe.py`, `accounting/results.json` | Real mbx footprint exceeds1KiB budget after pool eviction; #138 |
| `probe.py`, `accounting-identity/results.json` | Two mbx stores share an identity-free usage snapshot; #139 |
| `eviction-rebate.py`, `eviction-rebate/` | Store bytes stay fixed while eviction removes the cached hardlink overlap; #151 |
| `nested-independent/` | Primary agent independently reproduces stale nested codegen with natural checkout timestamps; #142 |
| `nested-controls.py`, `nested-controls/` | Isolated ablations: incremental/nested-mbx/private-state flags and local helper targets |
| `plot.py`, `assets/` | Source-backed PNG/SVG figures |

## Reproduce

Activate Rust1.89.0 and ensure the binary paths refer to the tested buildq and real mbx. The first probe has no external Cargo dependencies:

```sh
python3 probe.py --root /tmp/buildq-accounting-new \
  --buildq /absolute/path/buildq --mbx /absolute/path/mbx
```

The hardlink-overlap probe uses regex. Fetch the retained `eviction-rebate/Cargo.lock` dependencies before timing, then:

```sh
python3 eviction-rebate.py --root /tmp/buildq-rebate-new \
  --buildq /absolute/path/buildq --mbx /absolute/path/mbx \
  --lockfile eviction-rebate/Cargo.lock
```

It first creates the linked member by an actual mbx restore, and creates the private member with actual ordinary Cargo. Only member recency metadata is adjusted to select the linked member for eviction. No mbx response, object or TTL is fabricated. The configured budget is derived from measured store/private-member bytes to isolate exactly1MiB of missed overflow. mbx alone fits that budget.

For nested correctness:

```sh
python3 ../mbx/nested-switch.py --root /tmp/buildq-nested-new \
  --buildq /absolute/path/buildq --mbx /absolute/path/mbx
python3 nested-controls.py --root /tmp/buildq-local-helper-new \
  --buildq /absolute/path/buildq --mbx /absolute/path/mbx \
  --case local-helper-target
```

The first command intentionally exits1 when stale output is detected. The worktree-local-helper control exited0. Other controls retain the wrong-output finding and intentionally exit1. Their per-command results retain expected/actual stdout and exit codes.

## Evidence layout

`results.json` files are readable command-by-command records. Raw stdout/stderr for independent nested runs and ablations are bundled in `nested-raw.tar.gz`; extracted paths preserve their original layout below this directory. All JSON summaries remain loose for review. `probe.py` records direct full stdout/stderr separately under each experiment's `logs/` directory.

Regenerate figures from this directory with `python3 plot.py`. The plot reads retained JSON; it does not rerun benchmarks.
