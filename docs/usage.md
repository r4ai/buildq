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

[cargo]
rustc_wrapper = "none"   # Disable global sccache per job ("none" | "keep" | "/path")
# jobs = 8               # CARGO_BUILD_JOBS per build (default: cores / slots)
# backend = "mbx"        # Run queued jobs through mr-boxington ("cargo" | "mbx", default: "cargo")
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

mbx keeps its own store, private incremental state, and collector, outside
`cache.budget`. Budget the volume for both, or set `MBX_GC_AUTO` and mbx's
budget explicitly. `buildq gc` collects only buildq's pool.

`buildq cache ls` (and `--json`, as an optional `mbx` object) also runs
`mbx cache stats --json` and reports mbx's store, targets, incremental, and
generated bytes, the pool bytes that are hard links of files in the mbx store
(counted once by device and inode), and the combined unique footprint. mbx
counts logical bytes, so under `accounting = "allocated"` the combined figure
is an estimate. Reflinks cannot be seen as shared, and a store on another
filesystem shares nothing. If mbx cannot be run, `cache ls` prints a note
(`mbx_note` in JSON) instead. Coordinated collection is tracked separately.
