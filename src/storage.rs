//! Free-space admission and pressure checks for pooled Cargo builds.

use std::io;

use crate::config::Config;
use crate::util::{free_disk_bytes, human_bytes};

pub fn check(cfg: &Config) -> io::Result<()> {
    if cfg.min_free == 0 {
        return Ok(());
    }
    let free = free_disk_bytes(&cfg.cache_dir).ok_or_else(|| {
        io::Error::other(format!(
            "cannot measure free space for {}",
            cfg.cache_dir.display()
        ))
    })?;
    if free <= cfg.min_free {
        return Err(io::Error::other(format!(
            "cache volume has {} free; min_free requires more than {}. Free disk space or lower [cache].min_free",
            human_bytes(free),
            human_bytes(cfg.min_free)
        )));
    }
    Ok(())
}

pub fn admit(cfg: &Config) -> Result<(), String> {
    if check(cfg).is_ok() {
        return Ok(());
    }
    // Reclaim idle artifacts once; do not park a build slot waiting for disk.
    let report = crate::gc::gc(cfg, &crate::gc::Options::default());
    check(cfg).map_err(|error| {
        if report.failed_deletions > 0 {
            format!(
                "pooled build refused: {error}; GC: {}",
                report.warnings.join("; ")
            )
        } else {
            format!("pooled build refused: {error}")
        }
    })
}
