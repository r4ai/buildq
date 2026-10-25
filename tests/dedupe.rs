//! Cross-member deduplication. Filesystem only, like `gc.rs`: no process
//! spawning, so idle members never look busy through inherited descriptors.
use buildq::config::{CacheAccounting, Config, FileConfig};
use buildq::dedupe::{self, Options};
use buildq::lock;
use std::fs;
use std::path::{Path, PathBuf};

fn cfg(root: &Path) -> Config {
    let mut c = Config::from_parts(
        root.join("q"),
        root.join("q/config.toml"),
        FileConfig::default(),
    );
    c.cache_dir = root.join("cache");
    c.min_free = 0;
    c.accounting = CacheAccounting::Logical;
    c.dedupe_min_size = 1;
    c
}

fn opts(c: &Config, dry_run: bool) -> Options {
    Options::new(c, dry_run)
}

fn put(c: &Config, pool: &str, id: usize, rel: &str, data: &[u8]) -> PathBuf {
    let p = c
        .cache_dir
        .join("pools")
        .join(pool)
        .join(format!("m{id}"))
        .join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, data).unwrap();
    p
}

/// 20,000 bytes that differ from other `byte`s only outside both
/// fingerprinted blocks.
fn mid(byte: u8) -> Vec<u8> {
    let mut data = vec![0; 20_000];
    data[10_000] = byte;
    data
}

const RLIB: &str = "debug/deps/libdep-0123456789abcdef.rlib";

#[test]
fn dry_run_counts_identical_copies_beyond_the_first_without_changes() {
    let temp = tempfile::tempdir().unwrap();
    let c = cfg(temp.path());
    let a = put(&c, "p", 0, RLIB, &[7; 3000]);
    let b = put(&c, "p", 1, RLIB, &[7; 3000]);
    put(&c, "p", 2, RLIB, &[7; 3000]);
    put(&c, "p", 0, "debug/deps/libown-1.rlib", &[1; 500]);
    put(&c, "p", 1, "debug/deps/libown-1.rlib", &[2; 500]);
    let before = (fs::metadata(&a).unwrap(), fs::metadata(&b).unwrap());

    let r = dedupe::run(&c, &opts(&c, true));
    assert!(r.errors.is_empty(), "{r:?}");
    assert_eq!((r.compared, r.identical, r.differing), (3, 2, 1));
    assert_eq!(r.identical_bytes, 6000);
    // Three copies of 3000 bytes are fingerprinted whole first.
    assert_eq!(r.read_bytes, 3 * 3000 + 2 * (3000 + 3000 + 500));

    let after = (fs::metadata(&a).unwrap(), fs::metadata(&b).unwrap());
    assert_eq!(before.0.modified().unwrap(), after.0.modified().unwrap());
    assert_eq!(before.1.modified().unwrap(), after.1.modified().unwrap());
    assert!(!c.cache_dir.join("dedupe.json").exists());
}

#[test]
fn private_state_small_files_and_other_pools_are_not_candidates() {
    let temp = tempfile::tempdir().unwrap();
    let mut c = cfg(temp.path());
    c.dedupe_min_size = 1000;
    for id in [0, 1] {
        put(
            &c,
            "p",
            id,
            "debug/incremental/dep-1/s-1/query.bin",
            &[3; 4000],
        );
        put(&c, "p", id, "debug/.fingerprint/dep-1/lib-dep", &[3; 4000]);
        put(&c, "p", id, "debug/deps/small.rmeta", &[3; 999]);
    }
    put(&c, "p", 0, RLIB, &[3; 4000]);
    put(&c, "q", 0, RLIB, &[3; 4000]);
    // Same path in two members but different lengths.
    put(&c, "p", 1, RLIB, &[3; 4001]);
    let r = dedupe::run(&c, &opts(&c, true));
    assert_eq!(r.compared, 0, "{r:?}");
    assert_eq!(r.pools, 2);
    assert_eq!((r.indexed, r.candidates), (3, 0), "{r:?}");
}

