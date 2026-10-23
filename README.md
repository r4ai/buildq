# buildq

English | [日本語](README.ja.md)

**A daemonless job queue and shared `target/` directory pool<br>for running parallel `cargo` builds across Git worktrees**

---

## Why buildq?

When multiple coding agents run `cargo` simultaneously across separate Git worktrees, three problems occur:

- **CPU & RAM exhaustion**
  - Concurrent Rust compilations freeze the machine
- **Disk explosion**
  - Each worktree creates its own `target/` directory (5–20 GB each)
  - Disk usage reaches dozens of gigabytes within an hour
- **Unsafe naive sharing**
  - Sharing a single `CARGO_TARGET_DIR` serializes all builds on one Cargo lock
  - `mtime` differences across worktrees cause Cargo to silently reuse stale binaries

`buildq` sits behind a transparent `cargo` shim and solves all three problems.
It requires no background daemon.

```mermaid
flowchart LR
    subgraph Agents["Parallel Agents (Git Worktrees)"]
        A1["Worktree A: cargo build"]
        A2["Worktree B: cargo test"]
        A3["Worktree C: cargo run"]
    end

    Shim["cargo shim"]
    Queue["FIFO Queue (Max N Slots)"]

    subgraph Pool["Shared Target Pool (Disk-Budgeted)"]
        M0["Member m0 (target/)"]
        M1["Member m1 (target/)"]
    end

    A1 & A2 & A3 --> Shim
    Shim --> Queue
    Queue --> M0 & M1
```

### Key Features

- **Transparent `cargo` shim**
  - Intercepts heavy commands (`build`, `test`, `clippy`, `run`, `nextest`, etc.)
  - Passes lightweight commands (`fmt`, `metadata`, `tree`, `--version`) straight through
