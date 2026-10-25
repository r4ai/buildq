//! Threaded queue/lock tests, isolated from unrelated fork/exec probes.
use buildq::queue::{ClassQueue, JobInfo};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::{thread, time::Duration};

fn info(cmd: &str, prio: i32) -> JobInfo {
    JobInfo {
        pid: std::process::id(),
        cmd: cmd.into(),
        priority: prio,
        ..Default::default()
    }
}

#[test]
fn concurrency_never_exceeds_slots() {
    let dir = tempfile::tempdir().unwrap();
    let running = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));
    let mut hs = Vec::new();
    for n in 0..8 {
        let root = dir.path().to_path_buf();
        let running = running.clone();
        let max_seen = max_seen.clone();
        hs.push(thread::spawn(move || {
            let q = ClassQueue::new(&root, "build", 3, Duration::from_millis(5));
            let g = q.acquire(info(&format!("job{n}"), 0), &mut |_| {}).unwrap();
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            max_seen.fetch_max(now, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(40));
            running.fetch_sub(1, Ordering::SeqCst);
            drop(g);
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    let m = max_seen.load(Ordering::SeqCst);
    assert!((1..=3).contains(&m), "max concurrency {m}");
}

#[test]
fn fifo_and_priority_order() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let q = ClassQueue::new(&root, "build", 1, Duration::from_millis(5));
    // Occupy the only slot, then enqueue waiters in a known order.
    let blocker = q.acquire(info("blocker", 0), &mut |_| {}).unwrap();
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut hs = Vec::new();
    for (name, prio) in [("a", 0), ("b", 0), ("urgent", 5), ("c", 0)] {
        let root = root.clone();
        let order = order.clone();
        hs.push(thread::spawn(move || {
            let q = ClassQueue::new(&root, "build", 1, Duration::from_millis(5));
            let g = q.acquire(info(name, prio), &mut |_| {}).unwrap();
            order.lock().unwrap().push(name.to_string());
            thread::sleep(Duration::from_millis(10));
            drop(g);
        }));
        // Make enqueue order deterministic.
        loop {
            let n = q.status().waiters.len();
            if n == hs.len() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
    let st = q.status();
    assert_eq!(st.busy(), 1);
    let names: Vec<_> = st.waiters.iter().map(|w| w.cmd.clone()).collect();
    assert_eq!(names, ["urgent", "a", "b", "c"]);
    drop(blocker);
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(*order.lock().unwrap(), ["urgent", "a", "b", "c"]);
}

#[test]
fn status_reports_holder() {
    let dir = tempfile::tempdir().unwrap();
    let q = ClassQueue::new(dir.path(), "build", 2, Duration::from_millis(5));
    let mut g = q.acquire(info("cargo build", 0), &mut |_| {}).unwrap();
    g.set_target("m0");
    let st = q.status();
    assert_eq!(st.busy(), 1);
    assert_eq!(st.free(), 1);
    let job = st
        .holders
        .iter()
        .find(|h| h.busy)
        .unwrap()
        .job
        .clone()
        .unwrap();
    assert_eq!(job.cmd, "cargo build");
    assert_eq!(job.target.as_deref(), Some("m0"));
    drop(g);
    assert_eq!(q.status().busy(), 0);
}

/// A released slot wakes the waiter at once instead of after the poll
/// interval, which stays the fallback for holders that die without telling.
#[test]
fn a_release_wakes_the_next_waiter_before_the_poll_interval() {
    let dir = tempfile::tempdir().unwrap();
    let poll = Duration::from_secs(5);
    let q = ClassQueue::new(dir.path(), "build", 1, poll);
    let held = q.acquire(info("first", 0), &mut |_| {}).unwrap();
    let root = dir.path().to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    let waiter = thread::spawn(move || {
        let q = ClassQueue::new(&root, "build", 1, poll);
        let mut queued = Some(tx);
        let g = q
            .acquire(info("second", 0), &mut |_| {
                if let Some(tx) = queued.take() {
                    tx.send(()).unwrap();
                }
            })
            .unwrap();
        drop(g);
    });
    rx.recv().unwrap();
    let released = std::time::Instant::now();
    drop(held);
    waiter.join().unwrap();
    assert!(
        released.elapsed() < Duration::from_secs(1),
        "woke after {:?}",
        released.elapsed()
    );
}