/// Metadata cost of a pass that may read nothing, which `dedupe_max_read`
/// does not bound: `BQ_INDEX_FILES=100000 cargo test --test dedupe
/// index_scale -- --ignored --nocapture`.
#[test]
#[ignore = "benchmark"]
fn index_scale() {
    let n: usize = std::env::var("BQ_INDEX_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000);
    let temp = tempfile::tempdir().unwrap();
    let mut c = cfg(temp.path());
    c.dedupe_max_read = 1;
    for id in [0, 1] {
        for i in 0..n / 2 {
            let p = put(
                &c,
                "p",
                id,
                &format!("debug/deps/libprobe-{i:016x}.rlib"),
                &[],
            );
            fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_len(64 << 10)
                .unwrap();
        }
    }
    let r = dedupe::run(&c, &opts(&c, true));
    assert!(r.read_limited, "{r:?}");
    println!("{}", dedupe::render(&r));
}

#[test]
fn busy_members_are_skipped() {
    let temp = tempfile::tempdir().unwrap();
    let c = cfg(temp.path());
    for id in [0, 1, 2] {
        put(&c, "p", id, RLIB, &[9; 2000]);
    }
    let pool = c.cache_dir.join("pools/p");
    let _busy = lock::lock(&pool.join("m1.lock")).unwrap();
    let r = dedupe::run(&c, &opts(&c, true));
    assert_eq!(r.skipped_busy, 1, "{r:?}");
    assert_eq!((r.compared, r.identical), (1, 1));
}

#[test]
fn the_read_limit_stops_the_pass() {
    let temp = tempfile::tempdir().unwrap();
    let c = cfg(temp.path());
    for id in [0, 1, 2] {
        put(&c, "p", id, RLIB, &[5; 1000]);
    }
    let mut o = opts(&c, true);
    // Fingerprints cost 3000, leaving room for one comparison.
    o.max_read = 5000;
    let r = dedupe::run(&c, &o);
    assert!(r.read_limited, "{r:?}");
    assert_eq!((r.compared, r.read_bytes), (1, 5000));
}

#[test]
fn projected_passes_skip_excluded_files() {
    let temp = tempfile::tempdir().unwrap();
    let c = cfg(temp.path());
    const STALE: &str = "debug/deps/libaaa-0123456789abcdef.rlib";
    for id in [0, 1] {
        put(&c, "p", id, STALE, &[1; 1000]);
        put(&c, "p", id, RLIB, &[2; 1000]);
    }
    let mut o = opts(&c, true);
    o.max_read = 2000;
    let (r, _) = dedupe::run_projected(&c, &o, &|p| p.ends_with(STALE));
    assert_eq!(
        (r.compared, r.identical, r.read_limited),
        (1, 1, false),
        "{r:?}"
    );
}

#[test]
fn concurrent_passes_do_not_overlap() {
    let temp = tempfile::tempdir().unwrap();
    let c = cfg(temp.path());
    put(&c, "p", 0, RLIB, &[1; 10]);
    let _running = lock::lock(&c.cache_dir.join("dedupe.lock")).unwrap();
    let r = dedupe::run(&c, &opts(&c, true));
    assert!(r.skipped.is_some());
}

#[cfg(unix)]
fn age(p: &Path, secs: u64) -> std::time::SystemTime {
    let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
    fs::File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_modified(t)
        .unwrap();
    fs::metadata(p).unwrap().modified().unwrap()
}

#[cfg(unix)]
fn used(c: &Config, id: usize, last_used_ms: u64) {
    buildq::util::write_json(
        &c.cache_dir.join(format!("pools/p/m{id}.json")),
        &buildq::pool::MemberMeta {
            id,
            last_used_ms,
            ..Default::default()
        },
    )
    .unwrap();
}

#[cfg(unix)]
fn ino(p: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(p).unwrap().ino()
}

#[cfg(unix)]
/// Runs a real pass, or returns None where the volume cannot share data
/// (for example ext4 or tmpfs on Linux CI).
fn share(c: &Config) -> Option<dedupe::Report> {
    let r = dedupe::run(c, &opts(c, false));
    if let Some(why) = &r.unsupported {
        eprintln!("skipping: {why}");
        return None;
    }
    assert!(r.errors.is_empty(), "{r:?}");
    Some(r)
}

#[cfg(unix)]
#[test]
fn sharing_keeps_content_mode_and_mtime_and_shrinks_usage() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let c = cfg(temp.path());
    let data: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
    let paths: Vec<PathBuf> = (0..3).map(|id| put(&c, "p", id, RLIB, &data)).collect();
    fs::set_permissions(&paths[1], fs::Permissions::from_mode(0o755)).unwrap();
    let mtimes: Vec<_> = paths
        .iter()
        .enumerate()
        .map(|(i, p)| age(p, 1000 * (i as u64 + 1)))
        .collect();
    let before = buildq::gc::scan(&c, true).total;

    let Some(r) = share(&c) else { return };
    assert_eq!((r.shared, r.shared_bytes), (2, 100_000), "{r:?}");
    for (i, p) in paths.iter().enumerate() {
        assert_eq!(fs::read(p).unwrap(), data);
        assert_eq!(fs::metadata(p).unwrap().modified().unwrap(), mtimes[i]);
    }
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&paths[1]), 0o755);
    assert_ne!(mode(&paths[2]), 0o755);

    let usage = buildq::gc::scan(&c, true);
    assert_eq!(usage.pools[0].shared, 100_000);
    assert_eq!(usage.total, before - 100_000);
    assert!(usage.pools[0].members.iter().all(|m| m.shared == 50_000));

    // A second pass finds nothing new to do.
    let r = share(&c).unwrap();
    assert_eq!((r.shared, r.already_shared), (0, 2), "{r:?}");

    // Cargo rewriting one copy takes it out of the group.
    fs::write(&paths[1], &data).unwrap();
    let usage = buildq::gc::scan(&c, true);
    assert_eq!(usage.pools[0].shared, 50_000);
    let r = share(&c).unwrap();
    assert_eq!((r.shared, r.already_shared), (1, 1), "{r:?}");
}

