# buildq

A machine-wide queue and a shared, disk-budgeted target-dir pool for cargo
jobs that many coding agents start at the same time, each in its own git
worktree.

Agents keep typing `cargo build`, `cargo test`, `cargo dev`. A `cargo` shim
first on `PATH` sends the heavy ones through buildq, which

- runs at most N of them at once on the machine (default 2), in FIFO order
  with optional priorities;
- builds them in a small pool of shared target dirs instead of one `target/`
  per worktree, so dependencies (and unchanged workspace crates) are compiled
  once for all worktrees, safely;
- keeps that pool under a disk budget;
- can send jobs to other machines over ssh.

No daemon: the queue is plain files plus OS file locks in a directory every
process (and every sandbox) can write.

## Why

Six agents running `cargo build/test/run` at once on a 16-thread Windows box
froze it, and their per-worktree `target/` dirs (5-20 GB each for a Bevy
workspace) took 38 GB of disk within an hour. A shared sccache server did not
help: started inside one agent's sandbox, it could not write the other
worktrees' `target/` dirs.

buildq attacks both problems. The queue bounds CPU and RAM. The pool bounds
disk, and keeps incremental builds warm across worktrees. Because the pool
lives in one directory that every sandbox may write, the sandbox problem
goes away.

## Install

```sh
cargo install --path .
```

Then install the shim into a directory of its own and put that directory first
on `PATH`:

```sh
buildq shim install cargo --into C:\bq\bin      # writes cargo.exe (+ .buildq-shim marker)
```

On Windows the shim is a copy of `buildq.exe` named `cargo.exe`. A real
executable resolves the same way from PowerShell, cmd and Git Bash. On Unix it
is a two-line sh script (`--sh` writes one on Windows too). After upgrading
buildq, run `shim install` again so the copy is refreshed.

The shim finds the real cargo further down `PATH`. It skips its own directory
and any directory that holds a `.buildq-shim` marker, so it never calls
itself. `BUILDQ_REAL_CARGO` or `cargo.real` in the config overrides the lookup.

## Multi-agent setup (the recipe)

1. Pick one shared directory with a **short path** on the same volume as the
   worktrees, for example `C:\bq`. Every sandbox must be able to write it:
   - Codex CLI: add it to the writable roots of every agent.
   - Windows ACLs: sandboxes run with restricted tokens, so files one agent
     creates must stay writable for the next one. Give the directory an
     *inheritable* Modify grant for the identity the sandboxes run as (for
     example `icacls C:\bq /grant "<sandbox user or group>:(OI)(CI)M"`). Treat
     this as a security decision and make it yourself.
2. Put this in every agent's environment (user env vars, or the agent
   launcher):

   ```powershell
   $env:BUILDQ_DIR   = "C:\bq"           # queue, config, history
   $env:BUILDQ_CACHE = "C:\bq\c"         # pool root (keep it short: deep cargo paths)
   $env:PATH         = "C:\bq\bin;$env:PATH"
   ```

   ```sh
   export BUILDQ_DIR=/c/bq BUILDQ_CACHE=/c/bq/c PATH="/c/bq/bin:$PATH"
   ```

3. `buildq shim install cargo --into C:\bq\bin`
4. Optional: `C:\bq\config.toml` (all keys are optional):

   ```toml
   slots = 2            # concurrent build-class jobs on this machine
   run_slots = 2        # concurrent app runs (the run phase of `cargo run`)

   [cache]
   max_members = 3      # target dirs per (repo, toolchain); >= slots
   budget = "45G"       # everything under BUILDQ_CACHE
   min_free = "25G"     # and never leave less than this free on the volume

   [cargo]
   rustc_wrapper = "none"   # default: override the global sccache wrapper per job
   # jobs = 8               # default: cores / slots
   ```

5. Check it: `cargo --version` (passes straight through), then `cargo build`
   in a worktree. It prints `buildq: target <pool>/m0 (...)`. `buildq status`
   shows holders and waiters.

The host's global `~/.cargo/config.toml` sets `build.rustc-wrapper = sccache`
and `build.jobs = 8`. buildq overrides both per job: `RUSTC_WRAPPER=""`
disables a configured wrapper, and `CARGO_BUILD_JOBS` is set to
`cores / slots` unless the job passes `-j` or the env var is already set.
For `test`, `bench` and `nextest`, `RUST_TEST_THREADS` gets the same cap.

