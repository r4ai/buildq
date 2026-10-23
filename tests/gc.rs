//! GC filesystem tests run separately from process-spawning tests: a fork can
//! briefly inherit a held file description before exec closes it, making an
//! otherwise idle member appear busy to a nonblocking collector.
use buildq::config::CacheAccounting;
use buildq::gc::*;
use buildq::lock;
use buildq::pool::MemberMeta;
use buildq::util::{self, now_ms, read_json, write_json};
use std::fs;
use std::path::{Path, PathBuf};

const DAY_MS: u64 = 86_400_000;
// Publicly documented estimate lifetime.
const SIZE_CACHE_MS: u64 = 60_000;

use buildq::config::{Config, FileConfig};
use std::time::{Duration, SystemTime};

fn cfg(root: &Path) -> Config {
    let mut c = Config::from_parts(
        root.join("q"),
        root.join("q/config.toml"),
        FileConfig::default(),
    );
    c.cache_dir = root.join("cache");
    c.min_free = 0;
    // Tiny byte-level fixtures exercise the portable logical policy.
    c.accounting = CacheAccounting::Logical;
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
fn eviction_preserves_open_member_and_pool_lock_identities() {
    for whole_pool in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let mut c = cfg(temp.path());
        c.pool_idle_days = if whole_pool { 0 } else { 21 };
        c.budget = if whole_pool { u64::MAX } else { 1 };
        let dir = member(&c.cache_dir, "p", 0, now_ms(), 4000);
        let pool_lock = dir.parent().unwrap().join("pool.lock");
        let member_lock = dir.with_extension("lock");
        drop(lock::lock(&pool_lock).unwrap());
        drop(lock::lock(&member_lock).unwrap());
        // Open before eviction, lock after it: deterministic stale-handle
        // interleaving that catches replacement inodes without timing.
        let handles: Vec<_> = [&pool_lock, &member_lock]
            .iter()
            .map(|p| {
                fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(p)
                    .unwrap()
            })
            .collect();
        let report = gc(&c, &Options::default());
        assert_eq!(report.failed_deletions, 0, "{report:?}");
        assert!(!dir.exists());
        for (path, handle) in [&pool_lock, &member_lock].into_iter().zip(handles) {
            handle.lock().unwrap();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let ready = barrier.clone();
            let path = path.clone();
            let probe = std::thread::spawn(move || {
                ready.wait();
                lock::try_lock(&path).unwrap().is_none()
            });
            barrier.wait();
            assert!(probe.join().unwrap(), "GC replaced a live lock identity");
        }
    }
}

#[test]
fn idle_pool_retirement_defers_to_an_active_leasing_section() {
    let temp = tempfile::tempdir().unwrap();
    let mut c = cfg(temp.path());
    c.pool_idle_days = 0;
    let dir = member(&c.cache_dir, "p", 0, now_ms(), 4000);
    let leasing = lock::lock(&dir.parent().unwrap().join("pool.lock")).unwrap();
    let report = gc(&c, &Options::default());
    assert!(dir.exists(), "{report:?}");
    drop(leasing);
    gc(&c, &Options::default());
    assert!(!dir.exists());
}

#[test]
fn a_hardlinked_member_that_fits_the_budget_keeps_its_cache() {
    let temp = tempfile::tempdir().unwrap();
    let mut c = cfg(temp.path());
    c.budget = 6000;
    let dir = member(&c.cache_dir, "p", 0, now_ms(), 4096);
    fs::hard_link(
        dir.join("debug/deps/libbig-0000000000000001.rlib"),
        dir.join("debug/app"),
    )
    .unwrap();
    let report = gc(&c, &Options::default());
    assert!(dir.exists(), "{report:?}");
    assert_eq!(report.before, 4096);
    assert_eq!(report.freed, 0);
}

#[test]
fn allocated_accounting_and_mode_changes_refresh_member_estimates() {
    let temp = tempfile::tempdir().unwrap();
    let mut c = cfg(temp.path());
    let dir = member(&c.cache_dir, "p", 0, now_ms(), 4096);
    fs::hard_link(
        dir.join("debug/deps/libbig-0000000000000001.rlib"),
        dir.join("debug/app"),
    )
    .unwrap();
    scan(&c, true);
    c.accounting = CacheAccounting::default();
    assert_eq!(c.accounting, CacheAccounting::Allocated);
    let allocated = util::size_for(&dir, c.accounting).unwrap();
    c.budget = allocated + 1;
    let usage = scan(&c, false);
    assert_eq!(usage.total, allocated);
    assert_eq!(
        usage.pools[0].members[0].meta.size_accounting,
        Some(CacheAccounting::Allocated)
    );
    let report = gc(&c, &Options::default());
    assert_eq!(report.accounting, CacheAccounting::Allocated);
    assert!(dir.exists());
    assert_eq!(report.freed, 0);
    c.accounting = CacheAccounting::Logical;
    assert_eq!(scan(&c, false).total, 4096);
}

