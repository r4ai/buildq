//! Disk accounting and garbage collection for the pool root.
//!
//! Cheapest first:
//! 1. leftovers in `trash/`;
//! 2. inside every idle member: incremental sessions other than the newest per
//!    crate, incremental crate dirs untouched for `incremental_days`, and
//!    superseded artifact variants (same package, other hash) older than
//!    `stale_unit_days` (cargo-sweep style);
//!    then, at most hourly for automatic runs, identical files of idle
//!    members are shared as reflinks (`dedupe.rs`);
//! 3. whole pools unused for `pool_idle_days` (old toolchains, dead repos);
//! 4. idle run dirs older than two days;
//! 5. while over budget: idle run copies, then whole members, each LRU first.
//!
//! The budget is `min(budget, used + free - min_free)`, so a disk that fills
//! up for other reasons shrinks the pool too. Busy members are never touched.
//! Explicit target dirs that jobs used outside the pool (`external.rs`) are
//! reported next to the budget but never pruned.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::{CacheAccounting, Config};
use crate::dedupe;
use crate::external::{self, ExternalDir};
use crate::lock::{self, LockFile};
use crate::pool::{self, MemberMeta, PoolInfo};
use crate::util::{self, human_bytes, newest_mtime, now_ms, read_json, write_json};

const DAY_MS: u64 = 86_400_000;
/// Estimates are shared by jobs and inspection processes, not process-local.
/// A bounded lifetime also catches in-place writes (directory mtimes do not).
const SIZE_CACHE_MS: u64 = 60_000;
/// Automatic runs read files for deduplication at most this often.
const DEDUPE_INTERVAL_MS: u64 = 3_600_000;

#[derive(Debug, Clone, Serialize)]
pub struct MemberEntry {
    pub id: usize,
    pub dir: PathBuf,
    pub busy: bool,
    pub size: u64,
    /// Bytes of `size` that another member also holds (see `dedupe`).
    pub shared: u64,
    pub meta: MemberMeta,
}

#[derive(Debug, Clone, Serialize)]
pub struct PoolEntry {
    pub name: String,
    pub dir: PathBuf,
    pub info: PoolInfo,
    pub members: Vec<MemberEntry>,
    /// Copies that share data blocks with a copy counted elsewhere.
    pub shared: u64,
}