## Commands

| command | what it does |
| --- | --- |
| `buildq run [-p PRIO] [--class C] [--label L] -- cmd...` | wait for a slot, run, pass the exit code through; `cargo` gets the full treatment below |
| `buildq cargo ARGS` | what the shim runs |
| `buildq run --host H -- cmd...` | run on an ssh host (`H` = name, `auto`, or `a,b,local`) |
| `buildq status [--json] [--hosts]` | slot holders and waiters per class (and per host) |
| `buildq history [-n N] [--json]` | finished jobs: wait, run time, exit, pool member |
| `buildq cache ls [--json] [--refresh]` | pools, members, sizes, budget, free disk |
| `buildq gc [--dry-run] [--budget 40G]` | prune to the budget |
| `buildq shim install cargo --into DIR` | install the shim |
| `buildq config` | effective configuration and paths |

The shim queues `build check test run clippy bench doc rustc rustdoc fix install
nextest llvm-cov miri careful hack`, and aliases that resolve to them.
Aliases come from `[alias]` in every `.cargo/config.toml` from the current
directory up, then `$CARGO_HOME/config.toml`, plus `CARGO_ALIAS_*`; built-in
`b c d t r` count too. Everything else passes straight through: `fmt`,
`metadata`, `tree`, `--version`, `help`, `build --help`, and so on.
`[cargo] heavy` / `extra_heavy` change the list.

Environment knobs: `BUILDQ_DIR`, `BUILDQ_CACHE`, `BUILDQ_CONFIG`,
`BUILDQ_SLOTS`, `BUILDQ_BUDGET`, `BUILDQ_POOL=0` (queue only),
`BUILDQ_GC=0`, `BUILDQ_PRIORITY`, `BUILDQ_LABEL`, `BUILDQ_REAL_CARGO`,
`BUILDQ_QUIET=1`, `BUILDQ_TRACE=1` (phase timings).

## How the queue works

```text
$BUILDQ_DIR/<class>/
  queue.lock               short critical section (enqueue, hand out a slot)
  seq                      ticket counter (wall-clock ms as a floor)
  slot-<i>.lock / .json    held by the job in slot i for its whole life / who it is
  tickets/t-<seq>-<pid>.lock / .json   one per waiting job, locked by the waiter
```

- Every lock is an OS file lock (`LockFileEx` on Windows, `flock` on Unix,
  through `std::fs::File::lock`). It belongs to an open handle, so a process
  that dies or is killed releases its slot and its ticket without any cleanup.
  A ticket whose lock can be taken is dead and gets pruned. A slot whose lock
  can be taken is free.
- A waiter polls (250 ms by default). Under `queue.lock` it lists the live
  tickets sorted by `(priority desc, seq asc)` and takes a free slot only if
  its rank is below the number of free slots. This gives strict FIFO within
  a priority. A higher priority overtakes waiting jobs, never running ones.