/// An in-place rewrite that restores the old mtime, as
/// `cp --preserve=timestamps` does, breaks sharing and so the discount.
#[cfg(unix)]
#[test]
fn timestamp_preserving_rewrites_revoke_the_discount() {
    let temp = tempfile::tempdir().unwrap();
    let c = cfg(temp.path());
    let data: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
    put(&c, "p", 0, RLIB, &data);
    let b = put(&c, "p", 1, RLIB, &data);
    let Some(r) = share(&c) else { return };
    assert_eq!(r.shared, 1, "{r:?}");
    assert_eq!(buildq::gc::scan(&c, true).pools[0].shared, 50_000);

    let (inode, mtime) = (ino(&b), fs::metadata(&b).unwrap().modified().unwrap());
    let other: Vec<u8> = data.iter().map(|x| x ^ 0xff).collect();
    let f = fs::File::options().write(true).open(&b).unwrap();
    std::io::Write::write_all(&mut &f, &other).unwrap();
    f.set_modified(mtime).unwrap();
    drop(f);
    assert_eq!(
        (ino(&b), fs::metadata(&b).unwrap().modified().unwrap()),
        (inode, mtime)
    );

    assert_eq!(buildq::gc::scan(&c, true).pools[0].shared, 0);
    let r = share(&c).unwrap();
    assert_eq!((r.already_shared, r.differing), (0, 1), "{r:?}");
}

#[cfg(unix)]
#[test]
fn differing_files_and_leftover_clones() {
    let temp = tempfile::tempdir().unwrap();
    let c = cfg(temp.path());
    let a = put(&c, "p", 0, RLIB, &[1; 4096]);
    let b = put(&c, "p", 1, RLIB, &[2; 4096]);
    let leftover = put(&c, "p", 1, ".buildq-dedupe-123-0", &[1; 4096]);
    let (ia, ib) = (ino(&a), ino(&b));
    let Some(r) = share(&c) else { return };
    assert_eq!((r.shared, r.differing), (0, 1), "{r:?}");
    assert_eq!((ino(&a), ino(&b)), (ia, ib));
    assert_eq!(fs::read(&b).unwrap(), vec![2; 4096]);
    assert!(!leftover.exists());
    assert!(buildq::dedupe::Ledger::load(&c.cache_dir).groups.is_empty());
}

#[cfg(unix)]
#[test]
fn known_different_copies_are_skipped_until_either_changes() {
    let temp = tempfile::tempdir().unwrap();
    let c = cfg(temp.path());
    put(&c, "p", 0, RLIB, &[1; 4096]);
    let b = put(&c, "p", 1, RLIB, &[2; 4096]);
    let Some(r) = share(&c) else { return };
    assert_eq!((r.compared, r.differing), (1, 1), "{r:?}");

    let r = share(&c).unwrap();
    assert_eq!((r.compared, r.read_bytes), (0, 0), "{r:?}");
    assert_eq!(r.skipped_known_different, 1, "{r:?}");

    fs::write(&b, [1; 4096]).unwrap();
    age(&b, 10);
    let r = share(&c).unwrap();
    assert_eq!(
        (r.compared, r.shared, r.skipped_known_different),
        (1, 1, 0),
        "{r:?}"
    );
    assert!(
        buildq::dedupe::Ledger::load(&c.cache_dir)
            .differing
            .is_empty()
    );
}

#[cfg(unix)]
#[test]
fn passes_stopped_by_the_read_limit_resume_at_the_next_pair() {
    let temp = tempfile::tempdir().unwrap();
    let c = cfg(temp.path());
    for id in 0..4 {
        put(&c, "p", id, RLIB, &mid(id as u8));
    }
    let mut o = opts(&c, false);
    // Four fingerprints, then one comparison.
    o.max_read = 4 * 8192 + 2 * 20_000;
    let known = || buildq::dedupe::Ledger::load(&c.cache_dir).differing.len();
    // Room for one comparison per pass: each pass learns one new pair of
    // the six, never repeating one, until a pass completes.
    for n in 1..=6 {
        let r = dedupe::run(&c, &o);
        if r.unsupported.is_some() {
            return;
        }
        assert_eq!(r.compared, 1, "{r:?}");
        assert_eq!(known(), n);
    }
    let r = dedupe::run(&c, &o);
    assert_eq!((r.compared, r.read_limited), (0, false), "{r:?}");
    assert!(buildq::dedupe::Ledger::load(&c.cache_dir).resume.is_none());
}

/// Identical copies that differ from the group's first copy still share.
/// Unix only: a pass remembers differing copies by file identity.
#[cfg(unix)]
#[test]
fn copies_that_differ_from_the_first_are_compared_with_each_other() {
    let temp = tempfile::tempdir().unwrap();
    let c = cfg(temp.path());
    put(&c, "p", 0, RLIB, &mid(b'A'));
    put(&c, "p", 1, RLIB, &mid(b'B'));
    put(&c, "p", 2, RLIB, &mid(b'B'));
    put(&c, "p", 3, RLIB, &mid(b'C'));
    let r = dedupe::run(&c, &opts(&c, true));
    assert!(r.errors.is_empty(), "{r:?}");
    assert_eq!((r.compared, r.identical, r.differing), (5, 1, 4), "{r:?}");
}

