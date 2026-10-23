//! Job orchestration: queue -> (pool member) -> child process -> history.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cargo::{self, Parsed, Route, TargetDirChoice};
use crate::config::{Config, WrapperMode};
use crate::exec;
use crate::git::{self, Worktree};
use crate::history;
use crate::lock::{self, LockFile};
use crate::pool::{self, Lease, PackageOwners, Pool, PoolInfo};
use crate::queue::{ClassQueue, JobInfo, SlotGuard, WaitState};
use crate::util::{display_cmd, human_bytes, now_ms, read_json, system_time_ms, write_json};

#[derive(Debug, Clone, Default)]
pub struct Opts {
    pub class: String,
    pub priority: i32,
    pub label: Option<String>,
}

impl Opts {
    /// Defaults for the shim: class build, priority from `BUILDQ_PRIORITY`.
    pub fn from_env() -> Opts {
        Opts {
            class: "build".into(),
            priority: env::var("BUILDQ_PRIORITY")
                .ok()
                .and_then(|p| p.trim().parse().ok())
                .unwrap_or(0),
            label: env::var("BUILDQ_LABEL").ok().filter(|s| !s.is_empty()),
        }
    }
}

pub fn quiet() -> bool {
    env::var_os("BUILDQ_QUIET").is_some_and(|v| !v.is_empty() && v != "0")
}

/// `BUILDQ_TRACE=1`: phase timings on stderr.
pub fn trace(what: &str) {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    let start = *START.get_or_init(std::time::Instant::now);
    if env::var_os("BUILDQ_TRACE").is_some() {
        eprintln!("buildq: [{:>6} ms] {what}", start.elapsed().as_millis());
    }
}

pub fn note(msg: &str) {
    if !quiet() {
        eprintln!("buildq: {msg}");
    }
}