- Classes have separate slots: `build` (default `slots`), `run`
  (`run_slots`), `host-<name>` (that host's `slots`), and any name listed in
  `[classes]` (for example `gpu = 1`).
- Nesting: a job's children get `BUILDQ_HELD=<classes held>`. A buildq or
  shim call that finds its class there runs directly. So cargo calling cargo,
  a build script running `cargo metadata`, or a test calling `cargo build`
  cannot deadlock on its own slot.
- While a child runs, buildq ignores Ctrl+C (the console delivers it to the
  child too) and returns the child's exit code. On Windows the child runs in
  a kill-on-close job object: if buildq is killed, the whole build tree dies
  with it, so a freed slot never hides a build that is still running
  (`kill_tree = false` turns this off). On Unix, SIGTERM and SIGHUP are
  forwarded to the child.
- Lock files and JSON files carry no secrets. Lock files are opened
  read-only if another sandbox created them read-only. Metadata that cannot
  be overwritten only degrades `status` output.

## How the cache pool works

### Layout

```text
$BUILDQ_CACHE/
  pools/<repo>-<hash(repo, toolchain)>/
    pool.json                 repo, toolchain (rustc -vV)
    pool.lock                 short critical section for picking a member
    m<i>/                     a plain cargo target dir (CARGO_TARGET_DIR of a job)
    m<i>.lock                 held by the job using the member
    m<i>.json                 last use, last worktree, size, job count
    m<i>.manifest             path -> git blob id that the member's artifacts reflect
  runs/r<i>/ (+ .lock/.json)  copies of binaries being run
  trash/                      half-deleted trees (renamed aside first)
```

A pool is one repository (all worktrees share git's common dir) on one
toolchain. A pool has up to `max_members` members, and each job leases one
member exclusively for its duration. Cargo would serialise two builds in one
target dir anyway, so you need at least `slots` members for parallel builds.
More members give worktrees more room to keep their own warm state.

### What actually gets reused (measured, not assumed)

Cargo names artifacts `<crate>-<hash>`. For workspace members the hash takes
the package path *relative to the workspace root*, so `crates/core` hashes the
same in every worktree. Two worktrees on the same commit share every artifact,
workspace crates included. Third-party dependencies are shared anyway. The
e2e test below confirms it: the second worktree's first build compiles
nothing.

That same property makes a naively shared `CARGO_TARGET_DIR` unsafe. Cargo
decides freshness by mtime: an artifact counts as fresh if it is newer than
the sources. Say worktree B changed `core` but its file is older than A's
artifact (checked out earlier, or edited before A built). Then B silently gets
A's code. The e2e test reproduces this with plain cargo: after the edit, the
naive shared dir still prints the old string.

### The fix: source manifests and package invalidation

Each member records, for every file, the git blob id of the content its
artifacts were last known to reflect (`m<i>.manifest`). Before a job:

1. buildq takes the current worktree's manifest: `git ls-files -s` plus
   `git status`, with `git hash-object` for modified and untracked files.
   Ignored files are left out.
2. It diffs that manifest against the member's and maps each differing file
   to its package: the nearest directory with a `Cargo.toml` that has
   `[package]`.
3. It deletes those packages' fingerprints (`.fingerprint/<pkg>-*` in every
   profile and target dir of the member). Cargo must then rebuild exactly
   those packages and their dependants, from this worktree's sources, however
   old its mtimes are. rustc's incremental cache still applies, so a rebuild
   is incremental.
4. It writes the current manifest as the member's.

Invariant: for every package, either its fingerprints are absent, or its
artifacts were built from the manifest's content. That holds for partial
builds (`-p x`), failed builds and killed builds too, because the
invalidation happens before the build and is never undone. Changed files
outside any package, such as files pulled in with `include_str!("../../x")`,
are *touched* in the current worktree, which covers the common cases. After
the job, files edited *while* cargo ran (dirty and newer than the job start)
get the same treatment.

### Picking a member

Under `pool.lock`, every free member is scored by
`(never built, packages to invalidate, differing files, last used by another
worktree, least recent)`, and the lowest score wins. So a worktree usually
returns to the member it used last. Failing that, it gets the member whose
sources are closest to its own (for example the same branch point), which
maximises incremental reuse. A new member is created only when every member
is busy and the pool is below `max_members`, and, beyond `slots` members,
only if the disk budget has room for another member of average size.
Otherwise the job waits for a member.

### `cargo run` (and aliases like `cargo dev`)

On Windows a running exe is locked, so an app running out of a shared target
dir would break every later build of that binary in that member. buildq
therefore splits `run` into two phases:

1. **Build phase**, under a `build` slot and a member: `cargo build` with the
   same arguments, plus `--bin` chosen the way `cargo run` would choose it
   (`cargo metadata`: `-p`, the package of the current directory,
   `default-run`, or the single binary), plus
   `--message-format=json-render-diagnostics`. Diagnostics still render
   normally. buildq reads the artifact messages to find the executable, the
   DLLs/shared libraries of the build and the build-script link paths, and
   copies them into a free run dir (`runs/r<i>`). Only files whose size or
   mtime changed are copied, so a large `bevy_dylib` is copied once, not per
   run. `.pdb` files come along for backtraces (`copy_pdb`).
2. **Run phase**: the slot and the member are released, and the program runs
   from the run dir under a `run` slot. Its cwd is your cwd. `PATH` (or
   `LD_LIBRARY_PATH`) gets the run dir, the link paths and the toolchain's
   lib dirs. `CARGO_MANIFEST_DIR`, `CARGO_PKG_*` and `CARGO` are set as
   `cargo run` sets them, so Bevy still finds `assets/`.

A `run` that already passes `--message-format` is queued as a single job
instead.

### Target dirs given explicitly

`--target-dir`, `CARGO_TARGET_DIR` or `CARGO_BUILD_TARGET_DIR` set by the
caller: the job is queued but not pooled. A relative
`--config build.target-dir="target/native"`, as in an alias that builds with
`-Ctarget-cpu=native`, is re-rooted inside the member (`m<i>/sub/native`). It
stays pooled and keeps its separation.

### Alternatives considered

- **One target dir per worktree** (the status quo): correct, but N copies of
  every dependency and a cold first build for every new worktree.
- **Naively shared `CARGO_TARGET_DIR`**: maximal reuse, but stale artifacts
  (shown above), and every build of the machine serialises on one cargo lock.
- **A stable build path per member** (sync the worktree into a mirror
  checkout and build there): makes every absolute path identical, but
  `CARGO_MANIFEST_DIR`, test fixtures, snapshot files and the app's asset dir
  would point at the mirror, and writes would land there. Rejected. With
  relative-path hashing, the manifest approach gets the same reuse without
  moving sources.
- **`--remap-path-prefix`**: only changes paths embedded in outputs (debug
  info, panics). It does not affect cargo's hashing, which is already
  relative. Not needed.
- **sccache**: its server must be able to write every target dir. It still
  does not cache incremental (dev profile) workspace crates, and the
  per-member pool already shares dependency builds. buildq turns the wrapper
  off per job by default. `rustc_wrapper = "keep"` or a path re-enables it.
  This works if the server and cache live in a directory every sandbox can
  write.

## Disk budget and garbage collection

`buildq gc` runs automatically in the background after pooled jobs (throttled
to every 10 minutes unless over budget; `gc_after_job = false` or
`BUILDQ_GC=0` turns it off). Cheapest steps first; busy members are never
touched:

1. leftovers in `trash/`;
2. inside idle members: incremental sessions other than the newest per crate,
   incremental crate dirs untouched for `incremental_days` (3), and
   superseded unit variants (same package, another hash) older than
   `stale_unit_days` (14). The variant rule works like cargo-sweep.
3. whole pools unused for `pool_idle_days` (21): old toolchains, dead
   repositories;
4. run dirs idle for two days;
5. while still over budget: whole members, least recently used first.

The effective budget is `min(budget, used + free - min_free)`, so when the
disk fills for other reasons the pool shrinks too. Defaults: `budget = 60G`,
`min_free = 20G`. `buildq cache ls` shows the sizes, both budgets and free
space. `buildq gc --dry-run` lists what would go.

## Remote hosts

```toml
# $BUILDQ_DIR/config.toml
[hosts.box]
ssh = "box"                  # an alias from ~/.ssh/config (default: the table name)
slots = 4                    # jobs this machine sends there at once
root = ".buildq"             # remote root, relative to the remote home
buildq = "buildq"            # optional: also queue and pool on the host
env = { CARGO_TERM_COLOR = "always" }

auto_hosts = ["local", "box"]
```

```sh
buildq run --host box -- cargo test -p x
buildq run --host auto -- cargo clippy --all-targets
buildq run --host box --fetch target/debug/app -- cargo build -p app
buildq status --hosts
```

Protocol (plain ssh and git; the host needs git, a POSIX shell and the
toolchain):

1. Take a local `host-<name>` slot. The host's `slots` bound what this machine
   sends there, even without a remote buildq.
2. Snapshot the worktree as a **parentless commit**: tracked plus untracked
   files, minus ignored ones, built through a copy of the index. HEAD, the
   index and the files stay untouched.
3. `git init --bare <root>/<repo>-<hash>/repo.git` on the host (idempotent),
   then `git push` the snapshot to `refs/buildq/<worktree-id>`. A local ref
   `refs/buildq/remote/<host>/<id>` keeps the previous snapshot, so later
   pushes send only changed blobs and never the history.
4. Check it out into `<root>/<repo>-<hash>/wt/<worktree-id>`, a `git worktree`
   of the bare repo. git rewrites only changed files, so mtimes, and with them
   cargo's incremental state on the host, survive between jobs.
5. Run the command in the same subdirectory as locally. With `buildq` set,
   it runs as `buildq run -- <cmd>`, so the host's own queue and pool apply.
   Output streams back over ssh (`-t` when attached to a terminal, which also
   forwards Ctrl+C). The remote exit code is returned.
6. `--fetch PATH` copies paths relative to the remote worktree back with
   `scp` (default destination `target/buildq-remote/<host>/`). With a remote
   buildq, fetch runs set `BUILDQ_POOL=0` there so the paths live under the
   worktree.

`--host auto` (or `--host a,b,local`) picks the host this worktree used last
if it has a free slot, because its checkout and caches are warm. Otherwise it
picks the host with the largest share of free slots, or failing that the
shortest queue per slot. `buildq status --hosts` asks each host's buildq for
its status. The transport is a trait (`remote::Transport`), and the protocol
test runs against a local directory instead of ssh.

## Tests

```sh
cargo test                                    # unit + process-level + remote protocol
cargo test --test e2e -- --ignored --nocapture   # real cargo, two worktrees (needs the
                                              # regex crates in the local registry cache)
```

The tests check that:

- concurrency never exceeds the slots, across real processes;
- FIFO order holds, and priority overtakes waiting jobs;
- a killed holder frees its slot;
- nested calls work;
- exit codes pass through;
- the shim queues heavy commands (including aliases) with a pool target, and
  passes light ones straight through;
- member selection, invalidation and gc behave on fake target trees;
- budget eviction skips busy members;
- run-dir sync and artifact parsing work;
- the remote snapshot/push/worktree/run/fetch round trip works.

E2E on the sample workspace (`tests/fixtures/sample-ws`: `regex`, plus three
workspace crates), Windows 11, a machine already loaded by other agents:

| step | wall | crates compiled |
| --- | ---: | ---: |
| worktree 1, cold build (new member) | 12.5 s | 8 |
| worktree 2, first build, shared member | 1.4 s | 0 |
| control: worktree 2 with its own `target/` | 11.2 s | 8 |
| worktree 2 edits the bottom crate (old mtime) + `cargo run` | 4.3 s | 3 (prints the new code) |
| back to worktree 1 via an alias (`cargo sample`) | 4.1 s | 3 |
| control: naive shared target dir, same edit | | 0, **prints the old code** |

Disk: one 113 MiB member serves both worktrees, against 110 MiB for each
worktree's own `target/`. The saving grows linearly with the number of
worktrees, and for a Bevy workspace it is gigabytes per worktree. Per queued
command, buildq adds about 0.4 to 0.6 s on that loaded machine, mostly three
git processes.

## Limitations and next steps

- Freshness for files that git ignores is still cargo's mtime rule: generated
  sources under ignored paths, and files outside the repository. Changed
  files outside any package are only touched, not tracked per crate.
- Absolute paths inside reused artifacts (debug info, `file!()` in
  dependencies outside the workspace) point at the worktree that built them.
- A program that a job starts and deliberately leaves running is killed with
  the job on Windows (`kill_tree`). Long-lived servers started by builds
  (sccache, if re-enabled) restart on the next job.
- Tools that call `target/debug/x.exe` directly no longer find fresh
  binaries in the worktree. Set `[cargo] mirror_bins = true` to copy new
  top-level binaries back after each pooled build, or use `cargo run`.
- `cargo run` replicates the environment of `cargo run` except `[env]` from
  cargo config.
- The queue polls (250 ms). It is fair and cheap, but not instant.
- Remote: the host must have a POSIX shell (a Windows host with OpenSSH needs
  Git Bash as its default shell). rsync sync and per-host toolchain checks
  are not implemented. `--host auto` uses local accounting plus affinity, not
  live remote load. Remote artifacts come back only through `--fetch`.
- Next: a cargo-aware weight for member choice (dependants of an invalidated
  package), size estimates without full tree walks, a live remote load probe,
  rsync transport, and a `buildq top` view.

## License

MIT OR Apache-2.0, at your option. See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).
