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
# pool_fallback = true   # If the pool cannot be opened (Git/toolchain failure), build outside it with a warning instead of failing (default: false; env BUILDQ_POOL_FALLBACK)
# dedupe = "off"         # Share identical files between members during GC ("auto" | "off", default: "auto")
# seed_members = true    # Start new members as copy-on-write clones of the closest built one, and keep a warm member per worktree (APFS, Btrfs, XFS; experimental; default: false; env BUILDQ_SEED_MEMBERS)

[cargo]
rustc_wrapper = "none"   # Disable global sccache per job ("none" | "keep" | "/path")
# jobs = 8               # CARGO_BUILD_JOBS per build (default: cores / slots)
# backend = "mbx"        # Run queued jobs through mr-boxington ("cargo" | "mbx", default: "cargo")
# nested_mbx = true      # With backend = "mbx", also run plain `cargo` nested inside a job through mbx (default: false; env BUILDQ_NESTED_MBX)
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

## Automatic GC

Post-job GC defaults to `[cache] gc_mode = "background"`. On Unix, detached
collectors require an init that reaps adopted children. In containers, use Docker
`--init`, or run `tini` / `dumb-init` as PID 1. A non-reaping PID 1 can accumulate
terminated collectors; these zombies are not running compilers or held slots.
`buildq config` displays the effective mode and its init requirement.

Only one collector runs at a time. A post-job GC that finds another collector
working leaves a request (`gc.pending` in the cache directory) instead of being
dropped; that collector runs up to three follow-up passes for such requests after
releasing its lock, so members that were busy during its pass are reconsidered
once their jobs finish. `gc --auto --json` reports them as `follow_ups`.

Without a reaping init, collect within the job process instead:

```toml
[cache]
gc_mode = "inline"
```

Inline GC runs after releasing build slots and eligible member leases, and waits
for collection before returning. It can add cleanup latency; automatic deletion
warnings do not replace the build's exit status. Both modes skip collection within
10 minutes while estimated usage is within budget. Background mode checks this
before creating a process, rather than spawning a collector only to skip it.

