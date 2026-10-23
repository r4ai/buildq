//! A machine-wide, daemon-less job queue made of plain files and OS locks.
//!
//! Layout of one class (`<queue dir>/<class>/`):
//!
//! ```text
//! queue.lock            short critical section: ticket creation, slot hand-out
//! seq                   monotonically increasing ticket counter
//! slot-<i>.lock         held by the job running in slot i (for its whole life)
//! slot-<i>.json         who holds slot i (written by the holder)
//! tickets/t-<seq>-<pid>.lock / .json   one per waiting job, locked by the waiter
//! ```
//!
//! Every lock is tied to a process handle, so a killed holder or waiter
//! disappears from the queue by itself: a ticket whose lock can be taken is
//! dead and gets pruned; a slot whose lock can be taken is free.
//!
//! Under `queue.lock`, waiters below the soft worktree cap precede those at
//! the cap; each group keeps (priority desc, seq asc) order. Only the head
//! takes a slot, then ranking is recomputed with the new holder. This keeps
//! FIFO within equal standing even when multiple slots become free at once.
//! If every waiter is at the cap, the head still runs: idle slots are usable.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::lock::{self, LockFile};
use crate::util::{now_ms, read_json, write_json};

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct JobInfo {
    pub pid: u32,
    pub class: String,
    pub cmd: String,
    pub cwd: String,
    /// Canonical Git worktree root, or canonical cwd outside Git.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    /// Computed by status, never persisted in a ticket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_reason: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub enqueued_ms: u64,
    #[serde(default)]
    pub started_ms: u64,
    /// Pool member or host the job runs on, when known.
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotStatus {
    pub slot: usize,
    pub busy: bool,
    pub job: Option<JobInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassStatus {
    pub class: String,
    pub slots: usize,
    pub holders: Vec<SlotStatus>,
    pub waiters: Vec<JobInfo>,
}

impl ClassStatus {
    pub fn busy(&self) -> usize {
        self.holders.iter().filter(|h| h.busy).count()
    }
    pub fn free(&self) -> usize {
        self.slots.saturating_sub(self.busy())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitState {
    /// 0-based rank among waiters.
    pub position: usize,
    pub waiters: usize,
    pub busy: usize,
}

pub struct ClassQueue {
    pub dir: PathBuf,
    pub class: String,
    pub slots: usize,
    pub poll: Duration,
    pub max_per_worktree: usize,
}

/// Holding this means holding a slot. Dropping it frees the slot.
#[derive(Debug)]
pub struct SlotGuard {
    _lock: LockFile,
    pub slot: usize,
    pub info: JobInfo,
    info_path: PathBuf,
    pub waited_ms: u64,
}

impl SlotGuard {
    pub fn set_target(&mut self, target: &str) {
        self.info.target = Some(target.to_string());
        let _ = write_json(&self.info_path, &self.info);
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.info_path);
    }
}

struct Ticket {
    lock: Option<LockFile>,
    json: PathBuf,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let lock_path = self.lock.as_ref().map(|l| l.path.clone());
        self.lock = None;
        let _ = fs::remove_file(&self.json);
        if let Some(p) = lock_path {
            let _ = fs::remove_file(p);
        }
    }
}

impl ClassQueue {
    pub fn new(queue_root: &Path, class: &str, slots: usize, poll: Duration) -> ClassQueue {
        ClassQueue {
            dir: queue_root.join(sanitize(class)),
            class: class.to_string(),
            slots: slots.max(1),
            poll,
            max_per_worktree: slots.max(1).div_ceil(2),
        }
    }

    pub fn with_max_per_worktree(mut self, cap: usize) -> Self {
        self.max_per_worktree = cap;
        self
    }

    /// Reorder using a snapshot of locked slots, never stale slot JSON alone.
    fn rank_tickets(&self, tickets: &mut [(PathBuf, JobInfo)], holders: &[JobInfo]) {
        let running = |job: &JobInfo| {
            holders
                .iter()
                .filter(|h| worktree(h) == worktree(job))
                .count()
        };
        let capped =
            |job: &JobInfo| self.max_per_worktree != 0 && running(job) >= self.max_per_worktree;
        tickets.sort_by_key(|(_, job)| (capped(job), std::cmp::Reverse(job.priority), job.seq));
        // A capped ticket is deferred only if an uncapped competitor exists.
        let preferred = tickets
            .first()
            .and_then(|(_, job)| (!capped(job)).then(|| worktree(job).to_string()));
        for (_, job) in tickets {
            job.wait_reason =
                if capped(job) && preferred.as_deref().is_some_and(|w| w != worktree(job)) {
                    Some("worktree cap".into())
                } else {
                    None
                };
        }
    }

    fn slot_lock(&self, i: usize) -> PathBuf {
        self.dir.join(format!("slot-{i}.lock"))
    }

    fn slot_json(&self, i: usize) -> PathBuf {
        self.dir.join(format!("slot-{i}.json"))
    }

    fn tickets_dir(&self) -> PathBuf {
        self.dir.join("tickets")
    }

    fn next_seq(&self) -> u64 {
        let p = self.dir.join("seq");
        // Wall-clock milliseconds as a floor keep the order right even if the
        // counter file cannot be written (e.g. created by another sandbox).
        let n = (fs::read_to_string(&p)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
            + 1)
        .max(now_ms());
        let _ = fs::write(&p, n.to_string());
        n
    }

    /// Live tickets in service order. With `prune`, dead tickets are deleted.
    fn live_tickets(&self, prune: bool) -> Vec<(PathBuf, JobInfo)> {
        let mut out = Vec::new();
        let Ok(rd) = fs::read_dir(self.tickets_dir()) else {
            return out;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let lock_path = p.with_extension("lock");
            if lock::is_held(&lock_path) {
                if let Some(info) = read_json::<JobInfo>(&p) {
                    out.push((p, info));
                }
            } else if prune {
                let _ = fs::remove_file(&p);
                let _ = fs::remove_file(&lock_path);
            }
        }
        out.sort_by_key(|(_, i)| (std::cmp::Reverse(i.priority), i.seq));
        out
    }

    /// Waits for a slot. `on_wait` is called whenever the wait state changes.
    pub fn acquire(
        &self,
        mut info: JobInfo,
        on_wait: &mut dyn FnMut(WaitState),
    ) -> io::Result<SlotGuard> {
        fs::create_dir_all(self.tickets_dir())?;
        info.class = self.class.clone();
        info.enqueued_ms = now_ms();
        let queue_lock = self.dir.join("queue.lock");

        // Enqueue: create and lock the ticket inside the critical section so a
        // pruner can never see it unlocked.
        let ticket = {
            let _g = lock::lock(&queue_lock)?;
            info.seq = self.next_seq();
            let base = self
                .tickets_dir()
                .join(format!("t-{:012}-{}", info.seq, info.pid));
            let json = base.with_extension("json");
            write_json(&json, &info)?;
            let l = lock::try_lock(&base.with_extension("lock"))?
                .ok_or_else(|| io::Error::other("fresh ticket lock already held"))?;
            Ticket {
                lock: Some(l),
                json,
            }
        };

        let mut last: Option<WaitState> = None;
        loop {
            {
                let _g = lock::lock(&queue_lock)?;
                let mut tickets = self.live_tickets(true);
                let mut free: Vec<(usize, LockFile)> = Vec::new();
                let mut holders = Vec::new();
                for i in 0..self.slots {
                    if let Some(l) = lock::try_lock(&self.slot_lock(i))? {
                        free.push((i, l));
                    } else if let Some(job) = read_json::<JobInfo>(&self.slot_json(i)) {
                        holders.push(job);
                    }
                }
                self.rank_tickets(&mut tickets, &holders);
                let position = tickets
                    .iter()
                    .position(|(p, _)| *p == ticket.json)
                    .unwrap_or(tickets.len());
                if position == 0 && !free.is_empty() {
                    let (slot, l) = free.swap_remove(0);
                    drop(free);
                    info.started_ms = now_ms();
                    let info_path = self.slot_json(slot);
                    if write_json(&info_path, &info).is_err() {
                        // Not ours to overwrite (another sandbox): status falls back to "busy".
                        let _ = fs::remove_file(&info_path);
                    }
                    drop(ticket);
                    return Ok(SlotGuard {
                        _lock: l,
                        slot,
                        waited_ms: info.started_ms - info.enqueued_ms,
                        info,
                        info_path,
                    });
                }
                let state = WaitState {
                    position,
                    waiters: tickets.len(),
                    busy: self.slots - free.len(),
                };
                if last != Some(state) {
                    on_wait(state);
                    last = Some(state);
                }
            }
            thread::sleep(self.poll);
        }
    }

    pub fn status(&self) -> ClassStatus {
        // Use the same critical section and ranking as slot admission.
        let _g = lock::lock(&self.dir.join("queue.lock")).ok();
        let mut holders = Vec::new();
        for i in 0..self.slots {
            let busy = lock::is_held(&self.slot_lock(i));
            let job = if busy {
                read_json::<JobInfo>(&self.slot_json(i))
            } else {
                None
            };
            holders.push(SlotStatus { slot: i, busy, job });
        }
        let running: Vec<_> = holders.iter().filter_map(|h| h.job.clone()).collect();
        let mut tickets = self.live_tickets(false);
        self.rank_tickets(&mut tickets, &running);
        ClassStatus {
            class: self.class.clone(),
            slots: self.slots,
            holders,
            waiters: tickets.into_iter().map(|(_, i)| i).collect(),
        }
    }
}

fn worktree(job: &JobInfo) -> &str {
    job.worktree.as_deref().unwrap_or(&job.cwd)
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

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
}