- **Machine-wide job queue**
  - Caps concurrent builds (default: `2`) in FIFO order with priority support
  - Uses OS file locks (`flock` / `LockFileEx`) with no background daemon
  - Releases OS locks automatically if the holder exits; Unix child-process cleanup has a [known limitation](https://github.com/r4ai/buildq/issues/13)
- **Safe shared `target/` pool**
  - Shares a small pool of `target/` directories across all worktrees of a repository
  - Tracks file content via Git blob hashes
  - Invalidates only modified packages before each build
- **Automatic disk budget & GC**
  - Prunes stale incremental sessions and unused artifacts in the background
  - Evicts idle build members toward a configurable disk budget (default: `60 GB`); busy artifacts and recent run copies can keep usage above it ([#14](https://github.com/r4ai/buildq/issues/14))
- **Lock-free `cargo run`**
  - Copies compiled binaries and DLLs to an isolated run directory before execution
  - Prevents running executables from locking the shared `target/` directory on Windows
- **Remote execution over SSH**
  - Offloads builds to remote machines using lightweight, incremental Git snapshots

### Performance & Impact

Measured on 2026-10-23 with **Bevy 0.16.1** (PBR, PNG, multithreading; 210 compilation steps), Linux x86_64, Rust 1.89.0, a **4-CPU quota / 16 GiB RAM**, and two Cargo jobs per build. buildq used **2 slots / 2 pool members**. Dependencies were fetched before timing; builds used `--offline --locked` and `debug = 0`.

| Metric | Plain Cargo, independent targets | buildq |
| :--- | :--- | :--- |
| Single cold build | 116.05 s | 114.32 s |
| Three worktree builds, all caches initially empty | 165.14 s | **120.80 s (26.8% less time)** |
| Peak aggregate process-tree RSS during those three builds | 4.37 GiB | **2.89 GiB (33.8% lower)** |
| Allocated target disk space after those builds | 3.21 GiB (3 targets) | **2.14 GiB (2 members)** |
| Warm build, median of 10 runs | 0.203 s | 0.220 s via `buildq cargo`; **0.235 s via the installed cargo shim** |

A second worktree reused the pool in **0.204 s with no recompilation**. After a source edit with an artificially old `mtime`, naive target sharing ran stale code; buildq rebuilt the app and ran the correct version in **3.55 s**. Bevy's `dynamic_linking` run and a build while another app was running also passed.

The three-build comparison includes pool reuse: two builds compile from cold, and the third reuses a completed member. Cold measurements are one batch per condition; the single-build difference is not evidence of a speedup. RSS is sampled process-tree RSS, not exclusive physical memory. This is a headless build/run benchmark, not a GPU-rendering benchmark or a guarantee for other machines.

**Known limitations found in this run:** killing only the buildq parent on Unix can leave Cargo alive after the slot/member is released ([#13](https://github.com/r4ai/buildq/issues/13)); recent idle run copies can remain above the requested GC budget ([#14](https://github.com/r4ai/buildq/issues/14)). Neither issue is fixed by these measurements.

[Detailed report and methodology (Japanese)](docs/benchmarks/bevy-2026-10-23.ja.md) · [Raw results](docs/benchmarks/data/bevy-2026-10-23/summary.json) · [Reproducible benchmark](scripts/stress-bevy.py)

---

## Installation

### 1. Install the binary

```sh
cargo install --git https://github.com/r4ai/buildq
```

### 2. Install the `cargo` shim

Create a directory for the shim and place it **first** on your `PATH`:

**macOS / Linux**
```sh
buildq shim install cargo --into ~/.local/share/buildq/bin
export PATH="$HOME/.local/share/buildq/bin:$PATH"
```

**Windows (PowerShell)**
```powershell
buildq shim install cargo --into C:\bq\bin
$env:PATH = "C:\bq\bin;$env:PATH"
```

> [!TIP]
> Running multiple sandboxed agents? Set `BUILDQ_DIR` and `BUILDQ_CACHE` to a shared writable directory.
> See the [Multi-Agent Sandbox Recipe](docs/development.md#multi-agent-sandbox-recipe) for details.

---

## Usage

### Everyday Use

Once the shim is on your `PATH`, run `cargo` commands as usual:

```sh
cargo build
cargo test
cargo run
```

`buildq` automatically queues heavy commands and prints the leased pool member:

```text
buildq: target <pool>/m0 (...)
```

### CLI Commands

| Command | Description |
| :--- | :--- |
| `buildq status [--hosts]` | Show active slot holders and waiting jobs per class |
| `buildq history [-n N]` | Show recent jobs<br>(wait time, run time, exit code, pool member) |
| `buildq cache ls [--refresh]` | Show target pools, member sizes, budget, and free space |
| `buildq gc [--dry-run]` | Manually prune caches toward the disk budget (busy artifacts and recent run copies may remain) |
| `buildq config` | Print effective configuration and directory paths |
| `buildq run [-p PRIO] -- <cmd>...` | Queue any arbitrary command (with optional priority) |
| `buildq run --host <H> -- <cmd>...` | Run a queued command on a remote SSH host |

### Configuration (`config.toml`)

All settings are optional.
Place `config.toml` in `$BUILDQ_DIR/config.toml` (check `buildq config` for the default path on your OS):

```toml
slots = 2                # Max concurrent build jobs (default: 2)
run_slots = 2            # Max concurrent app runs for `cargo run` (default: 2)

[cache]
max_members = 3          # Max target/ dirs per repo+toolchain (default: slots + 1)
budget = "45G"           # Max disk usage under BUILDQ_CACHE (default: "60G")
min_free = "25G"         # Minimum free disk space to preserve on volume (default: "20G")
# pool_explicit = true   # Also pool explicit --target-dir / CARGO_TARGET_DIR inside the worktree (default: false)

[cargo]
rustc_wrapper = "none"   # Disable global sccache per job ("none" | "keep" | "/path")
# jobs = 8               # CARGO_BUILD_JOBS per build (default: cores / slots)
```

### Remote Builds (SSH)

Add remote hosts to `config.toml` to offload builds over SSH and Git:

```toml
[hosts.box]
ssh = "box"              # Host alias from ~/.ssh/config
slots = 4                # Max concurrent jobs sent to this host

auto_hosts = ["local", "box"]
```

Run commands on a specific host or let `buildq` pick one automatically:

```sh
buildq run --host box -- cargo test
buildq run --host auto -- cargo clippy --all-targets
buildq run --host box --fetch target/debug/app -- cargo build --bin app
```

---

## How It Works

```mermaid
flowchart TD
    Start["cargo build / test / run"] --> Check["Check subcommand & aliases"]
    Check -- "Light (fmt, tree)" --> Pass["Bypass buildq"]
    Check -- "Heavy (build, test, run)" --> Lock["Acquire OS lock (BUILDQ_DIR)"]
    Lock --> Lease["Lease warm target/ member (m0)"]
    Lease --> Diff["Diff Git blobs vs m0.manifest"]
    Diff --> Invalidate["Delete .fingerprint/ of changed crates"]
    Invalidate --> Build["Run cargo (CARGO_TARGET_DIR=m0)"]
```

1. **OS File-Locked Queue**
   - Exclusive OS file locks (`flock` / `LockFileEx`) in `$BUILDQ_DIR`
   - Automatic lock release on process crash or kill—no stale locks
   - Deadlock-free nested `cargo` calls via `BUILDQ_HELD` detection
2. **Content-Addressed Manifest Invalidation**
   - Cross-worktree artifact reuse via Cargo's workspace-relative path hashing
   - Per-file Git blob tracking in `m0.manifest`
   - Targeted `.fingerprint/<pkg>-*` deletion for modified packages before each build
   - Fast, accurate incremental rebuilds independent of file `mtime`
3. **Two-Phase `cargo run`**
   - Phase 1: Compile under a `build` slot and copy the binary + DLLs to `runs/r0`
   - Phase 2: Release the `build` slot and `target/` member, then launch the binary under a `run` slot
   - Zero `target/` locking while applications are running

---

## Documentation

- **[Architecture & Internals](docs/architecture.md)**
  - Queue lock design, manifest invalidation invariant, and member selection scoring
  - 5-stage GC, remote SSH protocol, design alternatives, and limitations
- **[Development & Setup Guide](docs/development.md)**
  - Codebase layout and running unit / E2E tests
  - Multi-agent sandbox setup and environment variables

## License

Dual-licensed under MIT or Apache-2.0 ([LICENSE-MIT](LICENSE-MIT) / [LICENSE-APACHE](LICENSE-APACHE))
