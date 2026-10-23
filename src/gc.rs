//! Disk accounting and garbage collection for the pool root.
//!
//! Cheapest first:
//! 1. leftovers in `trash/`;
//! 2. inside every idle member: incremental sessions other than the newest per
//!    crate, incremental crate dirs untouched for `incremental_days`, and
//!    superseded artifact variants (same package, other hash) older than
//!    `stale_unit_days` (cargo-sweep style);
//! 3. whole pools unused for `pool_idle_days` (old toolchains, dead repos);
//! 4. idle run dirs older than two days;
//! 5. while over budget: idle run copies, then whole members, each LRU first.
//!
//! The budget is `min(budget, used + free - min_free)`, so a disk that fills
//! up for other reasons shrinks the pool too. Busy members are never touched.
//! Explicit target dirs that jobs used outside the pool (`external.rs`) are
//! reported next to the budget but never pruned.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::Config;
use crate::external::{self, ExternalDir};
use crate::lock::{self, LockFile};
use crate::pool::{self, MemberMeta, PoolInfo};
use crate::util::{
    self, dir_size, human_bytes, newest_mtime, now_ms, read_json, remove_tree, write_json,
};

const DAY_MS: u64 = 86_400_000;

#[derive(Debug, Clone, Serialize)]
pub struct MemberEntry {
    pub id: usize,
    pub dir: PathBuf,
    pub busy: bool,
    pub size: u64,
    pub meta: MemberMeta,
}

#[derive(Debug, Clone, Serialize)]
pub struct PoolEntry {
    pub name: String,
    pub dir: PathBuf,
    pub info: PoolInfo,
    pub members: Vec<MemberEntry>,
}

