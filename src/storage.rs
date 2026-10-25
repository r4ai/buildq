//! Free-space admission and pressure checks for pooled Cargo builds.

use std::io;
use std::path::Path;

use crate::config::{CargoBackend, Config};
use crate::util::{free_disk_bytes, human_bytes};

pub fn check(cfg: &Config) -> io::Result<()> {
    if cfg.min_free == 0 {
        return Ok(());
    }
    check_cache_volume(cfg)?;
    check_mbx_volume(cfg)
}

fn check_cache_volume(cfg: &Config) -> io::Result<()> {
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

/// With `backend = "mbx"`, the reserve also holds on the volume of mbx's
/// store (last measured, `mbx::cached_store`) when it is not the cache
/// volume. buildq cannot free space there, so only mbx's GC helps.
fn check_mbx_volume(cfg: &Config) -> io::Result<()> {
    if cfg.backend != CargoBackend::Mbx {
        return Ok(());
    }
    let Some(store) = crate::mbx::cached_store(cfg) else {
        return Ok(());
    };
    if same_volume(&store, &cfg.cache_dir) {
        return Ok(());
    }
    match free_disk_bytes(&store) {
        Some(free) if free <= cfg.min_free => Err(io::Error::other(format!(
            "mbx store volume ({}) has {} free; min_free requires more than {}. \
             buildq never deletes mbx entries: run mbx's GC, tighten its limits, \
             or lower [cache].min_free",
            store.display(),
            human_bytes(free),
            human_bytes(cfg.min_free)
        ))),
        _ => Ok(()),
    }
}

/// Whether two paths (or their nearest existing ancestors) share a device.
/// Assumed true where that cannot be told.
fn same_volume(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let dev = |p: &Path| {
            p.ancestors()
                .find_map(|p| p.metadata().ok())
                .map(|m| m.dev())
        };
        match (dev(a), dev(b)) {
            (Some(x), Some(y)) => x == y,
            _ => true,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        true
    }
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