#[cfg(unix)]
#[test]
fn groups_with_several_contents_share_within_each() {
    let temp = tempfile::tempdir().unwrap();
    let c = cfg(temp.path());
    put(&c, "p", 0, RLIB, &mid(b'A'));
    put(&c, "p", 1, RLIB, &mid(b'B'));
    put(&c, "p", 2, RLIB, &mid(b'B'));
    let Some(r) = share(&c) else { return };
    assert_eq!((r.shared, r.differing), (1, 2), "{r:?}");
    assert_eq!(buildq::gc::scan(&c, true).pools[0].shared, 20_000);
    // Nothing is left to compare on the next pass.
    let r = share(&c).unwrap();
    assert_eq!((r.compared, r.already_shared), (0, 1), "{r:?}");
}

/// Uplifted hard links on both sides are one copy each, not two.
#[cfg(unix)]
#[test]
fn hard_linked_names_count_once() {
    let temp = tempfile::tempdir().unwrap();
    let c = cfg(temp.path());
    for id in [0, 1] {
        let p = put(&c, "p", id, RLIB, &[4; 8192]);
        fs::hard_link(
            &p,
            p.parent().unwrap().parent().unwrap().join("libdep.rlib"),
        )
        .unwrap();
    }
    let dry = dedupe::run(&c, &opts(&c, true));
    assert_eq!(
        (dry.compared, dry.identical, dry.identical_bytes),
        (1, 1, 8192),
        "{dry:?}"
    );
    let Some(r) = share(&c) else { return };
    assert_eq!(
        (r.compared, r.shared, r.shared_bytes),
        (1, 1, 8192),
        "{r:?}"
    );
    let r = share(&c).unwrap();
    assert_eq!(
        (r.compared, r.already_shared, r.already_shared_bytes),
        (0, 1, 8192),
        "{r:?}"
    );
}

/// An alias may pair with another counterpart than its first name did.
#[cfg(unix)]
#[test]
fn an_alias_still_matches_a_different_counterpart() {
    let temp = tempfile::tempdir().unwrap();
    let c = cfg(temp.path());
    let up = |p: &Path| p.parent().unwrap().parent().unwrap().join("libdep.rlib");
    let a = put(&c, "p", 0, RLIB, &[b'A'; 8192]);
    fs::write(up(&a), [b'B'; 8192]).unwrap();
    let b = put(&c, "p", 1, RLIB, &[b'B'; 8192]);
    fs::hard_link(&b, up(&b)).unwrap();
    let dry = dedupe::run(&c, &opts(&c, true));
    assert_eq!(
        (dry.compared, dry.identical, dry.differing),
        (2, 1, 1),
        "{dry:?}"
    );
}

/// A failed replacement moves already re-linked names back (macOS).
#[cfg(target_os = "macos")]
#[test]
fn a_failed_relink_keeps_the_hard_links_together() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let c = cfg(temp.path());
    put(&c, "p", 0, RLIB, &[4; 8192]);
    let b = put(&c, "p", 1, RLIB, &[4; 8192]);
    let uplifted = b.parent().unwrap().parent().unwrap().join("libdep.rlib");
    fs::hard_link(&b, &uplifted).unwrap();
    // The alias moves first; replacing the hashed name then fails.
    let deps = b.parent().unwrap().to_path_buf();
    fs::set_permissions(&deps, fs::Permissions::from_mode(0o555)).unwrap();
    let r = dedupe::run(&c, &opts(&c, false));
    fs::set_permissions(&deps, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!((r.shared, r.errors.len()), (0, 1), "{r:?}");
    assert_eq!(ino(&b), ino(&uplifted));
    assert_eq!(fs::metadata(&b).unwrap().len(), 8192);
    let leftovers: Vec<_> = fs::read_dir(c.cache_dir.join("pools/p/m1"))
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(".buildq-dedupe-")
        })
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

/// A pass killed mid-relink left the alias on the clone: the next pass puts
/// the journaled names back together before removing its leftovers.
#[cfg(unix)]
#[test]
fn an_interrupted_relink_is_restored() {
    use std::os::unix::ffi::OsStrExt;
    let temp = tempfile::tempdir().unwrap();
    let c = cfg(temp.path());
    let b = put(&c, "p", 1, RLIB, &[4; 8192]);
    let uplifted = b.parent().unwrap().parent().unwrap().join("libdep.rlib");
    let member = c.cache_dir.join("pools/p/m1");
    let original = member.join(".buildq-dedupe-1-0.orig");
    fs::hard_link(&b, &original).unwrap();
    let mut names = Vec::new();
    for n in [&b, &uplifted] {
        names.extend_from_slice(n.as_os_str().as_bytes());
        names.push(0);
    }
    fs::write(member.join(".buildq-dedupe-1-0.names"), names).unwrap();
    // The alias already moved to a clone of the same content.
    fs::write(&uplifted, [4; 8192]).unwrap();
    assert_ne!(ino(&b), ino(&uplifted));

    let r = dedupe::run(&c, &opts(&c, false));
    assert!(r.errors.is_empty(), "{r:?}");
    assert_eq!(ino(&b), ino(&uplifted));
    assert_eq!(fs::read(&uplifted).unwrap(), vec![4; 8192]);
    let leftovers: Vec<_> = fs::read_dir(&member)
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(".buildq-dedupe-")
        })
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[cfg(unix)]
#[test]
fn hard_links_are_kept_together() {
    let temp = tempfile::tempdir().unwrap();
    let c = cfg(temp.path());
    let a = put(&c, "p", 0, RLIB, &[4; 8192]);
    let b = put(&c, "p", 1, RLIB, &[4; 8192]);
    let uplifted = b.parent().unwrap().parent().unwrap().join("libdep.rlib");
    fs::hard_link(&b, &uplifted).unwrap();
    // A name the index does not see (incremental state is never indexed).
    let hidden = b.parent().unwrap().parent().unwrap().join("incremental/x");
    fs::create_dir_all(hidden.parent().unwrap()).unwrap();
    fs::hard_link(&b, &hidden).unwrap();
    let Some(r) = share(&c) else { return };
    if cfg!(target_os = "linux") {
        // Shared in place: every name keeps pointing at the shared inode.
        assert_eq!(r.shared, 1, "{r:?}");
    } else {
        // Replacing the inode would split the unseen name off.
        assert_eq!((r.shared, r.skipped_hardlinked), (0, 1), "{r:?}");
        fs::remove_file(&hidden).unwrap();
        // With every name indexed, all of them move to the clone together.
        let r = share(&c).unwrap();
        assert_eq!(r.shared, 1, "{r:?}");
        assert_eq!(fs::read(&uplifted).unwrap(), fs::read(&a).unwrap());
    }
    assert_eq!(ino(&b), ino(&uplifted));
}

