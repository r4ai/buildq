# Development & Setup Guide

English | [日本語](development.ja.md)

This guide covers building and testing `buildq`, codebase organization, and configuring multi-agent sandboxes.

## Repository Structure

```text
src/
  main.rs      # CLI entrypoint and subcommand dispatch
  lib.rs       # Library exports
  config.rs    # Configuration loading (defaults < config.toml < env vars)
  shim.rs      # Cargo shim installation and invocation detection
  job.rs       # Job orchestration (queueing, pool leasing, two-phase cargo run)
  queue.rs     # File-locked priority FIFO queue per job class
  pool.rs      # Target directory pool and member selection
  git.rs       # Git blob manifest calculation and parentless snapshots
  cargo.rs     # Cargo argument parsing, alias resolution, and metadata inspection
  gc.rs        # Disk budget tracking and 5-stage garbage collection
  remote.rs    # SSH + Git remote execution transport and protocol
  exec.rs      # Process spawning, signal forwarding, Windows Job Objects, and the Unix watchdog
  lock.rs      # Cross-platform OS file locks (LockFileEx / flock)
  history.rs   # Job execution history log
  external.rs  # Registry of explicit target dirs used outside the pool
  util.rs      # Byte formatting, time helpers, and path canonicalization
tests/
  cli.rs       # Integration tests (queue concurrency, priority, shim, GC, remote)
  gc.rs        # GC filesystem regressions in a dedicated test process
  e2e.rs       # End-to-end multi-worktree Cargo build & invalidation benchmark
  fixtures/    # Sample workspace used by E2E tests
```

## Running Tests

### Unit & Integration Tests

Run unit tests, multi-process queue tests, shim routing, GC, and local-transport remote protocol tests:

```sh
cargo test
```

Verified behaviors:
- Concurrency never exceeds configured `slots` across real OS processes
- Strict FIFO ordering within a priority level; higher-priority waiters overtake lower-priority ones
- Killing a slot holder immediately frees its slot for the next waiter
- Nested calls (`BUILDQ_HELD`) bypass the queue without deadlocking
- Exit codes (including non-zero and Windows NTSTATUS codes) pass through
- Caller-supplied TMPDIR/TMP/TEMP isolate otherwise-colliding concurrent fixtures on Unix
- `cargo` shim queues heavy commands (including `.cargo/config.toml` aliases) and passes light commands through
- Pool member selection, manifest invalidation, and budget-aware GC skip busy members
- Two-phase `cargo run` binary/DLL syncing and JSON artifact parsing
- Remote snapshot creation, incremental Git push, remote worktree checkout, execution, and `--fetch`

GC filesystem, queue, and pool lease-transition tests use separate integration-test binaries. On Unix, a fork in
an unrelated process-spawning test can briefly inherit a lock before exec closes
the descriptor, making an idle fixture appear busy. Separating these tests keeps
deterministic dry-run/actual comparisons without weakening nonblocking GC locks.

### End-to-End (E2E) Cargo Tests

Runs real `cargo` builds across two Git worktrees on `tests/fixtures/sample-ws` (requires `regex` crates in the local Cargo cache):

```sh
cargo test --test e2e -- --ignored --nocapture
```

## Python Benchmark Tools

`scripts/` is a standalone Python project managed by uv.
`.python-version` pins Python 3.12.14; `uv.lock` pins dependency versions and distribution hashes.
Run these commands from the repository root.

```sh
uv sync --project scripts --locked
uv run --project scripts --locked python scripts/stress-bevy.py \
  --root /tmp/buildq-bevy-new \
  --buildq "$PWD/target/release/buildq"
uv run --project scripts --locked python scripts/plot-bevy-benchmark.py
```

- Activate the Rust toolchain and build buildq with `cargo build --release` first
- Use a new measurement directory; the Linux benchmark needs approximately 12 GiB of free space
- Japanese figures use the system Noto Sans CJK font
  - Select another installation path with `--font /path/to/NotoSansCJK-Regular.ttc`
- Request optional PNG previews with `--preview-dir /tmp/buildq-figures`

Each CLI handles arguments; shared implementation uses normal imports.

| Module | Responsibility |
| :--- | :--- |
| `buildq_bench/measurement.py` | Subprocess measurements, resource sampling, JSONL output |
| `buildq_bench/scenarios.py` | Worktree, cache, and concurrency scenarios |
| `buildq_bench/plotting.py` | Bilingual comparison figures from recorded data |

```sh
uv run --directory scripts --locked pytest
uv run --directory scripts --locked ruff check .
uv run --directory scripts --locked ruff format --check .
```

After changing dependencies, run `uv lock --project scripts` and commit `pyproject.toml` and `uv.lock` together.
Do not commit `.venv`, tool caches, or `__pycache__`.

## Container Process Lifecycle

Use a reaping init such as Docker `--init`, `tini`, or `dumb-init` when enabling
background post-job GC. buildq is daemonless: collectors exit after each pass,
and the operating environment must reap them if their parent has already exited.
PID 1's executable name does not reliably establish whether it reaps children;
check the container's actual supervision policy. `buildq config` exposes the mode.

