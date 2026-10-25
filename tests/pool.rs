//! Lease transitions run without unrelated process-spawning tests.
use buildq::git::Worktree;
use buildq::pool::*;
use buildq::util::{read_json, write_json};
use std::{path::PathBuf, thread, time::Duration};
fn m(pairs: &[(&str, &str)]) -> Manifest {
    pairs
        .iter()
        .map(|(p, id)| (p.to_string(), id.to_string()))
        .collect()
}
fn owner(path: &str) -> Option<String> {
    path.strip_prefix("crates/")
        .and_then(|p| p.split('/').next())
        .map(str::to_owned)
}
fn wt(name: &str) -> Worktree {
    Worktree {
        root: PathBuf::from(format!("/w/{name}")),
        common_dir: PathBuf::from("/w/main/.git"),
        head: None,
    }
}
#[test]
fn lease_prefers_closest_member_and_grows_only_when_busy() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Pool::open(dir.path().join("p"), PoolInfo::default()).unwrap();
    let poll = Duration::from_millis(5);
    let mut owners = |f: &str, _: Option<&str>| owner(f);
    let a = m(&[("crates/a/src/lib.rs", "1"), ("crates/b/src/lib.rs", "1")]);
    let b = m(&[("crates/a/src/lib.rs", "2"), ("crates/b/src/lib.rs", "2")]);

    // Two concurrent leases create two members.
    let l0 = pool
        .lease(
            &wt("a"),
            &a,
            &RebuildGraph::default(),
            &mut owners,
            3,
            &mut |_| true,
            poll,
            &mut || {},
        )
        .unwrap();
    assert!(l0.fresh_member);
    let l1 = pool
        .lease(
            &wt("b"),
            &b,
            &RebuildGraph::default(),
            &mut owners,
            3,
            &mut |_| true,
            poll,
            &mut || {},
        )
        .unwrap();
    assert!(l1.fresh_member);
    let (id_a, id_b) = (l0.id, l1.id);
    drop(l0);
    drop(l1);

    // Metadata from before root invalidation cannot certify compiled
    // paths, even when last_worktree already matches the current root.
    let mut old_meta: MemberMeta = read_json(&pool.meta_path(id_b)).unwrap();
    old_meta.source_root = None;
    write_json(&pool.meta_path(id_b), &old_meta).unwrap();
    let l = pool
        .lease(
            &wt("b"),
            &b,
            &RebuildGraph::default(),
            &mut owners,
            3,
            &mut |_| true,
            poll,
            &mut || {},
        )
        .unwrap();
    assert_eq!(l.id, id_b);
    assert_eq!(l.plan.packages.iter().collect::<Vec<_>>(), ["a", "b"]);
    drop(l);

    // Worktree b again: picks the member whose manifest matches, no invalidation.
    let l = pool
        .lease(
            &wt("b"),
            &b,
            &RebuildGraph::default(),
            &mut owners,
            3,
            &mut |_| true,
            poll,
            &mut || {},
        )
        .unwrap();
    assert_eq!(l.id, id_b);
    assert!(l.plan.packages.is_empty());
    drop(l);

    // A third worktree goes to a's member, but both local crates must
    // rebuild because their compiled paths refer to a different root.
    let c = m(&[("crates/a/src/lib.rs", "1"), ("crates/b/src/lib.rs", "3")]);
    let l = pool
        .lease(
            &wt("c"),
            &c,
            &RebuildGraph::default(),
            &mut owners,
            3,
            &mut |_| true,
            poll,
            &mut || {},
        )
        .unwrap();
    assert_eq!(l.id, id_a);
    assert_eq!(l.plan.packages.iter().collect::<Vec<_>>(), ["a", "b"]);
    assert!(!l.fresh_member);
    drop(l);
    assert_eq!(pool.member_ids().len(), 2);
}

#[test]
fn lease_waits_when_full() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("p");
    let pool = Pool::open(root.clone(), PoolInfo::default()).unwrap();
    let poll = Duration::from_millis(5);
    let mut owners = |f: &str, _: Option<&str>| owner(f);
    let a = m(&[("crates/a/src/lib.rs", "1")]);
    let held = pool
        .lease(
            &wt("a"),
            &a,
            &RebuildGraph::default(),
            &mut owners,
            1,
            &mut |_| true,
            poll,
            &mut || {},
        )
        .unwrap();
    let waiting = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = waiting.clone();
    let t = thread::spawn(move || {
        let pool = Pool {
            dir: root,
            seed: false,
        };
        let mut owners = |f: &str, _: Option<&str>| owner(f);
        let l = pool
            .lease(
                &wt("b"),
                &a,
                &RebuildGraph::default(),
                &mut owners,
                1,
                &mut |_| true,
                poll,
                &mut || flag.store(true, std::sync::atomic::Ordering::SeqCst),
            )
            .unwrap();
        l.id
    });
    while !waiting.load(std::sync::atomic::Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(2));
    }
    let id = held.id;
    drop(held);
    assert_eq!(t.join().unwrap(), id);
}

/// With seeding, worktrees taking turns keep one warm member each while the
/// pool may grow, instead of rebuilding one shared member at every switch.
#[test]
fn seeding_keeps_a_warm_member_per_alternating_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Pool::open(dir.path().join("p"), PoolInfo::default())
        .unwrap()
        .with_seeding(true);
    let mut owners = |f: &str, _: Option<&str>| owner(f);
    let mut lease = |name: &str, current: &Manifest, max: usize| {
        pool.lease(
            &wt(name),
            current,
            &RebuildGraph::default(),
            &mut owners,
            max,
            &mut |_| true,
            Duration::from_millis(5),
            &mut || {},
        )
        .unwrap()
    };
    let a = m(&[("crates/a/src/lib.rs", "1")]);
    let b = m(&[("crates/a/src/lib.rs", "2")]);
    let l = lease("a", &a, 3);
    assert_eq!(l.id, 0);
    std::fs::write(l.target_dir.join("artifact"), "built for a").unwrap();
    drop(l);

    let l = lease("b", &b, 3);
    if l.seeded_from.is_none() {
        // This volume cannot clone: b reuses a's member as before.
        assert_eq!(l.id, 0);
        return;
    }
    assert_eq!((l.id, l.seeded_from), (1, Some(0)));
    assert!(l.target_dir.join("artifact").exists());
    assert_eq!(l.plan.packages.iter().collect::<Vec<_>>(), ["a"]);
    drop(l);
    for (name, current, id) in [("a", &a, 0), ("b", &b, 1)] {
        let l = lease(name, current, 3);
        assert_eq!(l.id, id);
        assert!(l.plan.packages.is_empty(), "{name}: {:?}", l.plan);
    }

    // At the member cap a third worktree reuses the closest member.
    let c = m(&[("crates/a/src/lib.rs", "3")]);
    let l = lease("c", &c, 2);
    assert!(!l.fresh_member && l.seeded_from.is_none());
}