#[cfg(unix)]
#[test]
fn eviction_frees_only_bytes_no_other_member_shares() {
    let temp = tempfile::tempdir().unwrap();
    let mut c = cfg(temp.path());
    put(&c, "p", 0, RLIB, &[8; 40_000]);
    put(&c, "p", 1, RLIB, &[8; 40_000]);
    put(&c, "p", 0, "debug/deps/own-0", &[1; 1000]);
    put(&c, "p", 1, "debug/deps/own-1", &[2; 1000]);
    for id in [0, 1] {
        used(&c, id, buildq::util::now_ms() - 1000 * (2 - id as u64));
    }
    if share(&c).is_none() {
        return;
    }
    assert_eq!(buildq::gc::scan(&c, true).total, 42_000);
    // Evicting m0 leaves m1 holding the shared blocks: only 1000 bytes free.
    c.budget = 41_500;
    let report = buildq::gc::gc(&c, &buildq::gc::Options::default());
    assert_eq!(report.freed, 1000, "{report:?}");
    assert!(!c.cache_dir.join("pools/p/m0").exists());
    let usage = buildq::gc::scan(&c, true);
    assert_eq!((usage.total, usage.pools[0].shared), (41_000, 0));
}

#[cfg(unix)]
#[test]
fn gc_shares_before_budget_eviction_and_throttles_automatic_passes() {
    use buildq::gc::{Options as GcOptions, gc};
    let temp = tempfile::tempdir().unwrap();
    let mut c = cfg(temp.path());
    put(&c, "p", 0, RLIB, &[6; 30_000]);
    put(&c, "p", 1, RLIB, &[6; 30_000]);
    for id in [0, 1, 2] {
        used(&c, id, buildq::util::now_ms());
    }
    // Over budget only while the copies are counted twice.
    c.budget = 45_000;
    let pass = |dry_run, auto| GcOptions {
        dry_run,
        auto,
        dedupe: true,
        ..Default::default()
    };

    // A dry run projects what the real run does, sharing or (where the
    // volume cannot share) evicting, without changing anything.
    let dry = gc(&c, &pass(true, false));
    assert_eq!(dry.dedupe.as_ref().map(|d| d.identical), Some(1), "{dry:?}");
    assert!(!c.cache_dir.join("dedupe.json").exists());
    assert!(!c.cache_dir.join("dedupe.stamp").exists());

    let r = gc(&c, &pass(false, false));
    assert_eq!((dry.freed, dry.after), (r.freed, r.after), "{dry:?} {r:?}");
    assert_eq!(dry.actions.len(), r.actions.len(), "{dry:?} {r:?}");
    let Some(d) = &r.dedupe else { panic!("{r:?}") };
    if d.unsupported.is_some() {
        return;
    }
    assert_eq!(d.shared, 1, "{d:?}");
    // Sharing alone met the budget: no member was evicted.
    assert_eq!(r.freed, 0, "{r:?}");
    assert_eq!(r.after, 30_000);
    for id in [0, 1] {
        assert!(c.cache_dir.join(format!("pools/p/m{id}")).exists());
    }

    put(&c, "p", 2, RLIB, &[6; 30_000]);
    c.budget = u64::MAX;
    // Automatic runs wait an hour after the last pass.
    let r = gc(&c, &pass(false, true));
    assert!(r.dedupe.is_none(), "{r:?}");
    fs::write(c.cache_dir.join("dedupe.stamp"), "0").unwrap();
    fs::remove_file(c.cache_dir.join("gc.stamp")).unwrap();
    let r = gc(&c, &pass(false, true));
    assert_eq!(r.dedupe.map(|d| d.shared), Some(1));
}

#[test]
fn gc_without_dedupe_leaves_files_alone() {
    let temp = tempfile::tempdir().unwrap();
    let c = cfg(temp.path());
    put(&c, "p", 0, RLIB, &[6; 3000]);
    put(&c, "p", 1, RLIB, &[6; 3000]);
    let r = buildq::gc::gc(&c, &buildq::gc::Options::default());
    assert!(r.dedupe.is_none());
    assert!(!c.cache_dir.join("dedupe.json").exists());
}