impl PoolEntry {
    pub fn size(&self) -> u64 {
        self.members
            .iter()
            .map(|m| m.size)
            .sum::<u64>()
            .saturating_sub(self.shared)
    }
    pub fn last_used(&self) -> u64 {
        self.members
            .iter()
            .map(|m| m.meta.last_used_ms)
            .max()
            .unwrap_or(self.info.last_used_ms)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RunEntry {
    pub dir: PathBuf,
    pub busy: bool,
    pub size: u64,
    pub last_used_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Usage {
    pub pools: Vec<PoolEntry>,
    pub runs: Vec<RunEntry>,
    /// Pools, run dirs, and pending trash: what the budget governs.
    pub total: u64,
    pub trash_bytes: u64,
    pub budget: u64,
    pub effective_budget: u64,
    pub accounting: CacheAccounting,
    pub over_budget: bool,
    pub over_budget_bytes: u64,
    pub over_budget_reasons: Vec<String>,
    pub free_disk: Option<u64>,
    /// Explicit target dirs jobs used outside the pool (filled by `scan_all`).
    pub external: Vec<ExternalDir>,
    pub external_total: u64,
}

impl Usage {
    fn budget_state(&mut self) {
        self.over_budget_bytes = self.total.saturating_sub(self.effective_budget);
        self.over_budget = self.over_budget_bytes > 0;
        self.over_budget_reasons.clear();
        if !self.over_budget {
            return;
        }
        if self.pools.iter().flat_map(|p| &p.members).any(|m| m.busy)
            || self.runs.iter().any(|r| r.busy)
        {
            self.over_budget_reasons
                .push("busy artifacts are protected; retry GC after jobs finish".into());
        }
        if self.trash_bytes > 0 {
            self.over_budget_reasons.push(
                "pending trash still consumes space; resolve deletion errors and retry GC".into(),
            );
        }
        if self.effective_budget < self.budget {
            self.over_budget_reasons
                .push("the volume free-space reserve reduces the effective budget".into());
        }
        if self.over_budget_reasons.is_empty() {
            self.over_budget_reasons
                .push("idle artifacts await GC or could not be reclaimed".into());
        }
    }

    pub fn budget_warning(&self) -> Option<String> {
        self.over_budget.then(|| {
            format!(
                "cache exceeds its effective budget by {} (soft limit): {}",
                human_bytes(self.over_budget_bytes),
                self.over_budget_reasons.join("; ")
            )
        })
    }
}

/// Measure at most once per TTL while the caller owns the member lease.
/// This also seeds the first size estimate after a new member's build.
pub fn completed_member_warning(cfg: &Config, dir: &Path, meta: &mut MemberMeta) -> Option<String> {
    if (meta.size_accounting != Some(cfg.accounting)
        || meta.size_ms == 0
        || now_ms().saturating_sub(meta.size_ms) >= SIZE_CACHE_MS)
        && let Ok(size) = util::size_for(dir, cfg.accounting)
    {
        meta.size_bytes = size;
        meta.size_ms = now_ms();
        meta.size_accounting = Some(cfg.accounting);
    }
    oversized_member_warning(dir, meta.size_bytes, cfg.budget)
}

fn oversized_member_warning(dir: &Path, size: u64, budget: u64) -> Option<String> {
    (size > budget).then(|| {
        format!(
            "completed cache member {} ({}) exceeds the entire cache budget {}; \
         GC may evict it and the next build will be cold. Increase [cache].budget \
         or reduce build artifacts to retain reuse",
            dir.display(),
            human_bytes(size),
            human_bytes(budget)
        )
    })
}

fn trash_estimates_path(trash: &Path, accounting: CacheAccounting) -> PathBuf {
    trash.with_extension(format!("{}.sizes.json", accounting.name()))
}

fn trash_estimates(trash: &Path, accounting: CacheAccounting) -> BTreeMap<PathBuf, u64> {
    read_json(&trash_estimates_path(trash, accounting))
        .or_else(|| read_json(&trash.with_extension("sizes.json")))
        .unwrap_or_default()
}

pub fn runs_dir(cfg: &Config) -> PathBuf {
    cfg.cache_dir.join("runs")
}

/// `scan` plus the explicit target dirs outside the pool (slower: measures them).
pub fn scan_all(cfg: &Config, refresh: bool) -> Usage {
    let mut u = scan(cfg, refresh);
    u.external = external::scan(cfg, refresh);
    u.external_total = u.external.iter().map(|e| e.size_bytes).sum();
    u
}

/// The warning shown when pool + external target dirs no longer fit the budget.
pub fn external_warning(pool: u64, external: u64, budget: u64) -> Option<String> {
    (external > 0 && pool + external > budget).then(|| {
        format!(
            "pool {} + external target dirs {} exceed the budget {}; \
             gc cannot prune external target dirs: remove unused ones, \
             or set [cache] pool_explicit = true",
            human_bytes(pool),
            human_bytes(external),
            human_bytes(budget)
        )
    })
}

/// Reuse member size estimates for up to a minute, even across builds.
/// `refresh` bypasses this cache; real GC passes always refresh before pruning.
pub fn scan(cfg: &Config, refresh: bool) -> Usage {
    let sharing = dedupe::Sharing::load(cfg);
    let mut pools = Vec::new();
    if let Ok(rd) = fs::read_dir(cfg.cache_dir.join("pools")) {
        for e in rd.flatten() {
            let dir = e.path();
            if !dir.is_dir() {
                continue;
            }
            let info = read_json::<PoolInfo>(&dir.join("pool.json")).unwrap_or_default();
            let name = e.file_name().to_string_lossy().to_string();
            let mut members = Vec::new();
            for id in pool::member_ids(&dir) {
                let mdir = dir.join(format!("m{id}"));
                // Eviction retains lock inodes for safety. A lock-only ID is
                // not a cache member; do not republish empty size metadata.
                if !mdir.is_dir() {
                    continue;
                }
                let meta_path = dir.join(format!("m{id}.json"));
                // Keep the lock through measurement and metadata publication:
                // otherwise a finishing build can overwrite a refreshed size,
                // or this scan can overwrite the build's newer job metadata.
                let _member_lock = lock::try_lock(&dir.join(format!("m{id}.lock")))
                    .ok()
                    .flatten();
                let busy = _member_lock.is_none();
                let mut meta = read_json::<MemberMeta>(&meta_path).unwrap_or_default();
                if (refresh
                    || meta.size_accounting != Some(cfg.accounting)
                    || meta.size_ms == 0
                    || (!busy && now_ms().saturating_sub(meta.size_ms) >= SIZE_CACHE_MS))
                    && let Ok(size) = util::size_for(&mdir, cfg.accounting)
                {
                    meta.size_bytes = size;
                    meta.size_ms = now_ms();
                    meta.size_accounting = Some(cfg.accounting);
                    // Busy sizes are transient estimates; never replace the
                    // lease holder's metadata while it is updating the member.
                    if !busy {
                        let _ = write_json(&meta_path, &meta);
                    }
                }
                members.push(MemberEntry {
                    id,
                    size: meta.size_bytes,
                    shared: sharing.member_shared(&name, id),
                    dir: mdir,
                    busy,
                    meta,
                });
            }
            if members.is_empty() {
                continue;
            }
            pools.push(PoolEntry {
                shared: sharing.pool_discount(&name),
                name,
                dir,
                info,
                members,
            });
        }
    }
    let mut runs = Vec::new();
    if let Ok(rd) = fs::read_dir(runs_dir(cfg)) {
        for e in rd.flatten() {
            let dir = e.path();
            if !dir.is_dir() {
                continue;
            }
            let busy = lock::is_held(&dir.with_extension("lock"));
            runs.push(RunEntry {
                size: util::dir_size_for(&dir, cfg.accounting),
                last_used_ms: newest_mtime(&dir.with_extension("json")),
                busy,
                dir,
            });
        }
    }
    let estimates = trash_estimates(&cfg.trash_dir(), cfg.accounting);
    let trash_bytes = match fs::read_dir(cfg.trash_dir()) {
        Ok(entries) => entries
            .flatten()
            .map(|e| {
                util::size_for(&e.path(), cfg.accounting)
                    .unwrap_or_else(|_| estimates.get(&e.path()).copied().unwrap_or(0))
            })
            .sum::<u64>(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(_) => estimates.values().copied().sum(),
    };
    let total = pools.iter().map(|p| p.size()).sum::<u64>()
        + runs.iter().map(|r| r.size).sum::<u64>()
        + trash_bytes;
    let free_disk = util::free_disk_bytes(&cfg.cache_dir);
    let mut usage = Usage {
        over_budget: false,
        over_budget_bytes: 0,
        over_budget_reasons: Vec::new(),
        effective_budget: effective_budget(cfg.budget, cfg.min_free, total, free_disk),
        pools,
        runs,
        total,
        trash_bytes,
        budget: cfg.budget,
        accounting: cfg.accounting,
        free_disk,
        external: Vec::new(),
        external_total: 0,
    };
    usage.budget_state();
    usage
}

pub fn effective_budget(budget: u64, min_free: u64, used: u64, free: Option<u64>) -> u64 {
    match free {
        Some(free) => budget.min((used + free).saturating_sub(min_free)),
        None => budget,
    }
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub dry_run: bool,
    pub budget: Option<u64>,
    /// Automatic run after a job: throttled, quiet.
    pub auto: bool,
    /// Share identical files between idle members (never in a dry run).
    pub dedupe: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub actions: Vec<String>,
    pub freed: u64,
    pub before: u64,
    pub after: u64,
    pub effective_budget: u64,
    pub accounting: CacheAccounting,
    pub over_budget: bool,
    pub over_budget_bytes: u64,
    pub over_budget_reasons: Vec<String>,
    pub skipped: Option<String>,
    /// Explicit target dirs outside the pool: counted, never pruned.
    pub external: u64,
    pub warnings: Vec<String>,
    pub failed_deletions: usize,
    pub dedupe: Option<dedupe::Report>,
    #[serde(skip)]
    planned_removals: BTreeSet<PathBuf>,
}

impl Report {
    fn observe(&mut self, usage: &Usage) {
        self.after = usage.total;
        self.effective_budget = usage.effective_budget;
        self.accounting = usage.accounting;
        self.over_budget = usage.over_budget;
        self.over_budget_bytes = usage.over_budget_bytes;
        self.over_budget_reasons = usage.over_budget_reasons.clone();
        self.warnings.extend(usage.budget_warning());
    }

    fn act(&mut self, what: String, bytes: u64) {
        self.freed += bytes;
        self.actions
            .push(format!("{what} (path footprint {})", human_bytes(bytes)));
    }
}

/// Only account bytes actually removed; preserve a conservative estimate when
/// permissions also prevent measuring a failed deletion's remainder.
fn reclaim(
    path: &Path,
    bytes: u64,
    dry: bool,
    trash: &Path,
    move_aside: bool,
    report: &mut Report,
    action: String,
) -> u64 {
    if dry {
        report.planned_removals.insert(path.to_path_buf());
        report.act(action, bytes);
        return bytes;
    }
    let result = if move_aside {
        util::remove_tree_tracked(path, trash)
    } else {
        let result = if path.is_dir() {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        };
        result.map_err(|error| util::RemovalError {
            path: path.to_path_buf(),
            error,
        })
    };
    let (freed, remaining) = match result {
        Ok(()) => (bytes, None),
        Err(error) => {
            let measured = util::size_for(&error.path, report.accounting);
            let left = measured.as_ref().copied().unwrap_or(bytes);
            report.failed_deletions += 1;
            report.warnings.push(if measured.is_ok() {
                format!("could not remove {error}; {} remain", human_bytes(left))
            } else {
                format!(
                    "could not remove {error}; remaining size unavailable, retaining estimate {}",
                    human_bytes(left)
                )
            });
            (bytes.saturating_sub(left), Some((error.path, left)))
        }
    };
    if !trash.as_os_str().is_empty() {
        let metadata = trash_estimates_path(trash, report.accounting);
        let mut estimates = trash_estimates(trash, report.accounting);
        let mut changed = estimates.remove(path).is_some();
        if let Some((path, bytes)) = remaining
            && path.starts_with(trash)
        {
            estimates.insert(path, bytes);
            changed = true;
        }
        if changed {
            let _ = write_json(&metadata, &estimates);
        }
    }
    if freed > 0 {
        report.act(action, freed);
    }
    freed
}

fn recent_auto_usage(cfg: &Config) -> Option<Usage> {
    let last = fs::read_to_string(cfg.cache_dir.join("gc.stamp"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    if now_ms().saturating_sub(last) >= 10 * 60_000 {
        return None;
    }
    let usage = scan(cfg, false);
    (usage.total <= usage.effective_budget).then_some(usage)
}

/// Whether a path is gone after pruning: planned for removal in a dry run,
/// missing otherwise.
fn removed(report: &Report, dry: bool, path: &Path) -> bool {
    if dry {
        report
            .planned_removals
            .iter()
            .any(|planned| path.starts_with(planned))
    } else {
        fs::symlink_metadata(path).is_err()
    }
}

fn dedupe_due(cfg: &Config, auto: bool) -> bool {
    let last = fs::read_to_string(cfg.cache_dir.join("dedupe.stamp"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    !auto || now_ms().saturating_sub(last) >= DEDUPE_INTERVAL_MS
}

/// Advisory preflight before creating a collector process. The collector checks
/// again under gc.lock; cached measurements have the same bounded freshness.
pub fn auto_gc_needed(cfg: &Config) -> bool {
    recent_auto_usage(cfg).is_none()
}

pub fn gc(cfg: &Config, opts: &Options) -> Report {
    let mut report = Report {
        accounting: cfg.accounting,
        ..Default::default()
    };
    let mut cfg = cfg.clone();
    if let Some(b) = opts.budget {
        cfg.budget = b;
    }
    let _ = fs::create_dir_all(&cfg.cache_dir);
    let Ok(Some(_gc_lock)) = lock::try_lock(&cfg.cache_dir.join("gc.lock")) else {
        report.skipped = Some("another gc is running".into());
        report.observe(&scan(&cfg, false));
        report.before = report.after;
        return report;
    };
    let stamp = cfg.cache_dir.join("gc.stamp");
    if opts.auto
        && let Some(usage) = recent_auto_usage(&cfg)
    {
        report.skipped = Some("ran recently and within budget".into());
        report.observe(&usage);
        report.before = report.after;
        return report;
    }
    let dry = opts.dry_run;
    let now = now_ms();
    let usage = scan(&cfg, true);
    report.before = usage.total;
    for member in usage
        .pools
        .iter()
        .flat_map(|p| &p.members)
        .filter(|m| !m.busy)
    {
        report.warnings.extend(oversized_member_warning(
            &member.dir,
            member.size,
            cfg.budget,
        ));
    }

    // 1. trash
    let trash = cfg.trash_dir();
    if let Ok(rd) = fs::read_dir(&trash) {
        for e in rd.flatten() {
            let estimates = trash_estimates(&trash, cfg.accounting);
            let size = util::size_for(&e.path(), cfg.accounting)
                .unwrap_or_else(|_| estimates.get(&e.path()).copied().unwrap_or(0));
            reclaim(
                &e.path(),
                size,
                dry,
                &trash,
                false,
                &mut report,
                format!("remove trash {}", e.file_name().to_string_lossy()),
            );
        }
    }
    // 2. inside idle members
    let mut pruned = BTreeMap::new();
    // Pruning a copy that another member shares frees none of its blocks.
    let before_prune = dedupe::Sharing::load(&cfg);
    let mut pruned_holders = BTreeSet::new();
    for p in &usage.pools {
        for m in &p.members {
            let Ok(Some(_l)) = lock::try_lock(&p.dir.join(format!("m{}.lock", m.id))) else {
                continue;
            };
            let label = format!("{}/m{}", p.name, m.id);
            let freed = prune_member(&m.dir, &cfg, now, dry, &label, &mut report);
            // Keep the member's raw reduction for its own projected size.
            pruned.insert(m.dir.clone(), freed);
            let kept = before_prune.pruned_discount(
                &p.name,
                m.id,
                &|path| removed(&report, dry, path),
                &mut pruned_holders,
            );
            report.freed = report.freed.saturating_sub(kept);
            if !dry {
                let mp = p.dir.join(format!("m{}.json", m.id));
                let mut meta = read_json::<MemberMeta>(&mp).unwrap_or_else(|| m.meta.clone());
                if let Ok(size) = util::size_for(&m.dir, cfg.accounting) {
                    meta.size_bytes = size;
                    meta.size_ms = now_ms();
                    meta.size_accounting = Some(cfg.accounting);
                }
                let _ = write_json(&mp, &meta);
            }
        }
    }

    // A real run measures revoked discounts by rescanning; a dry run projects them.
    let revoked = if dry {
        before_prune.revoked_discount(&|path| removed(&report, dry, path))
    } else {
        0
    };

    // 2b. share identical files before budget decisions measure usage
    if opts.dedupe && !dry && dedupe_due(&cfg, opts.auto) {
        let r = dedupe::run(&cfg, &dedupe::Options::new(&cfg, false));
        report
            .warnings
            .extend(r.errors.iter().map(|e| format!("dedupe: {e}")));
        if r.skipped.is_none() {
            let _ = fs::write(cfg.cache_dir.join("dedupe.stamp"), now_ms().to_string());
        }
        report.dedupe = Some(r);
    }

    // 3. idle pools
    let mut usage = scan(&cfg, report.failed_deletions > 0);
    // Evicting a copy that another member shares frees none of its blocks.
    // A dry run has removed nothing yet, so it treats planned removals as gone.
    let sharing = dedupe::Sharing::load_excluding(&cfg, &|path| dry && removed(&report, dry, path));
    let mut evicted = BTreeSet::new();
    let mut removed_pools = Vec::new();
    for p in &usage.pools {
        if now.saturating_sub(p.last_used()) < cfg.pool_idle_days * DAY_MS {
            continue;
        }
        // Never rename/remove the coordination directory. A waiter may have
        // opened one of its locks before acquiring it; replacing that pathname
        // would allow independent owners of the same logical member.
        let Ok(Some(_pool_lock)) = lock::try_lock(&p.dir.join("pool.lock")) else {
            continue;
        };
        let Some((ids, _member_locks)) = lock_all(p) else {
            continue;
        };
        let last_used = ids
            .iter()
            .filter_map(|id| read_json::<MemberMeta>(&p.dir.join(format!("m{id}.json"))))
            .map(|m| m.last_used_ms)
            .chain(read_json::<PoolInfo>(&p.dir.join("pool.json")).map(|p| p.last_used_ms))
            .max()
            .unwrap_or(0);
        if now.saturating_sub(last_used) < cfg.pool_idle_days * DAY_MS {
            continue;
        }
        for id in ids {
            let dir = p.dir.join(format!("m{id}"));
            if !dir.is_dir() {
                continue;
            }
            let size = util::size_for(&dir, cfg.accounting)
                .unwrap_or_else(|_| {
                    p.members
                        .iter()
                        .find(|m| m.id == id)
                        .map(|m| m.size)
                        .unwrap_or(0)
                })
                .saturating_sub(if dry {
                    pruned.get(&dir).copied().unwrap_or(0)
                } else {
                    0
                })
                .saturating_sub(sharing.exclusive_discount(&p.name, id, &evicted));
            let failures = report.failed_deletions;
            evict_member(
                &cfg,
                &p.dir,
                id,
                size,
                dry,
                &mut report,
                format!(
                    "remove idle pool {}/m{} (unused {})",
                    p.name,
                    id,
                    util::ago(last_used)
                ),
            );
            if report.failed_deletions == failures {
                evicted.insert((p.name.clone(), id));
            }
        }
        removed_pools.push(p.dir.clone());
    }

    // 4. idle run dirs
    let mut removed_runs = Vec::new();
    for r in &usage.runs {
        if r.busy || now.saturating_sub(r.last_used_ms) < 2 * DAY_MS {
            continue;
        }
        let Ok(Some(_l)) = lock::try_lock(&r.dir.with_extension("lock")) else {
            continue;
        };
        reclaim(
            &r.dir,
            r.size,
            dry,
            &trash,
            true,
            &mut report,
            format!("remove run dir {}", r.dir.display()),
        );
        if !dry && !r.dir.exists() {
            let _ = fs::remove_file(r.dir.with_extension("json"));
        }
        removed_runs.push(r.dir.clone());
    }

    // 5. budget: discard run copies before reusable build artifacts.
    if !dry {
        usage = scan(&cfg, report.failed_deletions > 0);
    }
    let mut total = if dry {
        usage.total.saturating_sub(report.freed) + revoked
    } else {
        usage.total
    };
    // Both live artifacts and trash contribute to usage. Deleting either
    // decreases used and increases free equally in the dry-run projection.
    let budget = usage.effective_budget;
    report.effective_budget = budget;
    let mut runs: Vec<_> = usage
        .runs
        .iter()
        .filter(|r| !r.busy && !removed_runs.contains(&r.dir))
        .collect();
    runs.sort_by_key(|r| r.last_used_ms);
    for r in runs {
        if total <= budget {
            break;
        }
        let Ok(Some(_lock)) = lock::try_lock(&r.dir.with_extension("lock")) else {
            continue;
        };
        let freed = reclaim(
            &r.dir,
            r.size,
            dry,
            &trash,
            true,
            &mut report,
            format!("evict run dir {} (over budget)", r.dir.display()),
        );
        if !dry && !r.dir.exists() {
            let _ = fs::remove_file(r.dir.with_extension("json"));
        }
        // Keep the lock file: unlinking it could create two independent locks.
        total = total.saturating_sub(freed);
    }
    if total > budget {
        let mut members: Vec<(&PoolEntry, &MemberEntry)> = usage
            .pools
            .iter()
            .filter(|p| !removed_pools.contains(&p.dir))
            .flat_map(|p| p.members.iter().map(move |m| (p, m)))
            .filter(|(_, m)| !m.busy)
            .collect();
        members.sort_by_key(|(_, m)| m.meta.last_used_ms);
        for (p, m) in members {
            if total <= budget {
                break;
            }
            let lock_path = p.dir.join(format!("m{}.lock", m.id));
            let Ok(Some(l)) = lock::try_lock(&lock_path) else {
                continue;
            };
            let size = m
                .size
                .saturating_sub(if dry {
                    pruned.get(&m.dir).copied().unwrap_or(0)
                } else {
                    0
                })
                .saturating_sub(sharing.exclusive_discount(&p.name, m.id, &evicted));
            let failures = report.failed_deletions;
            let freed = evict_member(
                &cfg,
                &p.dir,
                m.id,
                size,
                dry,
                &mut report,
                format!(
                    "evict {}/m{} (last used {} ago, over budget {})",
                    p.name,
                    m.id,
                    util::ago(m.meta.last_used_ms),
                    human_bytes(budget)
                ),
            );
            // A failed deletion, even from trash, leaves shared blocks in place.
            if report.failed_deletions == failures {
                evicted.insert((p.name.clone(), m.id));
            }
            drop(l);
            // Keep the lock inode so a concurrent lease cannot acquire a
            // replacement lock while an existing handle is still held.
            total = total.saturating_sub(freed);
        }
    }
    let mut remaining = scan(&cfg, report.failed_deletions > 0);
    if dry {
        remaining.total = total;
        // Dry-run trash cleanup is projected as successful, like other actions.
        remaining.trash_bytes = 0;
        remaining.budget_state();
    }
    report.observe(&remaining);
    if !opts.auto {
        report.external = external::scan(&cfg, false)
            .iter()
            .map(|e| e.size_bytes)
            .sum();
        report
            .warnings
            .extend(external_warning(report.after, report.external, budget));
    }
    if !dry {
        let _ = fs::write(&stamp, now_ms().to_string());
    }
    report
}

fn evict_member(
    cfg: &Config,
    pool: &Path,
    id: usize,
    bytes: u64,
    dry: bool,
    report: &mut Report,
    action: String,
) -> u64 {
    let dir = pool.join(format!("m{id}"));
    let freed = reclaim(&dir, bytes, dry, &cfg.trash_dir(), true, report, action);
    if !dry && !dir.exists() {
        for extension in ["json", "manifest"] {
            let path = pool.join(format!("m{id}.{extension}"));
            if let Err(error) = fs::remove_file(&path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                report.failed_deletions += 1;
                report.warnings.push(format!(
                    "could not remove metadata {}: {error}",
                    path.display()
                ));
            }
        }
    }
    freed
}

/// Remove idle target pools of one repository (across its toolchains), or of
/// every repository when `repo_key` is `None`.
/// Retain coordination locks and never touch run copies or external targets.
pub fn clean_pools(cfg: &Config, repo_key: Option<&str>, dry: bool) -> Report {
    let mut report = Report {
        accounting: cfg.accounting,
        ..Default::default()
    };
    if let Err(error) = fs::create_dir_all(&cfg.cache_dir) {
        report.skipped = Some("cannot access cache directory".into());
        report.warnings.push(format!(
            "cannot access cache {}: {error}",
            cfg.cache_dir.display()
        ));
        return report;
    }
    let Ok(Some(_gc)) = lock::try_lock(&cfg.cache_dir.join("gc.lock")) else {
        report.skipped =
            Some("another cleanup holds the cache lock, or the lock is inaccessible; retry".into());
        report.observe(&scan(cfg, false));
        report.before = report.after;
        return report;
    };
    let usage = scan(cfg, true);
    report.before = usage.total;
    let sharing = dedupe::Sharing::load(cfg);
    let mut evicted = BTreeSet::new();
    let mut skipped = 0;
    let selected = |p: &&PoolEntry| repo_key.is_none_or(|k| p.info.repo_key == k);
    for pool in usage.pools.iter().filter(selected) {
        let Ok(Some(_pool)) = lock::try_lock(&pool.dir.join("pool.lock")) else {
            skipped += 1;
            report.warnings.push(format!(
                "{}: leasing is active or inaccessible; retry cache clean after jobs finish",
                pool.name
            ));
            continue;
        };
        let Some((ids, _members)) = lock_all(pool) else {
            skipped += 1;
            report.warnings.push(format!(
                "{}: members are busy or inaccessible; retry cache clean after jobs finish",
                pool.name
            ));
            continue;
        };
        for id in ids {
            let dir = pool.dir.join(format!("m{id}"));
            if !dir.is_dir() {
                continue;
            }
            let bytes = util::size_for(&dir, cfg.accounting)
                .unwrap_or_else(|_| {
                    pool.members
                        .iter()
                        .find(|m| m.id == id)
                        .map(|m| m.size)
                        .unwrap_or(0)
                })
                .saturating_sub(sharing.exclusive_discount(&pool.name, id, &evicted));
            let failures = report.failed_deletions;
            evict_member(
                cfg,
                &pool.dir,
                id,
                bytes,
                dry,
                &mut report,
                format!("remove member {}/m{id}", pool.name),
            );
            if report.failed_deletions == failures {
                evicted.insert((pool.name.clone(), id));
            }
        }
    }
    let mut after = scan(cfg, true);
    if dry {
        after.total = usage.total.saturating_sub(report.freed);
        after.budget_state();
    }
    report.observe(&after);
    if skipped > 0 {
        report.skipped = Some(format!("{skipped} pools are busy or inaccessible"));
    }
    report
}

fn lock_all(p: &PoolEntry) -> Option<(Vec<usize>, Vec<LockFile>)> {
    // Called while holding pool.lock: include members created since the scan.
    let ids = pool::member_ids(&p.dir);
    let mut locks = Vec::new();
    for id in &ids {
        locks.push(lock::try_lock(&p.dir.join(format!("m{id}.lock"))).ok()??);
    }
    Some((ids, locks))
}

/// Prunes incremental sessions and stale unit variants inside one member.
pub fn prune_member(
    dir: &Path,
    cfg: &Config,
    now: u64,
    dry: bool,
    label: &str,
    report: &mut Report,
) -> u64 {
    report.accounting = cfg.accounting;
    let before = util::size_for(dir, cfg.accounting).ok();
    let previous_freed = report.freed;
    // Incremental: `<profile>/incremental/<crate>-<hash>/s-<session>[.lock]`.
    for inc in find_named_dirs(dir, "incremental", 3) {
        let Ok(rd) = fs::read_dir(&inc) else { continue };
        for crate_dir in rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
            let mut sessions: Vec<(u64, PathBuf)> = fs::read_dir(&crate_dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .map(|p| (newest_mtime(&p), p))
                .collect();
            // rustc loads the newest finalized session; `-working` ones are
            // leftovers of interrupted compiles (no rustc runs: we hold the member).
            sessions.sort_by_key(|(t, p)| (!is_working(p), *t));
            let newest = sessions.iter().map(|(t, _)| *t).max().unwrap_or(0);
            let old = now.saturating_sub(newest) > cfg.incremental_days * DAY_MS;
            let victims: Vec<PathBuf> = if old {
                vec![crate_dir.clone()]
            } else {
                let keep = sessions
                    .iter()
                    .rev()
                    .find(|(_, p)| !is_working(p))
                    .map(|(_, p)| p.clone());
                sessions
                    .iter()
                    .map(|(_, p)| p.clone())
                    .filter(|p| Some(p) != keep.as_ref())
                    .collect()
            };
            for v in victims {
                let size = util::dir_size_for(&v, cfg.accounting);
                reclaim(
                    &v,
                    size,
                    dry,
                    &cfg.trash_dir(),
                    false,
                    report,
                    format!("{label}: incremental {}", rel(dir, &v)),
                );
                if !dry && !v.exists() {
                    let _ = fs::remove_file(v.with_extension("lock"));
                }
            }
        }
    }
    // Superseded variants: same package name, several hashes.
    for fp in pool::fingerprint_dirs(dir) {
        let profile_dir = fp.parent().unwrap_or(dir).to_path_buf();
        let mut groups: BTreeMap<String, Vec<(u64, String)>> = BTreeMap::new();
        for e in fs::read_dir(&fp).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some((pkg, hash)) = pool::split_unit(&name) {
                groups
                    .entry(pkg.to_string())
                    .or_default()
                    .push((newest_mtime(&e.path()), hash.to_string()));
            }
        }
        for (pkg, mut units) in groups {
            if units.len() < 2 {
                continue;
            }
            units.sort();
            let newest = units.last().unwrap().0;
            for (t, hash) in &units[..units.len() - 1] {
                if now.saturating_sub(*t) <= cfg.stale_unit_days * DAY_MS
                    || newest.saturating_sub(*t) < DAY_MS
                {
                    continue;
                }
                remove_unit_logged(
                    &profile_dir,
                    &pkg,
                    hash,
                    dry,
                    report,
                    &format!("{label}: stale {pkg}-{hash} ({} old)", util::ago(*t)),
                );
            }
        }
    }
    let after = if dry {
        util::size_excluding(dir, cfg.accounting, &report.planned_removals)
    } else {
        util::size_for(dir, cfg.accounting)
    }
    .ok();
    let freed = before
        .zip(after)
        .map(|(before, after)| before.saturating_sub(after))
        .unwrap_or(0);
    report.freed = previous_freed.saturating_add(freed);
    freed
}

/// Removes one unit's fingerprint, build dir and deps files; returns bytes.
pub fn remove_unit(profile_dir: &Path, pkg: &str, hash: &str, dry: bool) -> u64 {
    let mut report = Report::default();
    let before = util::size_for(profile_dir, report.accounting).unwrap_or(0);
    remove_unit_logged(profile_dir, pkg, hash, dry, &mut report, "remove unit");
    let after = util::size_excluding(profile_dir, report.accounting, &report.planned_removals)
        .unwrap_or(before);
    before.saturating_sub(after)
}

fn remove_unit_logged(
    profile_dir: &Path,
    pkg: &str,
    hash: &str,
    dry: bool,
    report: &mut Report,
    label: &str,
) -> u64 {
    let mut size = 0;
    let fp = profile_dir
        .join(".fingerprint")
        .join(format!("{pkg}-{hash}"));
    let build = profile_dir.join("build").join(format!("{pkg}-{hash}"));
    for d in [fp, build] {
        if d.exists() {
            size += reclaim(
                &d,
                util::dir_size_for(&d, report.accounting),
                dry,
                Path::new(""),
                false,
                report,
                format!("{label}: {}", d.display()),
            );
        }
    }
    let suffix = format!("-{hash}");
    for e in fs::read_dir(profile_dir.join("deps"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = e.file_name().to_string_lossy().to_string();
        let stem = name.split('.').next().unwrap_or("");
        if stem.ends_with(&suffix) {
            let path = e.path();
            size += reclaim(
                &path,
                util::size_for(&path, report.accounting).unwrap_or(0),
                dry,
                Path::new(""),
                false,
                report,
                format!("{label}: {}", path.display()),
            );
        }
    }
    size
}

fn is_working(p: &Path) -> bool {
    p.file_name()
        .is_some_and(|n| n.to_string_lossy().ends_with("-working"))
}

fn find_named_dirs(root: &Path, name: &str, depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(root).into_iter().flatten().flatten() {
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let n = e.file_name();
        if n == name {
            out.push(e.path());
        } else if depth > 0
            && !matches!(
                n.to_string_lossy().as_ref(),
                "deps" | "build" | ".fingerprint"
            )
        {
            out.extend(find_named_dirs(&e.path(), name, depth - 1));
        }
    }
    out
}

fn rel(base: &Path, p: &Path) -> String {
    p.strip_prefix(base).unwrap_or(p).display().to_string()
}

/// Human-readable `cache ls`.
pub fn render(u: &Usage) -> String {
    let mut s = String::new();
    if u.trash_bytes > 0 {
        s.push_str(&format!(
            "trash: {} (pending removal, included in total)\n",
            human_bytes(u.trash_bytes)
        ));
    }
    for p in &u.pools {
        s.push_str(&format!(
            "{}  {}  (rustc {}, last used {} ago{})\n",
            p.name,
            human_bytes(p.size()),
            p.info.toolchain,
            util::ago(p.last_used()),
            if p.shared > 0 {
                format!(", {} shared between members", human_bytes(p.shared))
            } else {
                String::new()
            }
        ));
        for m in &p.members {
            s.push_str(&format!(
                "  m{:<3} {:>10}  {:<5} {:>3} jobs  used {:>4} ago  by {}\n",
                m.id,
                human_bytes(m.size),
                if m.busy { "busy" } else { "idle" },
                m.meta.jobs,
                util::ago(m.meta.last_used_ms),
                m.meta.last_worktree.as_deref().unwrap_or("-"),
            ));
        }
    }
    let runs: u64 = u.runs.iter().map(|r| r.size).sum();
    if !u.runs.is_empty() {
        s.push_str(&format!(
            "run dirs: {} in {}\n",
            human_bytes(runs),
            u.runs.len()
        ));
    }
    s.push_str(&format!(
        "total {} / budget {} (effective {}), free disk {} [{} accounting]\n",
        human_bytes(u.total),
        human_bytes(u.budget),
        human_bytes(u.effective_budget),
        u.free_disk.map(human_bytes).unwrap_or_else(|| "?".into()),
        u.accounting.name()
    ));
    if let Some(warning) = u.budget_warning() {
        s.push_str(&format!("warning: {warning}\n"));
    }
    if !u.external.is_empty() {
        s.push_str(&format!(
            "external target dirs (not pooled, not pruned by gc): {}\n",
            human_bytes(u.external_total)
        ));
        for e in &u.external {
            s.push_str(&format!(
                "  {:>10}  {:>3} jobs  used {:>4} ago  {}  (from {})\n",
                human_bytes(e.size_bytes),
                e.jobs,
                util::ago(e.last_used_ms),
                e.path.display(),
                if e.last_cwd.is_empty() {
                    "-"
                } else {
                    &e.last_cwd
                },
            ));
        }
    }
    if let Some(w) = external_warning(u.total, u.external_total, u.effective_budget) {
        s.push_str(&format!("warning: {w}\n"));
    }
    s
}