For environments without that supervision, choose `[cache] gc_mode = "inline"`,
or disable automatic GC and explicitly wait for foreground `buildq gc`.
See [Automatic GC](usage.md#automatic-gc) for configuration and latency tradeoffs.
The Linux `tests/reaping.rs` binary acts as its own subreaper and waits only for
its descendants. It verifies an actual detached collector, inline collection,
and the absence of collector processes on recent under-budget builds.

## Multi-Agent Sandbox Recipe

When running multiple coding agents (e.g., Codex CLI, Claude Code) in restricted sandboxes on the same machine, every agent must share access to `$BUILDQ_DIR` and `$BUILDQ_CACHE`.

### 1. Create a Short Shared Directory

Pick a directory with a **short path** on the same volume as your worktrees (deep Cargo paths inside `target/` can hit Windows `MAX_PATH` limits):

- **Windows**: `C:\bq`
- **Unix / macOS**: `/tmp/bq` or `/var/tmp/bq`

### 2. Configure Sandbox Permissions

- **Agent writable roots**
  - Add `C:\bq` (or `/tmp/bq`) to the writable roots of every agent
- **Windows ACLs**
  - Sandboxed agents run under restricted tokens
  - Files created by one agent must remain modifiable by the next agent
  - Grant an inheritable Modify ACL to the sandbox identity:
    ```powershell
    icacls C:\bq /grant "<sandbox-user-or-group>:(OI)(CI)M"
    ```

### 3. Set Agent Environment Variables

Configure the following environment variables in your user profile or agent launcher:

**PowerShell (Windows)**
```powershell
$env:BUILDQ_DIR   = "C:\bq"
$env:BUILDQ_CACHE = "C:\bq\c"
$env:PATH         = "C:\bq\bin;$env:PATH"
```

**POSIX Shell (macOS / Linux / Git Bash)**
```sh
export BUILDQ_DIR=/tmp/bq
export BUILDQ_CACHE=/tmp/bq/c
export PATH="/tmp/bq/bin:$PATH"
```

### 4. Install the Shim

```sh
buildq shim install cargo --into C:\bq\bin
```

- **Windows**
  - Copies `buildq.exe` to `cargo.exe` alongside a `.buildq-shim` marker file
  - Resolves identically across PowerShell, `cmd.exe`, and Git Bash
  - Re-run `buildq shim install` after upgrading `buildq`
- **Unix**
  - Writes a two-line shell script (`--sh` writes it on Windows too)

## Environment Variables Reference

| Variable | Description |
| :--- | :--- |
| `BUILDQ_DIR` | Directory storing queue locks, `config.toml`, and job history |
| `BUILDQ_CACHE` | Root directory for target pools (`pools/`) and run dirs (`runs/`) |
| `BUILDQ_CONFIG` | Explicit path to `config.toml`<br>(defaults to `$BUILDQ_DIR/config.toml`) |
| `BUILDQ_SLOTS` | Override `slots` (max concurrent `build` jobs) |
| `BUILDQ_BUDGET` | Override disk budget (e.g., `45G`) |
| `BUILDQ_POOL` | Set to `0` to disable target pooling (queue only) |
| `BUILDQ_POOL_EXPLICIT` | Set to `1` to pool explicit target dirs inside the worktree<br>(same as `[cache] pool_explicit`) |
| `BUILDQ_GC` | Set to `0` to disable automatic post-job GC in either mode |
| `BUILDQ_PRIORITY` | Default job priority when invoked via the `cargo` shim |
| `BUILDQ_LABEL` | Free-form label attached to jobs in `status` and `history` |
| `BUILDQ_REAL_CARGO` | Explicit path to the underlying `cargo` binary |
| `BUILDQ_QUIET` | Set to `1` to suppress `buildq:` informational status lines |
| `BUILDQ_TRACE` | Set to `1` to print detailed phase timing diagnostics |

## Releases

Rust edition 2024, MSRV 1.89; CI and releases use Rust 1.99.0 from
`rust-toolchain.toml`. Toolchain updates must keep both CI versions green.
CI checks formatting, strict Clippy, docs, dependencies, and real Cargo E2E
on Linux, macOS, and Windows. Dependencies use weekly grouped Dependabot
updates with a seven-day cooldown; security updates should be reviewed promptly.

1. Update `package.version` in `Cargo.toml` and refresh `Cargo.lock` with
   `cargo update`; merge the reviewed change into `main` after CI passes
2. Tag that commit (`git tag -a v0.1.0 -m "Release v0.1.0"`) and push the tag
   (`git push origin v0.1.0`), replacing the example version as needed
3. The tag workflow reruns CI, checks the version and `main` ancestry, builds
   Linux/macOS x86_64 and aarch64 plus Windows x86_64 binaries, and publishes
   archives, licenses, `SHA256SUMS`, and GitHub-generated release notes

Prerelease tags such as `v0.2.0-rc.1` require the same version in `Cargo.toml`
and create prereleases. Provenance is attached to every archive; verify it with
`gh attestation verify ARCHIVE --repo r4ai/buildq`, and verify downloads with
`sha256sum -c SHA256SUMS` (macOS: `shasum -a 256 -c SHA256SUMS`).
Publication starts as a draft and becomes public only after all assets upload.
If publication fails after draft creation, inspect/delete that draft before
rerunning the failed job; never move a published tag.

Maintainers must allow GitHub Actions and artifact attestations, enable Dependabot
security updates and private vulnerability reporting, require **CI passed** in the
`main` ruleset, and restrict `v*` tag creation/update/deletion to release maintainers.
The public-repository workflow uses only `GITHUB_TOKEN` and GitHub OIDC; no personal
access token or signing secret is needed. See [Security policy](../SECURITY.md).