fn toolchain(c: &Config, pool: &str, id: &str) {
    buildq::util::write_json(
        &c.cache_dir.join(format!("pools/{pool}/pool.json")),
        &buildq::pool::PoolInfo {
            toolchain_id: id.into(),
            ..Default::default()
        },
    )
    .unwrap();
}

#[test]
fn pools_on_one_toolchain_group_and_other_toolchains_never_do() {
    let temp = tempfile::tempdir().unwrap();
    let c = cfg(temp.path());
    for pool in ["app", "lib", "old"] {
        put(&c, pool, 0, RLIB, &[2; 5000]);
    }
    toolchain(&c, "app", "rustc-a");
    toolchain(&c, "lib", "rustc-a");
    toolchain(&c, "old", "rustc-b");
    let r = dedupe::run(&c, &opts(&c, true));
    assert_eq!((r.pools, r.compared, r.identical), (3, 1, 1), "{r:?}");
}

#[cfg(unix)]
#[test]
fn cross_pool_sharing_is_charged_once_and_survives_retiring_a_pool() {
    let temp = tempfile::tempdir().unwrap();
    let c = cfg(temp.path());
    let app = put(&c, "app", 0, RLIB, &[3; 20_000]);
    put(&c, "lib", 0, RLIB, &[3; 20_000]);
    toolchain(&c, "app", "rustc-a");
    toolchain(&c, "lib", "rustc-a");
    let Some(r) = share(&c) else { return };
    assert_eq!(r.shared, 1, "{r:?}");
    let usage = buildq::gc::scan(&c, true);
    assert_eq!(usage.total, 20_000);
    // The copy beyond the first is the second pool's discount.
    let shared: Vec<u64> = usage.pools.iter().map(|p| p.shared).collect();
    assert_eq!(shared, vec![0, 20_000]);

    // Removing the canonical copy's pool keeps the other copy intact.
    fs::remove_dir_all(app.parent().unwrap().parent().unwrap().parent().unwrap()).unwrap();
    let usage = buildq::gc::scan(&c, true);
    assert_eq!((usage.total, usage.pools[0].shared), (20_000, 0));
}

/// m0 holds a stale unit (hash `a…`, superseded by `b…`) whose rlib it shares
/// with m1, where it is still the only unit and so is not pruned.
#[cfg(unix)]
fn stale_shared_unit(c: &Config, uplift: bool) -> Option<()> {
    let stale = put(
        c,
        "p",
        0,
        "debug/deps/libdep-aaaaaaaaaaaaaaaa.rlib",
        &[1; 10_000],
    );
    if uplift {
        // Cargo's uplifted name keeps the stale inode alive after pruning.
        let uplifted = stale
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("libdep.rlib");
        fs::hard_link(&stale, uplifted).unwrap();
    }
    put(
        c,
        "p",
        1,
        "debug/deps/libdep-aaaaaaaaaaaaaaaa.rlib",
        &[1; 10_000],
    );
    put(
        c,
        "p",
        0,
        "debug/deps/libdep-bbbbbbbbbbbbbbbb.rlib",
        &[2; 5_000],
    );
    for (id, hash, days) in [
        (0, "aaaaaaaaaaaaaaaa", 30),
        (0, "bbbbbbbbbbbbbbbb", 0),
        (1, "aaaaaaaaaaaaaaaa", 30),
    ] {
        let fp = put(
            c,
            "p",
            id,
            &format!("debug/.fingerprint/dep-{hash}/lib-dep"),
            &[0; 10],
        );
        age(&fp, days * 86_400);
    }
    share(c)?;
    Some(())
}

/// Pruning one name of a shared, uplifted copy makes a real pass compare
/// the surviving names again; a dry run projects the same.
#[cfg(unix)]
#[test]
fn dry_runs_replan_copies_whose_aliases_pruning_removes() {
    let mut results = Vec::new();
    for dry_run in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let c = cfg(temp.path());
        for id in [0, 1] {
            used(&c, id, buildq::util::now_ms());
        }
        let m1 = c.cache_dir.join("pools/p/m1/debug");
        fs::create_dir_all(m1.join("deps")).unwrap();
        fs::write(m1.join("libdep.rlib"), [1; 10_000]).unwrap();
        if stale_shared_unit(&c, true).is_none() {
            return;
        }
        let r = buildq::gc::gc(
            &c,
            &buildq::gc::Options {
                dry_run,
                dedupe: true,
                ..Default::default()
            },
        );
        results.push((r.after, r.actions.len()));
    }
    assert_eq!(results[0], results[1], "{results:?}");
}

#[cfg(unix)]
#[test]
fn pruning_a_copy_another_member_shares_frees_only_its_own_bytes() {
    for dry_run in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let c = cfg(temp.path());
        for id in [0, 1] {
            used(&c, id, buildq::util::now_ms());
        }
        if stale_shared_unit(&c, false).is_none() {
            return;
        }
        let before = buildq::gc::scan(&c, true).total;
        let r = buildq::gc::gc(
            &c,
            &buildq::gc::Options {
                dry_run,
                ..Default::default()
            },
        );
        // Only the stale fingerprint is freed; m1 keeps the rlib's blocks.
        assert_eq!(r.freed, 10, "dry_run={dry_run} {r:?}");
        if !dry_run {
            assert_eq!(buildq::gc::scan(&c, true).total, before - 10);
        }
    }
}