impl PoolEntry {
    pub fn size(&self) -> u64 {
        self.members.iter().map(|m| m.size).sum()
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
    /// Pools and run dirs: what the budget governs.
    pub total: u64,
    pub budget: u64,
    pub effective_budget: u64,
    pub free_disk: Option<u64>,
    /// Explicit target dirs jobs used outside the pool (filled by `scan_all`).
    pub external: Vec<ExternalDir>,
    pub external_total: u64,
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

/// The warning shown when pool + explicit target dirs no longer fit the budget.
pub fn external_warning(pool: u64, external: u64, budget: u64) -> Option<String> {
    (external > 0 && pool + external > budget).then(|| {
        format!(
            "pool {} + explicit target dirs {} exceed the budget {}; \
             gc cannot prune explicit target dirs: remove unused ones, \
             or set [cache] pool_explicit = true",
            human_bytes(pool),
            human_bytes(external),
            human_bytes(budget)
        )
    })
}

/// Sizes come from member metadata unless `refresh` (or never measured).
pub fn scan(cfg: &Config, refresh: bool) -> Usage {
    let mut pools = Vec::new();
    if let Ok(rd) = fs::read_dir(cfg.cache_dir.join("pools")) {
        for e in rd.flatten() {
            let dir = e.path();
            if !dir.is_dir() {
                continue;
            }
            let info = read_json::<PoolInfo>(&dir.join("pool.json")).unwrap_or_default();
            let mut members = Vec::new();
            for id in pool::member_ids(&dir) {
                let mdir = dir.join(format!("m{id}"));
                let meta_path = dir.join(format!("m{id}.json"));
                let mut meta = read_json::<MemberMeta>(&meta_path).unwrap_or_default();
                let busy = lock::is_held(&dir.join(format!("m{id}.lock")));
                if refresh
                    || (meta.size_ms == 0 && mdir.exists())
                    || meta.size_ms < meta.last_used_ms && !busy
                {
                    meta.size_bytes = dir_size(&mdir);
                    meta.size_ms = now_ms();
                    let _ = write_json(&meta_path, &meta);
                }
                members.push(MemberEntry {
                    id,
                    size: meta.size_bytes,
                    dir: mdir,
                    busy,
                    meta,
                });
            }
            pools.push(PoolEntry {
                name: e.file_name().to_string_lossy().to_string(),
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
                size: dir_size(&dir),
                last_used_ms: newest_mtime(&dir.with_extension("json")),
                busy,
                dir,
            });
        }
    }
    let total =
        pools.iter().map(|p| p.size()).sum::<u64>() + runs.iter().map(|r| r.size).sum::<u64>();
    let free_disk = util::free_disk_bytes(&cfg.cache_dir);
    Usage {
        effective_budget: effective_budget(cfg.budget, cfg.min_free, total, free_disk),
        pools,
        runs,
        total,
        budget: cfg.budget,
        free_disk,
        external: Vec::new(),
        external_total: 0,
    }
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
    /// Background run after a job: throttled, quiet.
    pub auto: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub actions: Vec<String>,
    pub freed: u64,
    pub before: u64,
    pub after: u64,
    pub effective_budget: u64,
    pub skipped: Option<String>,
    /// Explicit target dirs outside the pool: counted, never pruned.
    pub external: u64,
    pub warnings: Vec<String>,
}

impl Report {
    fn act(&mut self, what: String, bytes: u64) {
        self.freed += bytes;
        self.actions
            .push(format!("{what} ({})", human_bytes(bytes)));
    }
}

pub fn gc(cfg: &Config, opts: &Options) -> Report {
    let mut report = Report::default();
    let _ = fs::create_dir_all(&cfg.cache_dir);
    let Ok(Some(_gc_lock)) = lock::try_lock(&cfg.cache_dir.join("gc.lock")) else {
        report.skipped = Some("another gc is running".into());
        return report;
    };
    let stamp = cfg.cache_dir.join("gc.stamp");
    let mut cfg = cfg.clone();
    if let Some(b) = opts.budget {
        cfg.budget = b;
    }
    if opts.auto {
        let last = fs::read_to_string(&stamp)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let usage = scan(&cfg, false);
        if now_ms() - last < 10 * 60_000 && usage.total <= usage.effective_budget {
            report.skipped = Some("ran recently and within budget".into());
            return report;
        }
    }
    let dry = opts.dry_run;
    let now = now_ms();

    // 1. trash
    let trash = cfg.trash_dir();
    if let Ok(rd) = fs::read_dir(&trash) {
        for e in rd.flatten() {
            let size = dir_size(&e.path());
            if !dry {
                let _ = fs::remove_dir_all(e.path());
            }
            report.act(
                format!("remove trash {}", e.file_name().to_string_lossy()),
                size,
            );
        }
    }

    let usage = scan(&cfg, !opts.auto);
    report.before = usage.total;

    // 2. inside idle members
    for p in &usage.pools {
        for m in &p.members {
            let Ok(Some(_l)) = lock::try_lock(&p.dir.join(format!("m{}.lock", m.id))) else {
                continue;
            };
            let label = format!("{}/m{}", p.name, m.id);
            let freed = prune_member(&m.dir, &cfg, now, dry, &label, &mut report);
            if freed > 0 && !dry {
                let mp = p.dir.join(format!("m{}.json", m.id));
                let mut meta = m.meta.clone();
                meta.size_bytes = meta.size_bytes.saturating_sub(freed);
                let _ = write_json(&mp, &meta);
            }
        }
    }

    // 3. idle pools
    let mut usage = scan(&cfg, false);
    for p in &usage.pools {
        if now.saturating_sub(p.last_used()) < cfg.pool_idle_days * DAY_MS {
            continue;
        }
        let Some(locks) = lock_all(p) else { continue };
        let size = p.size();
        if !dry {
            let _ = remove_tree(&p.dir, &trash);
        }
        drop(locks);
        report.act(
            format!(
                "remove idle pool {} (unused {})",
                p.name,
                util::ago(p.last_used())
            ),
            size,
        );
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
        if !dry {
            if remove_tree(&r.dir, &trash).is_err() {
                continue;
            }
            let _ = fs::remove_file(r.dir.with_extension("json"));
        }
        report.act(format!("remove run dir {}", r.dir.display()), r.size);
        removed_runs.push(r.dir.clone());
    }

    // 5. budget: discard run copies before reusable build artifacts.
    if !dry {
        usage = scan(&cfg, false);
    }
    let mut total = usage
        .total
        .saturating_sub(if dry { report.freed } else { 0 });
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
        if !dry {
            if remove_tree(&r.dir, &trash).is_err() {
                continue;
            }
            let _ = fs::remove_file(r.dir.with_extension("json"));
        }
        // Keep the lock file: unlinking it could create two independent locks.
        total = total.saturating_sub(r.size);
        report.act(
            format!("evict run dir {} (over budget)", r.dir.display()),
            r.size,
        );
    }
    if total > budget {
        let mut members: Vec<(&PoolEntry, &MemberEntry)> = usage
            .pools
            .iter()
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
            if !dry {
                let _ = remove_tree(&m.dir, &trash);
                let _ = fs::remove_file(p.dir.join(format!("m{}.json", m.id)));
                let _ = fs::remove_file(p.dir.join(format!("m{}.manifest", m.id)));
            }
            drop(l);
            if !dry {
                let _ = fs::remove_file(&lock_path);
            }
            total = total.saturating_sub(m.size);
            report.act(
                format!(
                    "evict {}/m{} (last used {} ago, over budget {})",
                    p.name,
                    m.id,
                    util::ago(m.meta.last_used_ms),
                    human_bytes(budget)
                ),
                m.size,
            );
        }
    }
    report.after = total;
    if !opts.auto {
        report.external = external::scan(&cfg, false)
            .iter()
            .map(|e| e.size_bytes)
            .sum();
        report
            .warnings
            .extend(external_warning(total, report.external, budget));
    }
    if !dry {
        let _ = fs::write(&stamp, now_ms().to_string());
    }
    report
}