/// Classes whose slot an ancestor process already holds.
pub fn held_classes() -> Vec<String> {
    env::var("BUILDQ_HELD")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn is_held(class: &str) -> bool {
    let h = held_classes();
    // Legacy/boolean form: BUILDQ_HELD=1 means "everything".
    h.iter().any(|c| c == class || c == "1" || c == "all")
}

fn held_with(class: &str) -> String {
    let mut h = held_classes();
    if !h.iter().any(|c| c == class) {
        h.push(class.to_string());
    }
    h.join(",")
}

fn cwd() -> PathBuf {
    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn job_info(opts: &Opts, cmd: &str) -> JobInfo {
    let cwd = cwd();
    let worktree = crate::git::discover(&cwd)
        .map(|wt| wt.key())
        .unwrap_or_else(|| crate::util::norm_key(&crate::util::canonical(&cwd)));
    JobInfo {
        pid: std::process::id(),
        class: opts.class.clone(),
        cmd: cmd.to_string(),
        cwd: cwd.display().to_string(),
        worktree: Some(worktree),
        label: opts.label.clone(),
        priority: opts.priority,
        ..Default::default()
    }
}

pub fn acquire(cfg: &Config, opts: &Opts, cmd: &str) -> Result<SlotGuard, String> {
    let class = &opts.class;
    let q = ClassQueue::new(
        &cfg.queue_dir,
        class,
        cfg.class_slots(class),
        Duration::from_millis(cfg.poll_ms),
    )
    .with_max_per_worktree(cfg.max_per_worktree);
    let mut printed = false;
    let g = q
        .acquire(job_info(opts, cmd), &mut |w: WaitState| {
            note(&format!(
                "waiting for a {class} slot: {}/{} busy, position {} of {}",
                w.busy,
                q.slots,
                w.position + 1,
                w.waiters
            ));
            printed = true;
        })
        .map_err(|e| format!("queue {}: {e}", q.dir.display()))?;
    if printed {
        note(&format!(
            "got {class} slot {} after {}s",
            g.slot,
            g.waited_ms / 1000
        ));
    }
    Ok(g)
}

fn record(
    cfg: &Config,
    g: &SlotGuard,
    exit: i32,
    lease: Option<&Lease>,
    pool_name: Option<&str>,
    external: Option<&Path>,
) {
    let mut target = g.info.target.clone();
    if let (Some(l), Some(p)) = (lease, pool_name) {
        target = Some(format!("{p}/{}", l.name()));
    }
    if let Some(dir) = external {
        crate::external::record(cfg, dir, &g.info.cwd);
    }
    history::append(
        &cfg.queue_dir,
        &history::Entry {
            pid: g.info.pid,
            class: g.info.class.clone(),
            cmd: g.info.cmd.clone(),
            cwd: g.info.cwd.clone(),
            enqueued_ms: g.info.enqueued_ms,
            started_ms: g.info.started_ms,
            ended_ms: now_ms(),
            exit,
            target,
            external: external.map(|d| d.display().to_string()),
            invalidated: lease.map(|l| l.plan.packages.len()).unwrap_or(0),
            fresh_member: lease.map(|l| l.fresh_member).unwrap_or(false),
            label: g.info.label.clone(),
        },
    );
}

/// `buildq run -- <cmd...>`
pub fn run_command(cfg: &Config, opts: &Opts, argv: &[String]) -> Result<i32, String> {
    if argv.is_empty() {
        return Err("no command given".into());
    }
    if crate::util::file_stem_lower(&argv[0]) == "cargo" && opts.class == "build" {
        return run_cargo(cfg, opts, &argv[1..]);
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    if is_held(&opts.class) {
        return exec::run(&mut cmd, false).map_err(|e| format!("{}: {e}", argv[0]));
    }
    let g = acquire(cfg, opts, &display_cmd(argv))?;
    cmd.env("BUILDQ_HELD", held_with(&opts.class));
    let code = exec::run(&mut cmd, cfg.kill_tree).map_err(|e| format!("{}: {e}", argv[0]));
    record(cfg, &g, *code.as_ref().unwrap_or(&127), None, None, None);
    code
}

fn real_cargo(cfg: &Config) -> Result<PathBuf, String> {
    cargo::find_real_cargo(
        cfg.real_cargo.as_deref(),
        crate::shim::shim_dir().as_deref(),
    )
    .ok_or_else(|| "cannot find the real cargo on PATH (set BUILDQ_REAL_CARGO)".to_string())
}

pub struct Toolchain {
    pub id: String,
    pub release: String,
    pub host: String,
}

fn rustc_cmd(real: &Path, toolchain: Option<&str>) -> Command {
    let rustc = env::var_os("RUSTC").map(PathBuf::from).unwrap_or_else(|| {
        let sib = real.with_file_name(if cfg!(windows) { "rustc.exe" } else { "rustc" });
        if sib.is_file() {
            sib
        } else {
            PathBuf::from("rustc")
        }
    });
    let mut c = Command::new(rustc);
    if let Some(t) = toolchain {
        c.env("RUSTUP_TOOLCHAIN", t);
    }
    c.stdin(Stdio::null()).stderr(Stdio::null());
    c
}

/// `rustc -vV` through a rustup proxy costs a few hundred ms, so results are
/// cached under a key that changes whenever the selected toolchain can:
/// override, `rust-toolchain` files, and rustup's toolchain dir and settings.
pub fn probe_toolchain(real: &Path, toolchain: Option<&str>) -> Option<Toolchain> {
    let Some(key) = toolchain_key(real, toolchain) else {
        return probe_toolchain_uncached(real, toolchain);
    };
    let cache_path = crate::config::Config::load()
        .ok()?
        .queue_dir
        .join("toolchains.json");
    let mut cache: BTreeMap<String, (u64, String)> = read_json(&cache_path).unwrap_or_default();
    if let Some((at, id)) = cache.get(&key)
        && now_ms().saturating_sub(*at) < 3_600_000
    {
        return Some(Toolchain::from_id(id));
    }
    let tc = probe_toolchain_uncached(real, toolchain)?;
    cache.retain(|_, (at, _)| now_ms().saturating_sub(*at) < 3_600_000);
    cache.insert(key, (now_ms(), tc.id.clone()));
    let _ = write_json(&cache_path, &cache);
    Some(tc)
}

fn toolchain_key(real: &Path, toolchain: Option<&str>) -> Option<String> {
    if env::var_os("RUSTC").is_some() {
        return None;
    }
    let rustup_home = env::var_os("RUSTUP_HOME").map(PathBuf::from).or_else(|| {
        env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .map(|h| PathBuf::from(h).join(".rustup"))
    })?;
    let mtime = |p: &Path| {
        fs::metadata(p)
            .and_then(|m| m.modified())
            .map(system_time_ms)
            .unwrap_or(0)
    };
    let toolchains = rustup_home.join("toolchains");
    if !toolchains.is_dir() {
        return None;
    }
    let mut key = format!(
        "{}|{:?}|{:?}|{}|{}",
        real.display(),
        toolchain,
        env::var("RUSTUP_TOOLCHAIN").ok(),
        mtime(&toolchains),
        mtime(&rustup_home.join("settings.toml"))
    );
    let here = cwd();
    let mut dir = Some(here.as_path());
    while let Some(d) = dir {
        for f in ["rust-toolchain.toml", "rust-toolchain"] {
            if let Ok(s) = fs::read_to_string(d.join(f)) {
                key.push_str(&format!("|{}|{s}", d.display()));
            }
        }
        dir = d.parent();
    }
    Some(key)
}

impl Toolchain {
    fn from_id(id: &str) -> Toolchain {
        let field = |k: &str| {
            id.lines()
                .find_map(|l| l.strip_prefix(k))
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        };
        Toolchain {
            release: field("release:"),
            host: field("host:"),
            id: id.to_string(),
        }
    }
}

fn probe_toolchain_uncached(real: &Path, toolchain: Option<&str>) -> Option<Toolchain> {
    let out = rustc_cmd(real, toolchain).arg("-vV").output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(Toolchain::from_id(&String::from_utf8_lossy(&out.stdout)))
}

fn sysroot(real: &Path, toolchain: Option<&str>) -> Option<PathBuf> {
    let out = rustc_cmd(real, toolchain)
        .args(["--print", "sysroot"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

fn with_toolchain(parsed: &Parsed, args: Vec<String>) -> Vec<String> {
    match &parsed.toolchain {
        Some(t) => std::iter::once(format!("+{t}")).chain(args).collect(),
        None => args,
    }
}

/// Environment every queued cargo job gets.
fn cargo_env(cmd: &mut Command, cfg: &Config, parsed: &Parsed, target: Option<&Path>) {
    if let Some(t) = target {
        cmd.env("CARGO_TARGET_DIR", t);
    }
    let has_jobs = parsed
        .sub_args()
        .iter()
        .any(|a| a == "-j" || a.starts_with("--jobs") || (a.starts_with("-j") && a.len() > 2));
    if !has_jobs && env::var_os("CARGO_BUILD_JOBS").is_none() {
        cmd.env("CARGO_BUILD_JOBS", cfg.jobs.to_string());
    }
    match &cfg.wrapper {
        // An empty RUSTC_WRAPPER overrides `build.rustc-wrapper` from config files.
        WrapperMode::Disable => {
            cmd.env("RUSTC_WRAPPER", "");
        }
        WrapperMode::Path(p) => {
            cmd.env("RUSTC_WRAPPER", p);
        }
        WrapperMode::Keep => {}
    }
    if cfg.limit_test_threads
        && matches!(parsed.sub(), Some("test" | "bench" | "nextest"))
        && env::var_os("RUST_TEST_THREADS").is_none()
    {
        cmd.env("RUST_TEST_THREADS", cfg.jobs.to_string());
    }
    cmd.env("BUILDQ_HELD", held_with("build"));
}

struct PoolCtx {
    wt: Worktree,
    pool: Pool,
    name: String,
    manifest: pool::Manifest,
    rebuild_graph: pool::RebuildGraph,
    /// An explicit target dir inside the worktree pooled under
    /// `cache.pool_explicit`: (requested dir, its path relative to the worktree).
    explicit: Option<(PathBuf, String)>,
}

/// The absolute dir a job's explicit target-dir choice points at, if any:
/// `--target-dir`/`CARGO_TARGET_DIR`, or an absolute `build.target-dir`
/// (relative ones are re-rooted into the member anyway).
fn explicit_target_dir(choice: &TargetDirChoice, here: &Path) -> Option<PathBuf> {
    let value = match choice {
        TargetDirChoice::Explicit { value, .. } if !value.is_empty() => value,
        TargetDirChoice::Config(_, value) if Path::new(value).is_absolute() => value,
        _ => return None,
    };
    Some(crate::util::resolve(here, Path::new(value)))
}

/// `dir` relative to the worktree root (forward slashes), if it lies inside.
fn inside_worktree(dir: &Path, wt: &Worktree) -> Option<String> {
    let root = crate::util::canonical(&wt.root);
    let rel = dir.strip_prefix(&root).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

/// The pool to build in, or else the explicit target dir the job uses
/// outside the pool (which is then announced and recorded).
fn open_pool(cfg: &Config, real: &Path, parsed: &Parsed) -> (Option<PoolCtx>, Option<PathBuf>) {
    if !cfg.pool {
        return (None, None);
    }
    let here = cwd();
    let explicit = explicit_target_dir(&cargo::target_dir_choice(parsed), &here);
    let wt = git::discover(&here);
    let explicit = match (explicit, &wt) {
        (None, _) => None,
        (Some(dir), Some(wt)) if cfg.pool_explicit => match inside_worktree(&dir, wt) {
            Some(rel) => Some((dir, rel)),
            None => return (None, Some(note_unpooled(dir, false))),
        },
        (Some(dir), wt) => {
            let hint = wt
                .as_ref()
                .is_some_and(|w| inside_worktree(&dir, w).is_some());
            return (None, Some(note_unpooled(dir, hint)));
        }
    };
    let ctx = wt.and_then(|wt| pool_ctx(real, parsed, cfg, wt, explicit.clone()));
    match (ctx, explicit) {
        (Some(c), _) => (Some(c), None),
        (None, explicit) => (None, explicit.map(|(dir, _)| dir)),
    }
}

/// One line on stderr: this job's target dir is outside the pool and the budget.
fn note_unpooled(dir: PathBuf, hint: bool) -> PathBuf {
    note(&format!(
        "explicit target dir {}: not pooled, outside the disk budget{}",
        dir.display(),
        if hint {
            " (set [cache] pool_explicit = true to pool it)"
        } else {
            ""
        }
    ));
    dir
}

fn pool_ctx(
    real: &Path,
    parsed: &Parsed,
    cfg: &Config,
    wt: Worktree,
    explicit: Option<(PathBuf, String)>,
) -> Option<PoolCtx> {
    let tc = probe_toolchain(real, parsed.toolchain.as_deref())?;
    let dir = pool::pool_dir(&cfg.cache_dir, &wt, &tc.id);
    let name = dir.file_name()?.to_string_lossy().to_string();
    let pool = Pool::open(
        dir,
        PoolInfo {
            repo: wt.repo_name(),
            repo_key: wt.repo_key(),
            toolchain: tc.release.clone(),
            toolchain_id: tc.id.clone(),
            created_ms: now_ms(),
            last_used_ms: now_ms(),
        },
    )
    .ok()?;
    Some(PoolCtx {
        manifest: git::manifest(&wt)?,
        rebuild_graph: workspace_metadata(real, parsed)
            .as_ref()
            .and_then(pool::RebuildGraph::from_metadata)
            .unwrap_or_default(),
        wt,
        pool,
        name,
        explicit,
    })
}

fn lease_member(cfg: &Config, ctx: &mut PoolCtx) -> Result<Lease, String> {
    let mut owners_here = PackageOwners::new(vec![PathBuf::from(&ctx.wt.root)]);
    let mut owners_prev: BTreeMap<String, PackageOwners> = BTreeMap::new();
    let mut owners = |f: &str, prev: Option<&str>| {
        owners_here.owner(f).or_else(|| {
            let prev = prev?;
            owners_prev
                .entry(prev.to_string())
                .or_insert_with(|| PackageOwners::new(vec![PathBuf::from(prev)]))
                .owner(f)
        })
    };
    let slots = cfg.slots;
    let mut may_grow = |count: usize| {
        if count < slots {
            return true;
        }
        let u = crate::gc::scan(cfg, false);
        let members: usize = u
            .pools
            .iter()
            .map(|p| p.members.len())
            .sum::<usize>()
            .max(1);
        u.total + u.total / members as u64 <= u.effective_budget
    };
    let lease = ctx
        .pool
        .lease(
            &ctx.wt,
            &ctx.manifest,
            &ctx.rebuild_graph,
            &mut owners,
            cfg.max_members,
            &mut may_grow,
            Duration::from_millis(cfg.poll_ms),
            &mut || note("all cache members busy and pool at its limit; waiting for one"),
        )
        .map_err(|e| format!("pool {}: {e}", ctx.pool.dir.display()))?;
    let touched = pool::touch(&ctx.wt.root, &lease.plan.outside);
    let what = if lease.fresh_member {
        "new member (full build)".to_string()
    } else if lease.plan.packages.is_empty() {
        "no source differences".to_string()
    } else {
        let names: Vec<_> = lease.plan.packages.iter().take(6).cloned().collect();
        let more = lease.plan.packages.len().saturating_sub(6);
        format!(
            "{} package(s) invalidated: {}{}",
            lease.plan.packages.len(),
            names.join(", "),
            if more > 0 {
                format!(" +{more}")
            } else {
                String::new()
            }
        )
    };
    let touched = if touched > 0 {
        format!(", {touched} file(s) outside packages touched")
    } else {
        String::new()
    };
    note(&format!(
        "target {}/{} ({what}{touched})",
        ctx.name,
        lease.name()
    ));
    Ok(lease)
}

/// After the job: a file edited while cargo ran may have been compiled in
/// its old or new form, so its package is invalidated for the next worktree.
/// Only uncommitted files touched since the job started can be affected,
/// which keeps this to one `git status`.
fn after_job(ctx: &mut PoolCtx, lease: &mut Lease, started_ms: u64) {
    let changed: Vec<String> = git::dirty(&ctx.wt)
        .into_iter()
        .filter(|p| {
            fs::metadata(ctx.wt.root.join(p))
                .and_then(|m| m.modified())
                .map(|t| system_time_ms(t) + 1000 >= started_ms)
                .unwrap_or(ctx.manifest.contains_key(p))
        })
        .collect();
    if !changed.is_empty() {
        let mut now = ctx.manifest.clone();
        let existing: Vec<String> = changed
            .iter()
            .filter(|p| ctx.wt.root.join(p).is_file())
            .cloned()
            .collect();
        for p in &changed {
            now.remove(p);
        }
        for (p, id) in git::hash_objects(&ctx.wt, &existing).unwrap_or_default() {
            now.insert(p, id);
        }
        if now != ctx.manifest {
            let mut owners = PackageOwners::new(vec![PathBuf::from(&ctx.wt.root)]);
            let p = pool::plan(&ctx.manifest, &now, &mut |f| owners.owner(f));
            pool::invalidate(&lease.target_dir, &p.packages);
            let _ = pool::write_manifest(&ctx.pool.manifest_path(lease.id), &now);
        }
    }
    lease.finish();
}

fn spawn_gc(cfg: &Config) {
    if !cfg.gc_after_job {
        return;
    }
    if let Ok(me) = env::current_exe() {
        let mut c = Command::new(me);
        c.args(["gc", "--auto"]).env("BUILDQ_AS_CLI", "1");
        let _ = exec::spawn_detached(&mut c);
    }
}

/// The shim and `buildq run -- cargo ...` end up here.
pub fn run_cargo(cfg: &Config, opts: &Opts, args: &[String]) -> Result<i32, String> {
    trace("start");
    let real = real_cargo(cfg)?;
    let passthrough = |args: &[String]| {
        let mut c = Command::new(&real);
        c.args(args);
        exec::run(&mut c, false).map_err(|e| format!("{}: {e}", real.display()))
    };
    if is_held("build") {
        return passthrough(args);
    }
    let here = cwd();
    let aliases = cargo::load_aliases(&here);
    let parsed = cargo::parse(args, &aliases);
    if cargo::route(&parsed, &|s| cfg.is_heavy(s)) == Route::Passthrough {
        return passthrough(args);
    }
    if parsed.sub() == Some("run") && opt_absent(&parsed, "--message-format") {
        return cargo_run(cfg, opts, &real, args, &parsed);
    }
    trace("parsed");
    let mut g = acquire(cfg, opts, &format!("cargo {}", display_cmd(args)))?;
    trace("slot acquired");
    let (mut ctx, external) = open_pool(cfg, &real, &parsed);
    trace("pool opened (git + toolchain + manifest)");
    let mut lease = match ctx.as_mut() {
        Some(c) => Some(lease_member(cfg, c)?),
        None => None,
    };
    trace("member leased");
    let (cmd_args, target) = cargo_args(args, &parsed, lease.as_ref(), ctx.as_ref());
    if let (Some(l), Some(c)) = (&lease, &ctx) {
        g.set_target(&format!("{}/{}", c.name, l.name()));
    }
    let started = now_ms();
    let mut cmd = Command::new(&real);
    cmd.args(&cmd_args);
    cargo_env(&mut cmd, cfg, &parsed, target.as_deref());
    let code =
        exec::run(&mut cmd, cfg.kill_tree).map_err(|e| format!("{}: {e}", real.display()))?;
    trace("cargo finished");
    if let (Some(c), Some(l)) = (ctx.as_mut(), lease.as_mut()) {
        after_job(c, l, started);
        trace("after-job check");
        if code == 0
            && let Some(dest) = mirror_dest(cfg, c, &parsed)
        {
            mirror_bins(target.as_deref().unwrap_or(&l.target_dir), &dest, started);
        }
    }
    record(
        cfg,
        &g,
        code,
        lease.as_ref(),
        ctx.as_ref().map(|c| c.name.as_str()),
        external.as_deref(),
    );
    drop(lease);
    drop(g);
    if ctx.is_some() {
        spawn_gc(cfg);
    }
    trace("done");
    Ok(code)
}

fn opt_absent(p: &Parsed, name: &str) -> bool {
    !p.sub_args()
        .iter()
        .any(|a| a == name || a.starts_with(&format!("{name}=")))
}

/// Arguments for the real cargo plus the CARGO_TARGET_DIR to set.
/// A `build.target-dir` from `--config`, or an explicit target dir pooled
/// under `cache.pool_explicit`, is re-rooted into `<member>/sub/`.
/// CARGO_TARGET_DIR outranks `--config` in cargo, so it carries the
/// re-rooted dir too.
fn cargo_args(
    args: &[String],
    parsed: &Parsed,
    lease: Option<&Lease>,
    ctx: Option<&PoolCtx>,
) -> (Vec<String>, Option<PathBuf>) {
    let Some(l) = lease else {
        return (args.to_vec(), None);
    };
    let choice = cargo::target_dir_choice(parsed);
    let explicit = ctx.and_then(|c| c.explicit.as_ref());
    let new = match (&choice, explicit) {
        (_, Some((_, rel))) => cargo::reroot_target_dir(rel, &l.target_dir),
        (TargetDirChoice::Config(_, value), None) => cargo::reroot_target_dir(value, &l.target_dir),
        _ => None,
    };
    let Some(new) = new else {
        return (args.to_vec(), Some(l.target_dir.clone()));
    };
    let shown = new.display().to_string().replace('\\', "/");
    let rewrite = match choice {
        TargetDirChoice::Config(idx, _) => {
            Some((idx, "--config=", format!("build.target-dir={shown:?}")))
        }
        TargetDirChoice::Explicit { arg: Some(idx), .. } => Some((idx, "--target-dir=", shown)),
        _ => None,
    };
    let Some((idx, prefix, v)) = rewrite else {
        return (args.to_vec(), Some(new));
    };
    let mut a = parsed.args.clone();
    a[idx] = if a[idx].starts_with(prefix) {
        format!("{prefix}{v}")
    } else {
        v
    };
    (with_toolchain(parsed, a), Some(new))
}

/// Where top-level binaries of a successful pooled build are copied back
/// to: the target dir cargo would have used without buildq. Always for an
/// explicit target dir pooled under `cache.pool_explicit` (the job named
/// that path), otherwise only with `cargo.mirror_bins`.
fn mirror_dest(cfg: &Config, ctx: &PoolCtx, parsed: &Parsed) -> Option<PathBuf> {
    if let Some((requested, _)) = &ctx.explicit {
        return Some(requested.clone());
    }
    if !cfg.mirror_bins {
        return None;
    }
    Some(match cargo::target_dir_choice(parsed) {
        TargetDirChoice::Config(_, v) => crate::util::resolve(&cwd(), Path::new(&v)),
        _ => ctx.wt.root.join("target"),
    })
}

/// Copies new top-level executables of a pooled build (`<src>/<profile>/`)
/// into `<dest>/<profile>/`, for scripts that call `target/debug/x.exe`.
fn mirror_bins(src: &Path, dest: &Path, since_ms: u64) {
    for profile in fs::read_dir(src).into_iter().flatten().flatten() {
        let pdir = profile.path();
        if !pdir.join(".fingerprint").is_dir() {
            continue;
        }
        let dest = dest.join(profile.file_name());
        for f in fs::read_dir(&pdir).into_iter().flatten().flatten() {
            let p = f.path();
            let Ok(md) = f.metadata() else { continue };
            let fresh = md.modified().map(system_time_ms).unwrap_or(0) + 2000 >= since_ms;
            if md.is_file() && fresh && is_binary(&p, &md) {
                let _ = fs::create_dir_all(&dest);
                let _ = fs::copy(&p, dest.join(f.file_name()));
            }
        }
    }
}

fn is_binary(p: &Path, _md: &fs::Metadata) -> bool {
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    if cfg!(windows) {
        matches!(ext, "exe" | "dll" | "pdb")
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            return ext.is_empty() && _md.permissions().mode() & 0o111 != 0
                || matches!(ext, "so" | "dylib");
        }
        #[allow(unreachable_code)]
        false
    }
}

// ---------------------------------------------------------------------------
// cargo run: build in the pool, copy the binary out, release the build slot,
// then run under a `run` slot. A running exe in a shared target dir would
// otherwise block every later build of it on Windows.

#[derive(Debug, Default, Clone)]
struct Artifacts {
    exe: Option<(String, String, PathBuf, Vec<PathBuf>)>, // (package_id, manifest_path, exe, its files)
    libs: Vec<PathBuf>,
    linked: Vec<PathBuf>,
}

fn collect(line: &str, want: Option<&str>, art: &mut Artifacts) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    let Some(reason) = v.get("reason").and_then(Value::as_str) else {
        return false;
    };
    let strs = |v: &Value| -> Vec<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    match reason {
        "compiler-artifact" => {
            let files = strs(&v["filenames"]);
            for f in &files {
                let ext = Path::new(f)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("");
                if matches!(ext, "dll" | "so" | "dylib") {
                    art.libs.push(PathBuf::from(f));
                }
            }
            if let Some(exe) = v.get("executable").and_then(Value::as_str) {
                let kinds = strs(&v["target"]["kind"]);
                let name = v["target"]["name"].as_str().unwrap_or("");
                let runnable = kinds.iter().any(|k| k == "bin" || k == "example");
                if runnable && want.is_none_or(|w| w == name) {
                    art.exe = Some((
                        v["package_id"].as_str().unwrap_or("").to_string(),
                        v["manifest_path"].as_str().unwrap_or("").to_string(),
                        PathBuf::from(exe),
                        files.iter().map(PathBuf::from).collect(),
                    ));
                }
            }
        }
        "build-script-executed" => {
            for p in strs(&v["linked_paths"]) {
                let path = match p.split_once('=') {
                    Some(("native" | "dependency" | "crate" | "framework" | "all", rest)) => {
                        rest.to_string()
                    }
                    _ => p,
                };
                art.linked.push(PathBuf::from(path));
            }
        }
        _ => {}
    }
    true
}

/// Picks `--bin` the way `cargo run` would, so the build does not compile
/// every binary of the package.
fn pick_bin(real: &Path, parsed: &Parsed) -> Option<String> {
    if parsed.opt(&["--bin"]).is_some() || parsed.opt(&["--example"]).is_some() {
        return None;
    }
    let meta = workspace_metadata(real, parsed)?;
    pick_bin_from_metadata(&meta, parsed)
}

/// Offline, dependency-free workspace inspection; failure only loses a hint.
fn workspace_metadata(real: &Path, parsed: &Parsed) -> Option<Value> {
    let mut c = Command::new(real);
    let mut args = vec![
        "metadata".to_string(),
        "--no-deps".into(),
        "--format-version".into(),
        "1".into(),
        "--offline".into(),
    ];
    if let Some(m) = parsed.opt(&["--manifest-path"]) {
        args.push("--manifest-path".into());
        args.push(m);
    }
    let mut options = parsed.args.iter().take_while(|a| a.as_str() != "--");
    while let Some(arg) = options.next() {
        if arg == "--config" {
            args.push(arg.clone());
            args.push(options.next()?.clone());
        } else if arg.starts_with("--config=") {
            args.push(arg.clone());
        }
    }
    c.args(with_toolchain(parsed, args))
        .env("BUILDQ_HELD", held_with("build"))
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    let out = c.output().ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

fn pick_bin_from_metadata(meta: &Value, parsed: &Parsed) -> Option<String> {
    let packages = meta["packages"].as_array()?;
    let want_pkg = parsed
        .opt(&["-p", "--package"])
        .map(|p| p.split('@').next().unwrap_or("").to_string());
    let here = crate::util::canonical(&cwd());
    let pkg = match want_pkg {
        Some(name) => packages
            .iter()
            .find(|p| p["name"].as_str() == Some(&name))?,
        None => packages
            .iter()
            .filter(|p| {
                let dir = Path::new(p["manifest_path"].as_str().unwrap_or(""))
                    .parent()
                    .map(crate::util::canonical);
                dir.is_some_and(|d| here.starts_with(&d))
            })
            .max_by_key(|p| p["manifest_path"].as_str().map(str::len).unwrap_or(0))
            .or_else(|| {
                let defaults = meta["workspace_default_members"].as_array()?;
                (defaults.len() == 1)
                    .then(|| packages.iter().find(|p| p["id"] == defaults[0]))
                    .flatten()
            })?,
    };
    if let Some(d) = pkg["default_run"].as_str() {
        return Some(d.to_string());
    }
    let bins: Vec<&str> = pkg["targets"]
        .as_array()?
        .iter()
        .filter(|t| {
            t["kind"]
                .as_array()
                .is_some_and(|k| k.iter().any(|x| x == "bin"))
        })
        .filter_map(|t| t["name"].as_str())
        .collect();
    (bins.len() == 1).then(|| bins[0].to_string())
}

fn cargo_run(
    cfg: &Config,
    opts: &Opts,
    real: &Path,
    args: &[String],
    parsed: &Parsed,
) -> Result<i32, String> {
    let sub = parsed.sub_index.unwrap();
    let bin = pick_bin(real, parsed);
    let mut build: Vec<String> = parsed.args[..sub].to_vec();
    build.push("build".into());
    build.extend(parsed.sub_args().iter().cloned());
    build.push("--message-format=json-render-diagnostics".into());
    if let Some(b) = &bin {
        build.push("--bin".into());
        build.push(b.clone());
    }
    let build_parsed = Parsed {
        args: build.clone(),
        sub_index: Some(sub),
        ..parsed.clone()
    };
    let want = bin
        .clone()
        .or_else(|| parsed.opt(&["--bin"]))
        .or_else(|| parsed.opt(&["--example"]));

    let mut g = acquire(cfg, opts, &format!("cargo {}", display_cmd(args)))?;
    let (mut ctx, external) = open_pool(cfg, real, &build_parsed);
    let mut lease = match ctx.as_mut() {
        Some(c) => Some(lease_member(cfg, c)?),
        None => None,
    };
    let (cmd_args, target) = cargo_args(
        &with_toolchain(parsed, build),
        &build_parsed,
        lease.as_ref(),
        ctx.as_ref(),
    );
    if let (Some(l), Some(c)) = (&lease, &ctx) {
        g.set_target(&format!("{}/{}", c.name, l.name()));
    }
    let started = now_ms();
    let mut cmd = Command::new(real);
    cmd.args(&cmd_args).stdout(Stdio::piped());
    cargo_env(&mut cmd, cfg, &build_parsed, target.as_deref());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("{}: {e}", real.display()))?;
    let guard = exec::ChildGuard::attach(&child, cfg.kill_tree);
    let mut art = Artifacts::default();
    if let Some(out) = child.stdout.take() {
        let stdout = io::stdout();
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if !collect(&line, want.as_deref(), &mut art) {
                let _ = writeln!(stdout.lock(), "{line}");
            }
        }
    }
    let code = exec::exit_code(child.wait().map_err(|e| e.to_string())?);
    drop(guard);

    let mut run = None;
    if code == 0 {
        match &art.exe {
            Some((pkg_id, manifest, exe, files)) => {
                let (dir, lock) = lease_run_dir(cfg, exe).map_err(|e| format!("run dir: {e}"))?;
                let mut to_copy: Vec<PathBuf> = vec![exe.clone()];
                if cfg.copy_pdb {
                    to_copy.extend(
                        files
                            .iter()
                            .filter(|f| f.extension().is_some_and(|e| e == "pdb"))
                            .cloned(),
                    );
                }
                for lib in &art.libs {
                    to_copy.push(lib.clone());
                    let pdb = lib.with_extension("pdb");
                    if cfg.copy_pdb && pdb.is_file() {
                        to_copy.push(pdb);
                    }
                }
                let copied = sync_into(&dir, &to_copy)
                    .map_err(|e| format!("copy to {}: {e}", dir.display()))?;
                if copied > 0 {
                    note(&format!(
                        "copied {} to run dir {}",
                        human_bytes(copied),
                        dir.display()
                    ));
                }
                run = Some((dir, lock, pkg_id.clone(), manifest.clone(), exe.clone()));
            }
            None => {
                eprintln!("buildq: the build produced no runnable binary; pass --bin or -p");
                return Ok(101);
            }
        }
    }
    if let (Some(c), Some(l)) = (ctx.as_mut(), lease.as_mut()) {
        after_job(c, l, started);
    }
    record(
        cfg,
        &g,
        code,
        lease.as_ref(),
        ctx.as_ref().map(|c| c.name.as_str()),
        external.as_deref(),
    );
    drop(lease);
    drop(g);
    if ctx.is_some() {
        spawn_gc(cfg);
    }
    let Some((dir, _run_lock, pkg_id, manifest, exe)) = run else {
        return Ok(code);
    };

    // Run phase.
    let run_opts = Opts {
        class: "run".into(),
        ..opts.clone()
    };
    let exe_name = exe.file_name().unwrap();
    let local = dir.join(exe_name);
    let trailing = parsed.trailing().to_vec();
    let rg = if is_held("run") {
        None
    } else {
        Some(acquire(
            cfg,
            &run_opts,
            &format!("{} {}", exe_name.to_string_lossy(), display_cmd(&trailing)),
        )?)
    };
    let mut cmd = Command::new(&local);
    cmd.args(&trailing);
    crate::cargo_config::apply_env(&mut cmd, &cwd(), parsed)?;
    run_env(
        &mut cmd,
        real,
        parsed,
        &dir,
        &art.linked,
        RunPackage {
            exe: &exe,
            id: &pkg_id,
            manifest: &manifest,
        },
    );
    cmd.env("BUILDQ_HELD", held_with("run"));
    let code =
        exec::run(&mut cmd, cfg.kill_tree).map_err(|e| format!("{}: {e}", local.display()))?;
    if let Some(rg) = &rg {
        record(cfg, rg, code, None, None, None);
    }
    Ok(code)
}

/// The pool member's `deps` dir and profile dir next to the built binary.
///
/// Third-party DLLs and shared libraries that a build script drops there are
/// not artifacts of any compile unit, so only this search path finds them
/// (`cargo run` puts `deps` on the path for the same reason). The run dir
/// comes first, so copied libraries win over these.
fn build_dirs(exe: &Path) -> Vec<PathBuf> {
    // `<profile>/app`, or `<profile>/examples/app`.
    let Some(profile) = exe
        .ancestors()
        .skip(1)
        .take(2)
        .find(|d| d.join("deps").is_dir())
    else {
        return Vec::new();
    };
    vec![profile.join("deps"), profile.to_path_buf()]
}

struct RunPackage<'a> {
    exe: &'a Path,
    id: &'a str,
    manifest: &'a str,
}

/// The environment `cargo run` would give the program.
// These inputs mirror Cargo metadata and the launch context; keep them explicit.
#[allow(clippy::too_many_arguments)]
fn run_env(
    cmd: &mut Command,
    real: &Path,
    parsed: &Parsed,
    dir: &Path,
    linked: &[PathBuf],
    package: RunPackage<'_>,
) {
    let mut paths: Vec<PathBuf> = vec![dir.to_path_buf()];
    paths.extend(linked.iter().cloned());
    paths.extend(build_dirs(package.exe));
    if let (Some(sr), Some(tc)) = (
        sysroot(real, parsed.toolchain.as_deref()),
        probe_toolchain(real, parsed.toolchain.as_deref()),
    ) {
        if cfg!(windows) {
            paths.push(sr.join("bin"));
        }
        paths.push(sr.join("lib").join("rustlib").join(&tc.host).join("lib"));
    }
    let var = if cfg!(windows) {
        "PATH"
    } else if cfg!(target_os = "macos") {
        "DYLD_FALLBACK_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    };
    let configured = cmd
        .get_envs()
        .find(|(key, _)| *key == var)
        .and_then(|(_, value)| value.map(|v| v.to_os_string()));
    if let Some(old) = configured.or_else(|| env::var_os(var)) {
        paths.extend(env::split_paths(&old));
    }
    if let Ok(joined) = env::join_paths(paths) {
        cmd.env(var, joined);
    }
    let manifest = PathBuf::from(package.manifest);
    if let Some(d) = manifest.parent() {
        cmd.env("CARGO_MANIFEST_DIR", d);
    }
    cmd.env("CARGO_MANIFEST_PATH", &manifest);
    cmd.env("CARGO", real);
    let (name, version) = parse_package_id(package.id, &manifest);
    cmd.env("CARGO_PKG_NAME", name);
    cmd.env("CARGO_PKG_VERSION", &version);
    let mut parts = version.split(['.', '-']);
    for k in [
        "CARGO_PKG_VERSION_MAJOR",
        "CARGO_PKG_VERSION_MINOR",
        "CARGO_PKG_VERSION_PATCH",
    ] {
        cmd.env(k, parts.next().unwrap_or("0"));
    }
}

/// `path+file:///x/app#0.1.0`, `path+file:///x/dir#app@0.1.0`, legacy `app 0.1.0 (path+...)`.
pub fn parse_package_id(id: &str, manifest: &Path) -> (String, String) {
    if let Some((_, frag)) = id.rsplit_once('#') {
        if let Some((n, v)) = frag.split_once('@') {
            return (n.to_string(), v.to_string());
        }
        let name = manifest
            .parent()
            .and_then(|d| d.file_name())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        return (name, frag.to_string());
    }
    let mut it = id.split(' ');
    (
        it.next().unwrap_or("").to_string(),
        it.next().unwrap_or("").to_string(),
    )
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct RunIndex {
    exe: String,
    files: BTreeMap<String, (u64, u64)>,
}

/// A free run dir, preferring one that already holds this executable.
fn lease_run_dir(cfg: &Config, exe: &Path) -> io::Result<(PathBuf, LockFile)> {
    let root = crate::gc::runs_dir(cfg);
    fs::create_dir_all(&root)?;
    let _g = lock::lock(&root.join("runs.lock"))?;
    let exe_name = exe
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut ids: Vec<usize> = fs::read_dir(&root)?
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_string_lossy()
                .strip_prefix('r')?
                .split('.')
                .next()?
                .parse()
                .ok()
        })
        .collect();
    ids.sort();
    ids.dedup();
    let mut fallback = None;
    for id in &ids {
        let lp = root.join(format!("r{id}.lock"));
        if let Some(l) = lock::try_lock(&lp)? {
            let idx = read_json::<RunIndex>(&root.join(format!("r{id}.json"))).unwrap_or_default();
            if idx.exe == exe_name {
                return Ok((root.join(format!("r{id}")), l));
            }
            if fallback.is_none() {
                fallback = Some((root.join(format!("r{id}")), l));
            }
        }
    }
    if let Some(f) = fallback {
        return Ok(f);
    }
    let id = (0..).find(|i| !ids.contains(i)).unwrap();
    let l = lock::try_lock(&root.join(format!("r{id}.lock")))?
        .ok_or_else(|| io::Error::other("run dir busy"))?;
    Ok((root.join(format!("r{id}")), l))
}

/// Mirrors `files` into `dir` (flat), copying only what changed and deleting
/// leftovers. Returns bytes copied.
fn sync_into(dir: &Path, files: &[PathBuf]) -> io::Result<u64> {
    fs::create_dir_all(dir)?;
    let index_path = dir.with_extension("json");
    let mut index = read_json::<RunIndex>(&index_path).unwrap_or_default();
    let mut keep = BTreeMap::new();
    let mut copied = 0;
    for f in files {
        let Some(name) = f.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        let Ok(md) = fs::metadata(f) else { continue };
        let stamp = (md.len(), md.modified().map(system_time_ms).unwrap_or(0));
        let dest = dir.join(&name);
        if index.files.get(&name) != Some(&stamp) || !dest.exists() {
            fs::copy(f, &dest)?;
            copied += md.len();
        }
        keep.insert(name, stamp);
    }
    for e in fs::read_dir(dir)?.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if !keep.contains_key(&n) {
            let _ = fs::remove_file(e.path());
        }
    }
    index.exe = files
        .first()
        .and_then(|f| f.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    index.files = keep;
    write_json(&index_path, &index)?;
    Ok(copied)
}

#[cfg(test)]
mod tests {
    #[test]
    fn build_dirs_cover_deps_for_bins_and_examples() {
        let t = tempfile::tempdir().unwrap();
        let profile = t.path().join("debug");
        std::fs::create_dir_all(profile.join("deps")).unwrap();
        std::fs::create_dir_all(profile.join("examples")).unwrap();
        let want = vec![profile.join("deps"), profile.clone()];
        assert_eq!(build_dirs(&profile.join("app")), want);
        assert_eq!(build_dirs(&profile.join("examples").join("demo")), want);
        assert!(build_dirs(&t.path().join("elsewhere").join("app")).is_empty());
    }

    use super::*;

    #[test]
    fn package_ids() {
        let m = Path::new("/w/crates/app/Cargo.toml");
        assert_eq!(
            parse_package_id("path+file:///w/crates/app#0.1.0", m),
            ("app".into(), "0.1.0".into())
        );
        assert_eq!(
            parse_package_id("path+file:///w/crates/x#open-racing-app@0.2.0", m),
            ("open-racing-app".into(), "0.2.0".into())
        );
        assert_eq!(
            parse_package_id("app 0.1.0 (path+file:///w)", m),
            ("app".into(), "0.1.0".into())
        );
    }

    #[test]
    fn artifacts_from_messages() {
        let mut a = Artifacts::default();
        let lines = [
            r#"{"reason":"compiler-artifact","package_id":"p#dy@1.0.0","manifest_path":"/w/dy/Cargo.toml","target":{"name":"dy","kind":["dylib"]},"filenames":["/t/debug/deps/dy.dll","/t/debug/deps/dy.dll.lib"],"executable":null,"fresh":true}"#,
            r#"{"reason":"build-script-executed","linked_paths":["native=/t/debug/build/x/out"]}"#,
            r#"{"reason":"compiler-artifact","package_id":"p#app@1.0.0","manifest_path":"/w/app/Cargo.toml","target":{"name":"app","kind":["bin"]},"filenames":["/t/debug/app.exe","/t/debug/app.pdb"],"executable":"/t/debug/app.exe","fresh":false}"#,
            r#"{"reason":"compiler-artifact","package_id":"p#app@1.0.0","manifest_path":"/w/app/Cargo.toml","target":{"name":"other","kind":["bin"]},"filenames":["/t/debug/other.exe"],"executable":"/t/debug/other.exe","fresh":false}"#,
            r#"{"reason":"build-finished","success":true}"#,
        ];
        for l in lines {
            assert!(collect(l, Some("app"), &mut a));
        }
        assert!(!collect("plain text", None, &mut a));
        let (_, _, exe, files) = a.exe.unwrap();
        assert_eq!(exe, PathBuf::from("/t/debug/app.exe"));
        assert_eq!(files.len(), 2);
        assert_eq!(a.libs, [PathBuf::from("/t/debug/deps/dy.dll")]);
        assert_eq!(a.linked, [PathBuf::from("/t/debug/build/x/out")]);
    }

    #[test]
    fn sync_copies_only_changes() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("app.exe"), b"exe").unwrap();
        fs::write(src.join("dep.dll"), b"dll").unwrap();
        let run = d.path().join("r0");
        let files = vec![src.join("app.exe"), src.join("dep.dll")];
        assert_eq!(sync_into(&run, &files).unwrap(), 6);
        assert_eq!(sync_into(&run, &files).unwrap(), 0);
        fs::write(run.join("stale.dll"), b"x").unwrap();
        sync_into(&run, &files[..1]).unwrap();
        assert!(!run.join("stale.dll").exists());
        assert!(!run.join("dep.dll").exists());
        assert!(run.join("app.exe").exists());
    }
}