#[cfg(unix)]
#[test]
fn dry_run_projects_eviction_after_pruning_like_a_real_run() {
    let mut runs = Vec::new();
    for dry_run in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let mut c = cfg(temp.path());
        used(&c, 0, buildq::util::now_ms());
        used(&c, 1, buildq::util::now_ms() - 60_000);
        if stale_shared_unit(&c, false).is_none() {
            return;
        }
        // After pruning: the new unit (5010) in m0, the old one (10010) in m1.
        c.budget = 14_000;
        let r = buildq::gc::gc(
            &c,
            &buildq::gc::Options {
                dry_run,
                ..Default::default()
            },
        );
        if !dry_run {
            assert!(!c.cache_dir.join("pools/p/m1").exists());
            assert!(c.cache_dir.join("pools/p/m0").exists());
        }
        runs.push((r.freed, r.after));
    }
    if runs.len() == 2 {
        assert_eq!(runs[0], runs[1], "dry run vs real run");
        assert_eq!(runs[1], (10_020, 5_010));
    }
}

#[cfg(unix)]
#[test]
fn a_failed_eviction_keeps_its_shared_blocks_charged() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let mut c = cfg(temp.path());
    let rel = "debug/deps/locked/libbig.rlib";
    let held = put(&c, "p", 0, rel, &[7; 20_000]);
    put(&c, "p", 1, rel, &[7; 20_000]);
    put(&c, "p", 0, "debug/deps/own-0", &[1; 1000]);
    put(&c, "p", 1, "debug/deps/own-1", &[2; 1000]);
    used(&c, 0, buildq::util::now_ms() - 60_000);
    used(&c, 1, buildq::util::now_ms());
    if share(&c).is_none() {
        return;
    }
    // m0's shared copy cannot be deleted, even after moving it to trash.
    let locked = held.parent().unwrap().to_path_buf();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    if fs::write(locked.join("probe"), b"").is_ok() {
        eprintln!("skipping: permissions are not enforced (root?)");
        return;
    }
    c.budget = 0;
    let r = buildq::gc::gc(&c, &buildq::gc::Options::default());
    let trash: Vec<_> = fs::read_dir(c.cache_dir.join("trash"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    for t in &trash {
        let _ = fs::set_permissions(
            t.join("debug/deps/locked"),
            fs::Permissions::from_mode(0o755),
        );
    }
    assert_eq!(r.failed_deletions, 1, "{r:?}");
    // The trash copy still holds the shared blocks: evicting m1 frees only
    // its own file.
    assert_eq!(r.freed, 1000, "{r:?}");
}

#[cfg(unix)]
#[test]
fn a_surviving_hard_link_keeps_a_pruned_copy_shared() {
    let mut after = Vec::new();
    for dry_run in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let c = cfg(temp.path());
        for id in [0, 1] {
            used(&c, id, buildq::util::now_ms());
        }
        if stale_shared_unit(&c, true).is_none() {
            return;
        }
        let r = buildq::gc::gc(
            &c,
            &buildq::gc::Options {
                dry_run,
                ..Default::default()
            },
        );
        // The inode survives through its alias: only the fingerprint goes.
        assert_eq!(r.freed, 10, "dry_run={dry_run} {r:?}");
        after.push(r.after);
        if !dry_run {
            assert!(c.cache_dir.join("pools/p/m0/debug/libdep.rlib").exists());
            // Unlinking a name changes the inode's ctime, which a rewrite
            // would too: the survivor is conservatively counted in full.
            let usage = buildq::gc::scan(&c, true);
            assert_eq!(usage.pools[0].shared, 0, "{usage:?}");
            assert_eq!(usage.total, r.after);
        }
    }
    // A dry run projects the same usage the real run leaves.
    assert_eq!(after[0], after[1], "{after:?}");
}

/// Removing the idle pool afterwards cancels a revoked discount.
#[cfg(unix)]
#[test]
fn dry_runs_project_revoked_discounts_of_removed_pools() {
    let mut after = Vec::new();
    for dry_run in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let c = cfg(temp.path());
        for id in [0, 1] {
            used(&c, id, 0);
        }
        if stale_shared_unit(&c, true).is_none() {
            return;
        }
        let r = buildq::gc::gc(
            &c,
            &buildq::gc::Options {
                dry_run,
                ..Default::default()
            },
        );
        after.push(r.after);
    }
    assert_eq!(after, [0, 0]);
}