fn lock_all(p: &PoolEntry) -> Option<Vec<LockFile>> {
    let mut v = Vec::new();
    for m in &p.members {
        v.push(lock::try_lock(&p.dir.join(format!("m{}.lock", m.id))).ok()??);
    }
    Some(v)
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
    let mut freed = 0;
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
                let size = dir_size(&v);
                if !dry {
                    let _ = fs::remove_dir_all(&v);
                    let _ = fs::remove_file(v.with_extension("lock"));
                }
                freed += size;
                if size > 0 {
                    report.act(format!("{label}: incremental {}", rel(dir, &v)), size);
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
                let size = remove_unit(&profile_dir, &pkg, hash, dry);
                freed += size;
                report.act(
                    format!("{label}: stale {pkg}-{hash} ({} old)", util::ago(*t)),
                    size,
                );
            }
        }
    }
    freed
}

/// Removes one unit's fingerprint, build dir and deps files; returns bytes.
pub fn remove_unit(profile_dir: &Path, pkg: &str, hash: &str, dry: bool) -> u64 {
    let mut size = 0;
    let fp = profile_dir
        .join(".fingerprint")
        .join(format!("{pkg}-{hash}"));
    let build = profile_dir.join("build").join(format!("{pkg}-{hash}"));
    for d in [fp, build] {
        if d.exists() {
            size += dir_size(&d);
            if !dry {
                let _ = fs::remove_dir_all(&d);
            }
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
            size += e.metadata().map(|m| m.len()).unwrap_or(0);
            if !dry {
                let p = e.path();
                if p.is_dir() {
                    let _ = fs::remove_dir_all(p);
                } else {
                    let _ = fs::remove_file(p);
                }
            }
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
    for p in &u.pools {
        s.push_str(&format!(
            "{}  {}  (rustc {}, last used {} ago)\n",
            p.name,
            human_bytes(p.size()),
            p.info.toolchain,
            util::ago(p.last_used())
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
        "total {} / budget {} (effective {}), free disk {}\n",
        human_bytes(u.total),
        human_bytes(u.budget),
        human_bytes(u.effective_budget),
        u.free_disk.map(human_bytes).unwrap_or_else(|| "?".into())
    ));
    if !u.external.is_empty() {
        s.push_str(&format!(
            "explicit target dirs (not pooled, not pruned by gc): {}\n",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, FileConfig};
    use std::time::{Duration, SystemTime};

    fn cfg(root: &Path) -> Config {
        let mut c = Config::from_parts(
            root.join("q"),
            root.join("q/config.toml"),
            FileConfig::default(),
        );
        c.cache_dir = root.join("cache");
        c.min_free = 0;
        c
    }

    fn file(p: &Path, bytes: usize, age_days: u64) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, vec![0u8; bytes]).unwrap();
        let t = SystemTime::now() - Duration::from_secs(age_days * 86_400);
        fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    fn member(cache: &Path, pool: &str, id: usize, used_ms: u64, bytes: usize) -> PathBuf {
        let pd = cache.join("pools").join(pool);
        let md = pd.join(format!("m{id}"));
        file(
            &md.join("debug/deps/libbig-0000000000000001.rlib"),
            bytes,
            0,
        );
        write_json(
            &pd.join(format!("m{id}.json")),
            &MemberMeta {
                id,
                last_used_ms: used_ms,
                ..Default::default()
            },
        )
        .unwrap();
        md
    }

    #[test]
    fn prunes_incremental_and_stale_variants() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        let md = member(&c.cache_dir, "p", 0, now_ms(), 10);
        let d = md.join("debug");
        // Two sessions: only the newest survives. An old crate dir goes entirely.
        file(&d.join("incremental/core-aaa/s-old/x.bin"), 100, 1);
        file(&d.join("incremental/core-aaa/s-new/x.bin"), 100, 0);
        file(
            &d.join("incremental/core-aaa/s-newer-working/x.bin"),
            100,
            0,
        );
        file(&d.join("incremental/gone-bbb/s-1/x.bin"), 100, 10);
        // serde: an old variant and a current one.
        file(
            &d.join(".fingerprint/serde-1111111111111111/lib-serde"),
            1,
            30,
        );
        file(&d.join("deps/libserde-1111111111111111.rlib"), 500, 30);
        file(
            &d.join(".fingerprint/serde-2222222222222222/lib-serde"),
            1,
            0,
        );
        file(&d.join("deps/libserde-2222222222222222.rlib"), 500, 0);
        // A single old variant is kept (still the only one in use).
        file(&d.join(".fingerprint/log-3333333333333333/lib-log"), 1, 30);

        let r = gc(&c, &Options::default());
        assert!(!d.join("incremental/core-aaa/s-old").exists());
        assert!(d.join("incremental/core-aaa/s-new").exists());
        assert!(!d.join("incremental/core-aaa/s-newer-working").exists());
        assert!(!d.join("incremental/gone-bbb").exists());
        assert!(!d.join("deps/libserde-1111111111111111.rlib").exists());
        assert!(!d.join(".fingerprint/serde-1111111111111111").exists());
        assert!(d.join("deps/libserde-2222222222222222.rlib").exists());
        assert!(d.join(".fingerprint/log-3333333333333333").exists());
        assert!(r.freed >= 700, "{r:?}");
    }

    #[test]
    fn evicts_lru_members_over_budget_and_skips_busy() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cfg(dir.path());
        let now = now_ms();
        let old = member(&c.cache_dir, "p", 0, now - 3_000_000, 4000);
        let busy = member(&c.cache_dir, "p", 1, now - 5_000_000, 4000);
        let recent = member(&c.cache_dir, "p", 2, now - 1_000, 4000);
        let _busy_lock = lock::try_lock(&c.cache_dir.join("pools/p/m1.lock"))
            .unwrap()
            .unwrap();
        c.budget = 9000;

        let dry = gc(
            &c,
            &Options {
                dry_run: true,
                ..Default::default()
            },
        );
        assert!(old.exists() && dry.actions.iter().any(|a| a.contains("evict p/m0")));

        let r = gc(&c, &Options::default());
        assert!(!old.exists(), "{r:?}");
        assert!(busy.exists(), "busy member must survive");
        assert!(recent.exists());
        assert!(r.after <= 9000);
    }

    #[test]
    fn budget_evicts_recent_idle_runs_lru_before_members() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cfg(dir.path());
        c.budget = 9000;
        let md = member(&c.cache_dir, "p", 0, now_ms(), 1000);
        let runs = runs_dir(&c);
        for (id, age) in [(0, 1), (1, 0), (2, 3)] {
            file(&runs.join(format!("r{id}/app")), 4000, 0);
            file(&runs.join(format!("r{id}.json")), 0, age);
        }
        let _busy = lock::try_lock(&runs.join("r2.lock")).unwrap().unwrap();
        let dry = gc(
            &c,
            &Options {
                dry_run: true,
                ..Default::default()
            },
        );
        assert_eq!((dry.before, dry.freed, dry.after), (13000, 4000, 9000));
        assert!(runs.join("r0/app").exists());
        let report = gc(&c, &Options::default());
        assert_eq!(
            (report.before, report.freed, report.after),
            (13000, 4000, 9000)
        );
        assert!(!runs.join("r0").exists());
        assert!(!runs.join("r0.json").exists());
        assert!(runs.join("r0.lock").exists());
        assert!(runs.join("r1/app").exists());
        assert!(runs.join("r2/app").exists());
        assert!(md.exists());
        assert_eq!(scan(&c, true).total, report.after);
    }

    #[test]
    fn budget_cannot_evict_busy_runs() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cfg(dir.path());
        c.budget = 1;
        let runs = runs_dir(&c);
        file(&runs.join("r0/app"), 4000, 0);
        file(&runs.join("r0.json"), 0, 0);
        let busy = lock::try_lock(&runs.join("r0.lock")).unwrap().unwrap();
        let report = gc(&c, &Options::default());
        assert_eq!(report.after, 4000);
        assert_eq!(report.freed, 0);
        assert!(runs.join("r0/app").exists());
        drop(busy);
        let report = gc(&c, &Options::default());
        assert_eq!(report.after, 0);
        assert!(!runs.join("r0").exists());
    }

    #[test]
    fn dry_run_does_not_count_age_evicted_runs_twice() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cfg(dir.path());
        c.budget = 1;
        let runs = runs_dir(&c);
        file(&runs.join("r0/app"), 4000, 0);
        file(&runs.join("r0.json"), 0, 3);
        file(&runs.join("r1/app"), 4000, 0);
        file(&runs.join("r1.json"), 0, 0);
        let report = gc(
            &c,
            &Options {
                dry_run: true,
                ..Default::default()
            },
        );
        assert_eq!((report.freed, report.after), (8000, 0));
        assert_eq!(report.actions.len(), 2);
        assert!(runs.join("r0/app").exists());
        assert!(runs.join("r1/app").exists());
    }

    #[test]
    fn budget_shrinks_with_free_disk() {
        assert_eq!(effective_budget(100, 10, 50, Some(1000)), 100);
        assert_eq!(effective_budget(100, 30, 50, Some(20)), 40);
        assert_eq!(effective_budget(100, 30, 0, Some(10)), 0);
        assert_eq!(effective_budget(100, 30, 50, None), 100);
    }
}
