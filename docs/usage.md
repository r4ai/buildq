# Usage Guide

English | [日本語](usage.ja.md)

See the [README](../README.md) for installation and everyday use.
This guide covers configuration, monitoring, and remote execution.

## Configuration (`config.toml`)

All settings are optional.
Place `config.toml` in `$BUILDQ_DIR/config.toml` (check `buildq config` for the default path on your OS):

```toml
slots = 2                # Max concurrent build jobs (default: 2)
# max_per_worktree = 1   # Soft cap per worktree in each class (default: ceil(slots / 2); 0 disables)
run_slots = 2            # Max concurrent app runs for `cargo run` (default: 2)

[cache]
max_members = 3          # Max target/ dirs per repo+toolchain (minimum: slots; default: slots + 1)
budget = "45G"           # Max disk usage under BUILDQ_CACHE (default: "60G")
min_free = "25G"         # Minimum free disk space to preserve on volume (default: "20G")
# pool_explicit = true   # Also pool explicit --target-dir / CARGO_TARGET_DIR inside the worktree (default: false)

[cargo]
rustc_wrapper = "none"   # Disable global sccache per job ("none" | "keep" | "/path")
# jobs = 8               # CARGO_BUILD_JOBS per build (default: cores / slots)
```

`cache.max_members` must be at least the effective `slots` value, including
environment overrides. Smaller values are raised to `slots` with a warning that
shows the requested value, effective cap, and how to correct the configuration.
A per-project cap smaller than the global slot count is currently unsupported.

| Setting | Environment override | Default |
| :--- | :--- | :--- |
| `max_per_worktree` | `BUILDQ_MAX_PER_WORKTREE` | `ceil(slots / 2)`; `0` disables |

If a pooled build fails with a `min_free` diagnostic (exit `125`), free space on
the cache volume and retry. Admission first attempts GC once; active builds also
stop if the reserve is breached. Use `buildq cache ls --refresh` and
`buildq gc` to inspect and reclaim idle caches. Configure a reserve appropriate
for the volume; `min_free = "0"` disables this guard. See the
[free-space guard](architecture.md#free-space-guard) for scope and limitations.

## Cache Budget

`[cache] budget` controls cache retention, rather than the maximum size of a
running build. Busy artifacts are protected. Use `buildq cache ls --refresh`
or `buildq gc --json` to see remaining overflow and its reason. GC can exit
successfully while still over budget when busy artifacts prevent reclamation.

A completed member larger than the entire budget warns that GC may remove it,
causing another cold build next time. Increase `budget` to fit the working set
or reduce build artifacts. Oversized idle members have no retention exemption.
The separate `min_free` guard still refuses or stops pooled builds under disk
pressure; increasing `budget` does not disable it. See
[Disk Budget & Garbage Collection](architecture.md#4-disk-budget--garbage-collection) for details.

`cargo run` reports when native runtime dependencies keep a target member
leased while queued or running. GC protects that member and reports remaining
overflow. A long-running native application can therefore reduce build capacity;
allow enough `cache.max_members` for its retained member, or stop the application
to release it. Compilations still release their build slots before execution.

## Live Monitoring

`buildq top` refreshes running jobs, waiters and wait durations, remote queues,
and target pool usage every second. Use `--interval N` to change the interval
in seconds, or `--no-hosts` to monitor locally. Remote status runs in the
background so a slow host does not freeze local updates; probes start at most
once every five seconds and each request has a five-second deadline.
Press Ctrl+C to stop. Use `--once` for a full snapshot of all rows.
Redirected output also prints one plain-text snapshot without terminal codes.
Pool sizes use the same cached estimates as `cache ls`.

## Remote Builds (SSH)

Add remote hosts to `config.toml` to offload builds over SSH and Git:

```toml
auto_hosts = ["local", "box"]

[hosts.box]
# transport = "rsync"    # Optional: sync ignored generated files too
ssh = "box"              # Host alias from ~/.ssh/config
slots = 4                # Max concurrent jobs sent to this host
# buildq = "buildq"      # Probe remote queue load before automatic selection
# rustc_release = "1.99.0" # Optional required remote rustc release
```

Run commands on a specific host or let `buildq` pick one automatically:

```sh
buildq run --host box -- cargo test
buildq run --host auto -- cargo clippy --all-targets
buildq run --host box --fetch target/debug/app -- cargo build --bin app
```

See [Remote Execution Protocol](architecture.md#5-remote-execution-protocol-ssh) for host selection and transport details.