#[test]
fn pruning_one_hardlink_does_not_claim_the_retained_inodes_bytes() {
    for accounting in [CacheAccounting::Logical, CacheAccounting::Allocated] {
        let temp = tempfile::tempdir().unwrap();
        let mut c = cfg(temp.path());
        c.accounting = accounting;
        let dir = member(&c.cache_dir, "p", 0, now_ms(), 4096);
        let old = dir.join("debug/deps/libbig-0000000000000001.rlib");
        fs::hard_link(&old, dir.join("debug/libbig.rlib")).unwrap();
        file(
            &dir.join("debug/deps/libbig-2222222222222222.rlib"),
            4096,
            0,
        );
        let fp = dir.join("debug/.fingerprint/big-0000000000000001");
        file(&fp.join("lib-big"), 1, 30);
        file(
            &dir.join("debug/.fingerprint/big-2222222222222222/lib-big"),
            1,
            0,
        );
        let before = util::size_for(&dir, accounting).unwrap();
        let removable = util::size_for(&fp, accounting).unwrap();
        let preview = gc(
            &c,
            &Options {
                dry_run: true,
                ..Default::default()
            },
        );
        assert_eq!(preview.freed, removable, "{preview:?}");
        assert_eq!(preview.after, before - removable);
        let actual = gc(&c, &Options::default());
        assert_eq!((actual.freed, actual.after), (preview.freed, preview.after));
        assert!(!old.exists());
        assert!(dir.join("debug/libbig.rlib").exists());
        assert_eq!(scan(&c, true).total, actual.after);
    }
}

#[test]
fn busy_overflow_reports_why_gc_cannot_meet_the_soft_budget() {
    let temp = tempfile::tempdir().unwrap();
    let mut c = cfg(temp.path());
    c.budget = 1000;
    let dir = member(&c.cache_dir, "p", 0, now_ms(), 4000);
    let held = lock::try_lock(&c.cache_dir.join("pools/p/m0.lock"))
        .unwrap()
        .unwrap();
    let usage = scan(&c, true);
    assert!(usage.over_budget);
    assert_eq!(usage.over_budget_bytes, 3000);
    assert!(render(&usage).contains("busy artifacts are protected"));
    for dry_run in [false, true] {
        let report = gc(
            &c,
            &Options {
                dry_run,
                ..Default::default()
            },
        );
        assert!(report.over_budget);
        assert_eq!(report.over_budget_bytes, 3000);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("busy artifacts are protected"))
        );
        assert!(dir.exists());
    }
    {
        let _gc = lock::try_lock(&c.cache_dir.join("gc.lock"))
            .unwrap()
            .unwrap();
        let skipped = gc(&c, &Options::default());
        assert!(skipped.skipped.is_some());
        assert!(skipped.over_budget);
        assert_eq!(skipped.over_budget_bytes, 3000);
    }
    drop(held);
    let report = gc(&c, &Options::default());
    assert!(!report.over_budget);
    assert_eq!(report.over_budget_bytes, 0);
    assert!(!dir.exists());
}

#[test]
fn completed_member_warning_reuses_and_expires_the_size_estimate() {
    let temp = tempfile::tempdir().unwrap();
    let mut c = cfg(temp.path());
    c.budget = 1000;
    let dir = member(&c.cache_dir, "p", 0, now_ms(), 4000);
    let mut meta = MemberMeta::default();
    let warning = completed_member_warning(&c, &dir, &mut meta).unwrap();
    assert!(warning.contains("next build will be cold"));
    assert_eq!(meta.size_bytes, 4000);
    file(&dir.join("new-artifact"), 4000, 0);
    completed_member_warning(&c, &dir, &mut meta);
    assert_eq!(meta.size_bytes, 4000, "warm jobs must reuse the estimate");
    meta.size_ms = now_ms() - SIZE_CACHE_MS - 1;
    completed_member_warning(&c, &dir, &mut meta);
    assert_eq!(meta.size_bytes, 8000);
    c.budget = 9000;
    assert!(completed_member_warning(&c, &dir, &mut meta).is_none());
}

#[test]
fn size_estimates_survive_builds_and_expire() {
    let dir = tempfile::tempdir().unwrap();
    let c = cfg(dir.path());
    let md = member(&c.cache_dir, "p", 0, now_ms(), 100);
    assert_eq!(scan(&c, false).total, 100);
    let mp = md.with_extension("json");
    let mut meta = read_json::<MemberMeta>(&mp).unwrap();
    let measured = meta.size_ms;
    // A completed build updates usage but should not force another walk.
    meta.last_used_ms = measured + 1;
    meta.jobs += 1;
    write_json(&mp, &meta).unwrap();
    file(&md.join("debug/new/nested/artifact"), 200, 0);
    assert_eq!(scan(&c, false).total, 100);
    assert_eq!(read_json::<MemberMeta>(&mp).unwrap().size_ms, measured);
    meta.size_ms = now_ms() - SIZE_CACHE_MS - 1;
    write_json(&mp, &meta).unwrap();
    assert_eq!(scan(&c, false).total, 300);
    // In-place growth and removal need no parent directory mtime change.
    file(&md.join("debug/new/nested/artifact"), 400, 0);
    assert_eq!(scan(&c, true).total, 500);
    fs::remove_dir_all(md.join("debug/new")).unwrap();
    assert_eq!(scan(&c, true).total, 100);
}

