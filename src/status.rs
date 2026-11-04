//! Queue snapshots shared by status and live monitoring.

use std::time::Duration;

use crate::config::Config;
use crate::queue::{ClassQueue, ClassStatus};

pub fn classes(cfg: &Config) -> Vec<ClassStatus> {
    cfg.known_classes()
        .iter()
        .map(|class| {
            ClassQueue::new(
                &cfg.queue_dir,
                class,
                cfg.class_slots(class),
                Duration::from_millis(cfg.poll_ms),
            )
            .with_max_per_worktree(cfg.max_per_worktree)
            .status()
        })
        .collect()
}

/// A job waiting for a pool member before it queues for a slot (#85): it
/// holds no slot, so the class queues do not show it (#150).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemberWaiter {
    pub pid: u32,
    pub cmd: String,
    pub cwd: String,
    /// The pool dir name (`<repo>-<hash>`).
    pub pool: String,
    pub since_ms: u64,
}

fn waiters_dir(cfg: &Config) -> std::path::PathBuf {
    cfg.queue_dir.join("member-waiters")
}

/// Registered while its lock is held: a waiter killed mid-wait drops out
/// of the listing by itself.
pub struct WaiterGuard {
    record: std::path::PathBuf,
    _lock: crate::lock::LockFile,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.record);
    }
}

pub fn register_member_waiter(cfg: &Config, w: &MemberWaiter) -> Option<WaiterGuard> {
    let dir = waiters_dir(cfg);
    std::fs::create_dir_all(&dir).ok()?;
    // Unique per registration, not just per pid: builds in containers with
    // their own PID namespaces may share the queue dir. Never waits: a
    // taken name only costs this waiter its listing.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let id = format!("{}-{nanos}", w.pid);
    let lock = crate::lock::try_lock(&dir.join(format!("{id}.lock"))).ok()??;
    let record = dir.join(format!("{id}.json"));
    crate::util::write_json(&record, w).ok()?;
    Some(WaiterGuard {
        record,
        _lock: lock,
    })
}

/// Jobs waiting for a pool member now, oldest first. Records whose lock
/// is no longer held (the waiter died) are skipped and removed.
pub fn member_waiters(cfg: &Config) -> Vec<MemberWaiter> {
    let dir = waiters_dir(cfg);
    let mut found = Vec::new();
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = e.path();
        if path.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let lock = path.with_extension("lock");
        if !crate::lock::is_held(&lock) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if let Some(w) = crate::util::read_json::<MemberWaiter>(&path) {
            found.push(w);
        }
    }
    found.sort_by_key(|w| w.since_ms);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_waiters_are_listed_while_registered_and_dead_ones_drop_out() {
        let d = tempfile::tempdir().unwrap();
        let cfg = Config::from_parts(
            d.path().join("q"),
            d.path().join("q/config.toml"),
            crate::config::FileConfig::default(),
        );
        let w = |pid, since_ms| MemberWaiter {
            pid,
            cmd: "cargo build".into(),
            cwd: "/w".into(),
            pool: "p".into(),
            since_ms,
        };
        let later = register_member_waiter(&cfg, &w(2, 20)).unwrap();
        let first = register_member_waiter(&cfg, &w(1, 10)).unwrap();
        let pids = |v: Vec<MemberWaiter>| v.iter().map(|w| w.pid).collect::<Vec<_>>();
        assert_eq!(pids(member_waiters(&cfg)), [1, 2]);
        drop(first);
        assert_eq!(pids(member_waiters(&cfg)), [2]);
        // A waiter killed mid-wait leaves its record, but not its lock.
        let dead = waiters_dir(&cfg).join("3-1.json");
        crate::util::write_json(&dead, &w(3, 5)).unwrap();
        assert_eq!(pids(member_waiters(&cfg)), [2]);
        assert!(!dead.exists());
        // The same pid twice (separate PID namespaces) neither blocks nor
        // replaces the other.
        let twin = register_member_waiter(&cfg, &w(2, 30)).unwrap();
        assert_eq!(pids(member_waiters(&cfg)), [2, 2]);
        drop(twin);
        drop(later);
        assert!(member_waiters(&cfg).is_empty());
    }
}