Alternatively set `BUILDQ_GC=0` and have an orchestrator invoke foreground
`buildq gc` and wait for it. Manual GC and free-space admission checks remain
available. See [Container Process Lifecycle](development.md#container-process-lifecycle).

## Temporary Directory Isolation

Target-pool members and queue slots do not isolate temporary fixtures. Tests that
use process-local counters under a fixed temporary root can collide across
independent worktrees, including when invoked without buildq. buildq preserves
the caller's temporary-directory environment by default.

Opt in per invocation by creating a fresh directory and setting all three
variables for the command and its descendants. This POSIX example assumes the
command's entire process tree finishes before it returns:

```sh
job_tmp="$(mktemp -d /var/tmp/buildq-job.XXXXXXXX)" || exit 1
job_status=0
TMPDIR="$job_tmp" TMP="$job_tmp" TEMP="$job_tmp" buildq cargo test || job_status=$?
# The foreground command and all its descendants have exited here
rm -rf -- "$job_tmp"
exit "$job_status"
```

Choose an executable temporary volume: rustdoc compiles and runs doctest
executables there, so a `noexec` mount can cause failures independently of buildq.
`/var/tmp` above is an example; verify its mount policy. Keep the directory
outside Git checkouts and `BUILDQ_CACHE`. Arbitrary job temporary files are not
pool members, are not counted by the cache budget, and are not GC-owned. The
caller must monitor their disk usage, limit retention, and remove abandoned
directories after verifying that their process trees have exited.

For commands that leave background descendants, have a supervisor wait for the
whole tree before removal. A shell EXIT trap or the initial command's exit alone
does not establish that condition. On cancellation, retain the directory until
that check is complete. This recipe supplies unique inherited paths; programs
that ignore TMPDIR/TMP/TEMP or share other fixed paths still need their own
isolation. See [Container Process Lifecycle](development.md#container-process-lifecycle).

## Nested Cargo

Build scripts can build helpers in separate Cargo workspaces without inheriting
the parent's pooled target. Normal helper targets stay outside the pool; discovered
default directories appear in `cache ls` as external storage and are not pruned.
To keep helper artifacts inside the parent's budget and lease, scope its target
explicitly to `OUT_DIR`:

```rust
let target = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap())
    .join("helper-target");
let status = std::process::Command::new(std::env::var_os("CARGO").unwrap())
    .args(["build", "--manifest-path", "helper/Cargo.toml", "--target-dir"])
    .arg(target)
    .status()
    .unwrap();
assert!(status.success());
```

## mr-boxington (mbx)

`[cargo] backend = "mbx"` runs queued Cargo jobs through
[mr-boxington](https://github.com/jdx/mr-boxington), which restores matching
compiler outputs from its shared store. It is opt-in; the default backend runs
the real Cargo directly.

```toml
[cargo]
backend = "mbx"
# mbx = "/absolute/path/to/mbx"   # Default: `mbx` on PATH
```

Responsibilities stay split:

- buildq keeps job admission, pooled target placement, content-based source
  invalidation, `cargo run` staging, and its own GC. Each job gets
  `MBX_TARGET_VIEWS=0` and `MBX_TARGET_LANES=0`, so mbx does not create
  managed targets or check lanes.
- mbx wraps the compilers. buildq passes the resolved real Cargo as `CARGO`
  and forwards `+toolchain` unchanged, so the real Cargo must be one that
  accepts it (a rustup or mise proxy). mbx takes nothing else before the
  subcommand, so buildq moves Cargo's global options (`--config`, `--color`,
  `-v`, `--locked`, ...) after a built-in subcommand or `clippy`. Before an
  external subcommand, or with `-C`, they cannot move; that job runs the real
  Cargo without mbx and says so. Cargo aliases reach mbx expanded, so an alias
  named like an mbx command (`gc`, `cache`, `doctor`, ...) still runs Cargo.
- Only queued jobs use mbx. Light commands, nested Cargo inside a job, and
  buildq's own `cargo metadata` probes run the real Cargo directly.
- With `nested_mbx = true`, a plain `cargo` that a held job runs from `PATH`
  (the buildq shim) goes through mbx too, still without queueing and with
  mbx's managed targets off, so helper workspaces built that way are cached
  across worktrees. Build scripts that run `$CARGO` call Cargo directly and
  bypass both buildq and mbx.

buildq refuses a combination that would loop or silently stop caching:
`cargo.rustc_wrapper` set to a path (mbx would defer to it), a real Cargo that
resolves to mbx or its Cargo shim, and an `mbx` path that is buildq itself.
Keep `rustc_wrapper = "none"` (the default) or `"keep"`; with `"keep"`, a
`RUSTC_WRAPPER` already in the environment still makes mbx skip caching.

A Cargo found on `PATH` that runs mbx itself (mbx's Cargo shim, a wrapper
script, or a mise shim whose cargo is one of those) would make mbx wrap itself.
It would also route buildq's own Cargo probes through mbx, which then creates
its managed `target` view in the worktree. buildq therefore uses the active
toolchain's cargo from `rustup which cargo` for the job and its probes, and says
so. Without rustup, or when `cargo.real` / `BUILDQ_REAL_CARGO` names such a
Cargo explicitly, the job stops and names those settings.

mbx keeps its own store, private incremental state, and collector. buildq
never deletes mbx's entries, but counts them against `cache.budget`: the bytes
mbx reports beyond those it shares with the pool (see below) come off the
budget, and GC evicts pool members to make room. mbx's own limits stay yours
to set (mbx's config, `MBX_GC_AUTO`, and its budget variables); if mbx alone
outgrows `cache.budget`, GC empties the idle pool and warns. The measurement
runs `mbx cache stats` and walks the store, so GC and admission reuse it for
five minutes (`<cache>/mbx-usage.json`); a failing mbx is then not budgeted.
The `min_free` reserve already covers mbx on the cache volume; if the store
lives on another volume, a build is also refused (or stopped) when that volume
has `min_free` or less, since only mbx's GC can free it.

`buildq cache ls` (and `--json`, as an optional `mbx` object) also runs
`mbx cache stats --json` and reports mbx's store, targets, incremental, and
generated bytes, the pool bytes that are hard links of files in the mbx store
(counted once by device and inode), and the combined unique footprint. mbx
counts logical bytes, so under `accounting = "allocated"` the combined figure
is an estimate. Reflinks cannot be seen as shared, and a store on another
filesystem shares nothing. If mbx cannot be run, `cache ls` prints a note
(`mbx_note` in JSON) instead. `mbx_budget_bytes` is what the budget reserves
for mbx, and `cache ls` always measures afresh.

## Cache Budget

The default `accounting = "allocated"` counts file allocation and deduplicates
hardlinks within each cache member/run/trash item. Sparse holes are excluded.
Choose `[cache] accounting = "logical"` to budget unique logical file lengths
instead. `cache ls`, `gc`, `top`, and `config` label the basis; JSON includes
`accounting`. Allocation excludes directory metadata and conservatively counts
sharing across separate cache items.

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

## Member Deduplication

GC shares byte-identical build outputs between idle members, including members
of other repositories on the same toolchain. Shared files keep their content,
mode, and mtime, so Cargo still treats them as fresh. `cache ls` shows the
shared bytes per pool, and the budget counts them once. `buildq gc --dry-run`
compares files the same way (within `dedupe_max_read`) and projects evictions
from the deduplicated usage.

```sh
buildq cache dedupe --dry-run   # Measure duplication without changing files
buildq cache dedupe             # Share now, regardless of `[cache] dedupe`
```

| Setting | Default | Description |
| :--- | :--- | :--- |
| `dedupe` | `"auto"` | `"off"` disables passes during GC |
| `dedupe_min_size` | `"64KiB"` | Smaller files are not compared |
| `dedupe_max_read` | `"8G"` | Bytes one pass may read for comparisons |
| `dedupe_max_index` | `100000` | Most candidate copies one index holds; a larger toolchain scope is indexed and shared in batches (`index_batches` in the report), walking its members once per batch |

Sharing needs reflinks: btrfs, XFS with `reflink=1`, or bcachefs on Linux, and
APFS on macOS. Elsewhere a pass reports `unsupported` and changes nothing; a
dry run still measures duplication. A dry run's "would share" is content-based
potential, not reclaimable capacity: it reports `sharing` as `supported`,
`unsupported` (with the reason) or `unknown`, from what the last real pass on
this volume observed, and never probes by writing. Run `buildq cache dedupe`
once to find out. See
[Cross-Member Deduplication](architecture.md#cross-member-deduplication).

## Project Cache Cleanup

`cargo clean` keeps Cargo's normal selection: `-p`, `--release`, `--profile`,
`--target`, and `--manifest-path` affect its local/explicit target only. buildq
prints a notice that pooled caches remain unchanged. Explicit target directories
and Cargo target-dir settings retain their normal precedence; paths overlapping
managed cache storage are refused so live leases cannot be bypassed.

To remove this Git repository's pooled targets across all worktrees and toolchains:

```sh
buildq cache clean --dry-run
buildq cache clean
buildq cache clean --json
```

Run this from the intended repository, including when the previous clean used
`--manifest-path` elsewhere. The command clears whole idle target members, rather
than accepting Cargo's package/profile selectors. If any member or leasing section
in a pool is busy or inaccessible, that pool is skipped and the command exits `1`;
other idle toolchain pools may already have been cleaned. Stop/wait for those
jobs and retry. Stable lock files remain.

To remove idle pooled targets of every repository at once, from any directory:

```sh
buildq cache clean --all --dry-run
buildq cache clean --all
```

Busy pools are skipped the same way.

Without `--all`, other repositories are preserved. Run copies and external
target directories are always preserved.
GC handles eligible run copies; use ordinary Cargo clean on an external directory
you own. Report before/after totals cover the managed cache, while actions identify
this repository's removed members. Failed deletions also exit nonzero.

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
