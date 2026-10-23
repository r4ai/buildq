//! Target dirs observed outside the pool: explicit (`--target-dir`,
//! `CARGO_TARGET_DIR`) and discovered helper-workspace defaults. The budget cannot
//! cover them; buildq records them in `<cache dir>/external.json` so that
//! `cache ls` and `gc` can show how much disk they hold. They are never
//! deleted: these paths belong to the project or caller.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::lock;
use crate::util::{dir_size, norm_key, now_ms, read_json, write_json};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ExternalDir {
    pub path: PathBuf,
    pub last_used_ms: u64,
    /// Working directory of the last job that used it.
    pub last_cwd: String,
    pub jobs: u64,
    pub size_bytes: u64,
    pub size_ms: u64,
}

type Registry = BTreeMap<String, ExternalDir>;

fn registry_path(cfg: &Config) -> PathBuf {
    cfg.cache_dir.join("external.json")
}

fn with_registry<T>(cfg: &Config, f: impl FnOnce(&mut Registry) -> T) -> Option<T> {
    fs::create_dir_all(&cfg.cache_dir).ok()?;
    let _g = lock::lock(&cfg.cache_dir.join("external.lock")).ok()?;
    let path = registry_path(cfg);
    let mut reg: Registry = read_json(&path).unwrap_or_default();
    let out = f(&mut reg);
    let _ = write_json(&path, &reg);
    Some(out)
}

/// Notes that a job just used `dir` outside the pool.
pub fn record(cfg: &Config, dir: &Path, cwd: &str) {
    with_registry(cfg, |reg| {
        let e = reg.entry(norm_key(dir)).or_default();
        e.path = dir.to_path_buf();
        e.last_used_ms = now_ms();
        e.last_cwd = cwd.to_string();
        e.jobs += 1;
    });
}

/// Recorded dirs that still exist, largest first. Sizes are re-measured
/// when `refresh`, or when the dir was used after its last measurement;
/// dirs that are gone are forgotten.
pub fn scan(cfg: &Config, refresh: bool) -> Vec<ExternalDir> {
    if !registry_path(cfg).exists() {
        return Vec::new();
    }
    let stale: Vec<ExternalDir> = with_registry(cfg, |reg| {
        reg.retain(|_, e| e.path.is_dir());
        reg.values()
            .filter(|e| refresh || e.size_ms == 0 || e.size_ms < e.last_used_ms)
            .cloned()
            .collect()
    })
    .unwrap_or_default();
    // Measured without the lock: a big dir takes a while, and jobs record meanwhile.
    let measured: Vec<(String, u64, u64)> = stale
        .iter()
        .map(|e| (norm_key(&e.path), now_ms(), dir_size(&e.path)))
        .collect();
    let mut out = with_registry(cfg, |reg| {
        for (k, at, size) in measured {
            if let Some(e) = reg.get_mut(&k) {
                e.size_bytes = size;
                e.size_ms = at;
            }
        }
        reg.values().cloned().collect::<Vec<_>>()
    })
    .unwrap_or_default();
    out.sort_by_key(|e| std::cmp::Reverse(e.size_bytes));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FileConfig;

    #[test]
    fn records_measures_and_forgets() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Config::from_parts(
            dir.path().join("q"),
            dir.path().join("q/config.toml"),
            FileConfig::default(),
        );
        c.cache_dir = dir.path().join("cache");
        assert!(scan(&c, false).is_empty());

        let ext = dir.path().join("wt/target-base");
        fs::create_dir_all(ext.join("debug")).unwrap();
        fs::write(ext.join("debug/app"), vec![0u8; 1000]).unwrap();
        record(&c, &ext, "/wt");
        record(&c, &ext, "/wt");
        let gone = dir.path().join("gone");
        fs::create_dir_all(&gone).unwrap();
        record(&c, &gone, "/wt");
        fs::remove_dir_all(&gone).unwrap();

        let s = scan(&c, false);
        assert_eq!(s.len(), 1, "{s:?}");
        assert_eq!(s[0].path, ext);
        assert_eq!(s[0].jobs, 2);
        assert!(s[0].size_bytes >= 1000);
    }
}
