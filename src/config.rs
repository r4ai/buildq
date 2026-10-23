//! Effective configuration: defaults < config file < environment.
//!
//! The config file is `$BUILDQ_CONFIG`, else `<queue dir>/config.toml`, so one
//! shared directory (writable by every sandbox) carries both the queue and its
//! settings.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::util::parse_size;

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    /// Concurrent build-class jobs on this machine (default 2).
    pub slots: Option<usize>,
    /// Soft per-worktree cap within each class (default ceil(slots / 2), 0 disables).
    pub max_per_worktree: Option<usize>,
    /// Concurrent app runs (the run phase of `cargo run`), default 2.
    pub run_slots: Option<usize>,
    /// Slots for other named classes, e.g. `[classes] gpu = 1`.
    pub classes: BTreeMap<String, usize>,
    /// Queue poll interval in milliseconds (default 250).
    pub poll_ms: Option<u64>,
    /// Kill the whole process tree of a job when buildq dies (Windows job object).
    pub kill_tree: Option<bool>,
    pub cache: CacheFile,
    pub cargo: CargoFile,
    pub hosts: BTreeMap<String, HostConfig>,
    /// Hosts considered by `--host auto` (default: all configured hosts + local).
    pub auto_hosts: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct CacheFile {
    /// Pool root (default `<queue dir>/cache`).
    pub dir: Option<PathBuf>,
    /// Share target dirs between worktrees (default true).
    pub pool: Option<bool>,
    /// Also pool jobs whose explicit `--target-dir`/`CARGO_TARGET_DIR` lies
    /// inside the worktree (default false: such jobs build there, outside the budget).
    pub pool_explicit: Option<bool>,
    /// Members per pool (default = slots + 1).
    pub max_members: Option<usize>,
    /// Upper bound for everything under the pool root, e.g. "60G".
    pub budget: Option<String>,
    /// Keep at least this much free on the volume, e.g. "25G".
    pub min_free: Option<String>,
    /// Delete incremental crate dirs not touched for this many days (default 3).
    pub incremental_days: Option<u64>,
    /// Delete superseded artifact variants older than this many days (default 14).
    pub stale_unit_days: Option<u64>,
    /// Delete whole pools (workspace+toolchain) unused for this many days (default 21).
    pub pool_idle_days: Option<u64>,
    /// Run `gc --auto` in the background after jobs (default true).
    pub gc_after_job: Option<bool>,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct CargoFile {
    /// Path of the real cargo (default: next `cargo` on PATH after the shim).
    pub real: Option<PathBuf>,
    /// "none" (default: disable sccache etc. per job), "keep", or a wrapper path.
    pub rustc_wrapper: Option<String>,
    /// `CARGO_BUILD_JOBS` per job (default: cores / slots).
    pub jobs: Option<usize>,
    /// Also cap `RUST_TEST_THREADS` at the per-job jobs value (default true).
    pub limit_test_threads: Option<bool>,
    /// Subcommands that go through the queue (replaces the default list).
    pub heavy: Option<Vec<String>>,
    /// Extra heavy subcommands on top of the default list.
    pub extra_heavy: Vec<String>,
    /// Copy top-level executables of a pooled build back into `<worktree>/target/<profile>/`.
    pub mirror_bins: Option<bool>,
    /// Copy `.pdb` files with run binaries (default true; backtraces need them).
    pub copy_pdb: Option<bool>,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct HostConfig {
    /// ssh destination (an alias from ~/.ssh/config); defaults to the table name.
    pub ssh: Option<String>,
    /// Extra ssh options, e.g. ["-p", "2222"].
    pub ssh_args: Vec<String>,
    /// Concurrent jobs this machine accepts from here (default 2).
    pub slots: Option<usize>,
    /// Remote root, relative to the remote home unless absolute (default ".buildq").
    pub root: Option<String>,
    /// Remote buildq binary; when set, jobs also queue (and pool) on the remote side.
    pub buildq: Option<String>,
    /// Environment for remote jobs.
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WrapperMode {
    /// Set `RUSTC_WRAPPER=""`: overrides `build.rustc-wrapper` from any config file.
    Disable,
    /// Leave whatever cargo config says.
    Keep,
    Path(String),
}

#[derive(Debug, Clone)]
pub struct Config {
    pub queue_dir: PathBuf,
    pub config_path: PathBuf,
    pub cache_dir: PathBuf,
    pub slots: usize,
    pub max_per_worktree: usize,
    pub run_slots: usize,
    pub classes: BTreeMap<String, usize>,
    pub poll_ms: u64,
    pub kill_tree: bool,
    pub pool: bool,
    pub pool_explicit: bool,
    pub max_members: usize,
    pub budget: u64,
    pub min_free: u64,
    pub incremental_days: u64,
    pub stale_unit_days: u64,
    pub pool_idle_days: u64,
    pub gc_after_job: bool,
    pub real_cargo: Option<PathBuf>,
    pub wrapper: WrapperMode,
    pub jobs: usize,
    pub limit_test_threads: bool,
    pub heavy: Vec<String>,
    pub mirror_bins: bool,
    pub copy_pdb: bool,
    pub hosts: BTreeMap<String, HostConfig>,
    pub auto_hosts: Option<Vec<String>>,
}

pub const DEFAULT_HEAVY: &[&str] = &[
    "build", "check", "test", "run", "clippy", "bench", "doc", "rustc", "rustdoc", "fix",
    "install", "nextest", "llvm-cov", "miri", "careful", "hack",
];

fn default_queue_dir() -> PathBuf {
    if cfg!(windows) {
        if let Some(d) = env::var_os("LOCALAPPDATA") {
            return PathBuf::from(d).join("buildq");
        }
    } else {
        if let Some(d) = env::var_os("XDG_STATE_HOME") {
            return PathBuf::from(d).join("buildq");
        }
        if let Some(h) = env::var_os("HOME") {
            return PathBuf::from(h).join(".local/state/buildq");
        }
    }
    env::temp_dir().join("buildq")
}

fn env_nonempty(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn env_bool(name: &str) -> Option<bool> {
    env_nonempty(name).map(|v| !matches!(v.trim(), "0" | "false" | "no" | "off"))
}

impl Config {
    /// Loads from the process environment.
    pub fn load() -> Result<Config, String> {
        let queue_dir = env_nonempty("BUILDQ_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(default_queue_dir);
        let config_path = env_nonempty("BUILDQ_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| queue_dir.join("config.toml"));
        let file = match fs::read_to_string(&config_path) {
            Ok(s) => toml::from_str::<FileConfig>(&s)
                .map_err(|e| format!("{}: {e}", config_path.display()))?,
            Err(_) => FileConfig::default(),
        };
        Ok(Self::from_parts(queue_dir, config_path, file))
    }

    pub fn from_parts(queue_dir: PathBuf, config_path: PathBuf, f: FileConfig) -> Config {
        let slots = env_nonempty("BUILDQ_SLOTS")
            .and_then(|s| s.parse().ok())
            .or(f.slots)
            .unwrap_or(2)
            .max(1);
        let cache_dir = env_nonempty("BUILDQ_CACHE")
            .map(PathBuf::from)
            .or(f.cache.dir.clone())
            .unwrap_or_else(|| queue_dir.join("cache"));
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let wrapper = match f.cargo.rustc_wrapper.as_deref() {
            None | Some("none") | Some("") => WrapperMode::Disable,
            Some("keep") => WrapperMode::Keep,
            Some(p) => WrapperMode::Path(p.to_string()),
        };
        let mut heavy: Vec<String> = match &f.cargo.heavy {
            Some(h) => h.clone(),
            None => DEFAULT_HEAVY.iter().map(|s| s.to_string()).collect(),
        };
        heavy.extend(f.cargo.extra_heavy.iter().cloned());
        let budget = env_nonempty("BUILDQ_BUDGET")
            .or(f.cache.budget.clone())
            .and_then(|s| parse_size(&s))
            .unwrap_or(60 << 30);
        let min_free = f
            .cache
            .min_free
            .as_deref()
            .and_then(parse_size)
            .unwrap_or(20 << 30);
        Config {
            config_path,
            cache_dir,
            slots,
            max_per_worktree: env_nonempty("BUILDQ_MAX_PER_WORKTREE")
                .and_then(|s| s.parse().ok())
                .or(f.max_per_worktree)
                .unwrap_or(slots.div_ceil(2)),
            run_slots: f.run_slots.unwrap_or(2).max(1),
            classes: f.classes,
            poll_ms: f.poll_ms.unwrap_or(250).max(10),
            kill_tree: f.kill_tree.unwrap_or(true),
            pool: env_bool("BUILDQ_POOL").or(f.cache.pool).unwrap_or(true),
            pool_explicit: env_bool("BUILDQ_POOL_EXPLICIT")
                .or(f.cache.pool_explicit)
                .unwrap_or(false),
            max_members: f.cache.max_members.unwrap_or(slots + 1).max(slots),
            budget,
            min_free,
            incremental_days: f.cache.incremental_days.unwrap_or(3),
            stale_unit_days: f.cache.stale_unit_days.unwrap_or(14),
            pool_idle_days: f.cache.pool_idle_days.unwrap_or(21),
            gc_after_job: env_bool("BUILDQ_GC")
                .or(f.cache.gc_after_job)
                .unwrap_or(true),
            real_cargo: env_nonempty("BUILDQ_REAL_CARGO")
                .map(PathBuf::from)
                .or(f.cargo.real),
            wrapper,
            jobs: f.cargo.jobs.unwrap_or((cores / slots).max(1)),
            limit_test_threads: f.cargo.limit_test_threads.unwrap_or(true),
            heavy,
            mirror_bins: f.cargo.mirror_bins.unwrap_or(false),
            copy_pdb: f.cargo.copy_pdb.unwrap_or(true),
            hosts: f.hosts,
            auto_hosts: f.auto_hosts,
            queue_dir,
        }
    }

    /// Slots for a job class.
    pub fn class_slots(&self, class: &str) -> usize {
        if let Some(n) = self.classes.get(class) {
            return (*n).max(1);
        }
        if let Some(host) = class.strip_prefix("host-")
            && let Some(h) = self.hosts.get(host)
        {
            return h.slots.unwrap_or(2).max(1);
        }
        match class {
            "run" => self.run_slots,
            _ => self.slots,
        }
    }

    /// All classes worth showing in `status`.
    pub fn known_classes(&self) -> Vec<String> {
        let mut v = vec!["build".to_string(), "run".to_string()];
        v.extend(self.classes.keys().cloned());
        v.extend(self.hosts.keys().map(|h| format!("host-{h}")));
        v
    }

    pub fn is_heavy(&self, sub: &str) -> bool {
        self.heavy.iter().any(|h| h == sub)
    }

    pub fn trash_dir(&self) -> PathBuf {
        self.cache_dir.join("trash")
    }
}

/// Used by tests and `buildq config`.
pub fn describe(cfg: &Config) -> String {
    let mut s = String::new();
    let p = |s: &mut String, k: &str, v: String| s.push_str(&format!("{k:<18} {v}\n"));
    p(&mut s, "queue dir", show(&cfg.queue_dir));
    p(&mut s, "config file", show(&cfg.config_path));
    p(&mut s, "cache dir", show(&cfg.cache_dir));
    p(
        &mut s,
        "slots",
        format!("build {} / run {}", cfg.slots, cfg.run_slots),
    );
    p(&mut s, "max per worktree", cfg.max_per_worktree.to_string());
    p(&mut s, "jobs per build", cfg.jobs.to_string());
    p(
        &mut s,
        "pool",
        format!(
            "{} (max {} members{})",
            cfg.pool,
            cfg.max_members,
            if cfg.pool_explicit {
                ", explicit target dirs in the worktree too"
            } else {
                ""
            }
        ),
    );
    p(
        &mut s,
        "budget",
        format!(
            "{} (keep {} free)",
            crate::util::human_bytes(cfg.budget),
            crate::util::human_bytes(cfg.min_free)
        ),
    );
    p(&mut s, "rustc wrapper", format!("{:?}", cfg.wrapper));
    p(
        &mut s,
        "hosts",
        cfg.hosts.keys().cloned().collect::<Vec<_>>().join(", "),
    );
    s
}

fn show(p: &Path) -> String {
    p.display().to_string()
}
