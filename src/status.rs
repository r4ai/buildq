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