#[test]
fn scans_never_publish_metadata_for_a_busy_member() {
    let dir = tempfile::tempdir().unwrap();
    let c = cfg(dir.path());
    let md = member(&c.cache_dir, "p", 0, now_ms(), 100);
    let mp = md.with_extension("json");
    let before = fs::read(&mp).unwrap();
    let _lease = lock::try_lock(&md.with_extension("lock")).unwrap().unwrap();
    for refresh in [false, true] {
        let usage = scan(&c, refresh);
        assert!(usage.pools[0].members[0].busy);
        assert_eq!(usage.total, 100);
        assert_eq!(fs::read(&mp).unwrap(), before);
    }
    drop(_lease);
    scan(&c, false);
    assert!(read_json::<MemberMeta>(&mp).unwrap().size_ms > 0);
}

#[test]
fn actual_auto_gc_refreshes_both_under_and_overestimates() {
    for (cached, actual) in [(100, 4000), (4000, 100)] {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cfg(dir.path());
        c.budget = 1000;
        let md = member(&c.cache_dir, "p", 0, now_ms(), cached);
        assert_eq!(scan(&c, false).total, cached as u64);
        file(
            &md.join("debug/deps/libbig-0000000000000001.rlib"),
            actual,
            0,
        );
        assert_eq!(scan(&c, false).total, cached as u64);
        let report = gc(
            &c,
            &Options {
                auto: true,
                ..Default::default()
            },
        );
        assert!(report.skipped.is_none());
        assert_eq!(report.before, actual as u64);
        assert_eq!(md.exists(), actual <= 1000);
        assert_eq!(report.after, if actual <= 1000 { actual as u64 } else { 0 });
    }
}

#[test]
fn auto_gc_detects_growth_when_estimate_expires_despite_recent_gc() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = cfg(dir.path());
    c.budget = 1000;
    let md = member(&c.cache_dir, "p", 0, now_ms(), 100);
    scan(&c, false);
    fs::write(c.cache_dir.join("gc.stamp"), now_ms().to_string()).unwrap();
    file(&md.join("debug/new-artifact"), 4000, 0);
    let opts = Options {
        auto: true,
        ..Default::default()
    };
    // Auto-GC's cheap throttle may briefly defer enforcement while the
    // estimate is young, but expiration must discover even nested growth.
    assert!(gc(&c, &opts).skipped.is_some());
    let mp = md.with_extension("json");
    let mut meta = read_json::<MemberMeta>(&mp).unwrap();
    meta.size_ms = now_ms() - SIZE_CACHE_MS - 1;
    write_json(&mp, &meta).unwrap();
    let report = gc(&c, &opts);
    assert!(report.skipped.is_none());
    assert_eq!(report.before, 4100);
    assert_eq!(report.after, 0);
    assert!(!md.exists());
}

#[test]
fn dry_run_trash_does_not_hide_budget_eviction() {
    for reserve in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cfg(dir.path());
        c.budget = 1024 * 1024;
        let md = member(&c.cache_dir, "p", 0, now_ms(), 2 * 1024 * 1024);
        file(&c.trash_dir().join("old/artifact"), 4 * 1024 * 1024, 0);
        if reserve {
            c.min_free = util::free_disk_bytes(&c.cache_dir).unwrap() + 2 * 1024 * 1024;
        }
        let preview = gc(
            &c,
            &Options {
                dry_run: true,
                ..Default::default()
            },
        );
        assert!(md.exists());
        assert!(
            preview.actions.iter().any(|a| a.contains("evict p/m0")),
            "{preview:?}"
        );
        let actual = gc(&c, &Options::default());
        assert_eq!(preview.freed, actual.freed);
        assert_eq!(preview.after, actual.after);
        assert_eq!(preview.effective_budget, actual.effective_budget);
        assert!(!md.exists());
    }
}

#[test]
fn dry_run_does_not_count_pruned_artifacts_again_when_evicting() {
    for idle_pool in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cfg(dir.path());
        c.budget = 2000;
        let used = now_ms().saturating_sub(if idle_pool { 30 * DAY_MS } else { 1000 });
        let old = member(&c.cache_dir, "old", 0, used, 4000);
        member(&c.cache_dir, "recent", 0, now_ms(), 4000);
        file(&old.join("debug/incremental/core/s-old/a"), 1000, 1);
        file(&old.join("debug/incremental/core/s-new/a"), 1000, 0);
        let preview = gc(
            &c,
            &Options {
                dry_run: true,
                ..Default::default()
            },
        );
        let actual = gc(&c, &Options::default());
        assert_eq!(preview.freed, 10_000, "{preview:?}");
        assert_eq!(preview.freed, actual.freed);
        assert_eq!(preview.after, actual.after);
    }
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