#[cfg(unix)]
#[test]
fn a_rewritten_canonical_starts_a_new_group() {
    let temp = tempfile::tempdir().unwrap();
    let c = cfg(temp.path());
    let a = put(&c, "p", 0, RLIB, &[1; 8000]);
    put(&c, "p", 1, RLIB, &[1; 8000]);
    if share(&c).is_none() {
        return;
    }
    // m0 is rebuilt with other content while m1 is busy; m2 matches m0 now.
    fs::write(&a, [2; 8000]).unwrap();
    put(&c, "p", 2, RLIB, &[2; 8000]);
    let busy = lock::lock(&c.cache_dir.join("pools/p/m1.lock")).unwrap();
    let r = share(&c).unwrap();
    assert_eq!(r.shared, 1, "{r:?}");
    drop(busy);
    // Only m0 and m2 share; m1 keeps its own, different data.
    let usage = buildq::gc::scan(&c, true);
    assert_eq!(usage.pools[0].shared, 8000, "{usage:?}");
    let groups = buildq::dedupe::Ledger::load(&c.cache_dir).groups;
    assert_eq!(groups.len(), 1, "{groups:?}");
    let members: Vec<usize> = groups[0].entries.iter().map(|e| e.member).collect();
    assert_eq!(members, vec![0, 2]);
}

#[cfg(unix)]
#[test]
fn a_replaced_alias_does_not_keep_a_pruned_copy_alive() {
    for dry_run in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let c = cfg(temp.path());
        for id in [0, 1] {
            used(&c, id, buildq::util::now_ms());
        }
        if stale_shared_unit(&c, true).is_none() {
            return;
        }
        // Cargo uplifts a newer build: the alias now names another inode.
        let uplifted = c.cache_dir.join("pools/p/m0/debug/libdep.rlib");
        fs::remove_file(&uplifted).unwrap();
        fs::write(&uplifted, [3; 10_000]).unwrap();
        let r = buildq::gc::gc(
            &c,
            &buildq::gc::Options {
                dry_run,
                ..Default::default()
            },
        );
        // Unlinking the alias changed the rlib's ctime, so its sharing is no
        // longer proven: usage counted it in full, and so does pruning it.
        assert_eq!(r.freed, 10_010, "dry_run={dry_run} {r:?}");
    }
}

/// `n` copies of one path and length; the first `same` are identical and the
/// rest each differ in the first block.
fn divergent(c: &Config, n: usize, same: usize) {
    for i in 0..n {
        let mut data = vec![9u8; 20_000];
        let id = if i < same { 0 } else { i };
        data[..8].copy_from_slice(&(id as u64).to_le_bytes());
        put(c, "p", i, RLIB, &data);
    }
}

#[test]
fn copies_that_differ_in_the_first_block_are_not_compared_pairwise() {
    for n in [8, 32, 128] {
        let temp = tempfile::tempdir().unwrap();
        let c = cfg(temp.path());
        divergent(&c, n, 0);
        let r = dedupe::run(&c, &opts(&c, true));
        assert!(r.errors.is_empty(), "{r:?}");
        eprintln!("n={n} compared={} read_bytes={}", r.compared, r.read_bytes);
        assert_eq!((r.compared, r.identical), (0, 0), "{r:?}");
        assert_eq!(r.read_bytes, (n * 8192) as u64);
    }
}

#[test]
fn identical_copies_among_divergent_ones_are_still_found() {
    let temp = tempfile::tempdir().unwrap();
    let c = cfg(temp.path());
    divergent(&c, 20, 5);
    let r = dedupe::run(&c, &opts(&c, true));
    assert!(r.errors.is_empty(), "{r:?}");
    assert_eq!((r.compared, r.identical, r.differing), (4, 4, 0), "{r:?}");
}

#[test]
fn fingerprints_count_against_the_read_limit() {
    let temp = tempfile::tempdir().unwrap();
    let mut c = cfg(temp.path());
    divergent(&c, 8, 0);
    c.dedupe_max_read = 8192 * 4;
    let r = dedupe::run(&c, &opts(&c, true));
    assert!(r.read_limited && r.read_bytes <= c.dedupe_max_read, "{r:?}");
}

#[test]
fn settled_groups_read_nothing_on_the_next_pass() {
    let temp = tempfile::tempdir().unwrap();
    let c = cfg(temp.path());
    divergent(&c, 12, 4);
    let Some(first) = share(&c) else { return };
    assert!(first.read_bytes > 0, "{first:?}");
    let r = share(&c).unwrap();
    assert_eq!((r.compared, r.read_bytes), (0, 0), "{r:?}");
}

#[test]
fn passes_limited_while_fingerprinting_still_cover_every_group() {
    let temp = tempfile::tempdir().unwrap();
    let mut c = cfg(temp.path());
    for g in 0..6 {
        let rel = format!("debug/deps/libdep{g}-0123456789abcdef.rlib");
        for id in 0..4 {
            let mut data = vec![9u8; 20_000];
            data[0] = id as u8;
            put(&c, "p", id, &rel, &data);
        }
    }
    // Room for three fingerprints per pass, far fewer than the 24 copies.
    c.dedupe_max_read = 3 * 8192;
    let mut passes = 0;
    loop {
        let r = dedupe::run(&c, &opts(&c, false));
        if r.unsupported.is_some() {
            return;
        }
        assert!(r.read_bytes <= c.dedupe_max_read, "{r:?}");
        passes += 1;
        assert!(passes <= 12, "no progress: {r:?}");
        if !r.read_limited {
            break;
        }
    }
    let prints = buildq::dedupe::Ledger::load(&c.cache_dir).prints.len();
    assert_eq!(prints, 24);
}
