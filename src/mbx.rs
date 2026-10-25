//! mbx's own storage next to buildq's pool (`backend = "mbx"`). buildq never
//! deletes mbx's entries, but the bytes mbx holds beyond what it shares with
//! the pool count against `cache.budget`, so GC leaves room for them by
//! evicting pool members. The `min_free` reserve also guards the store's
//! volume when it differs from the cache volume. mbx's own GC limits stay the
//! user's (mbx's config or `MBX_GC_*`).

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::{CacheAccounting, Config};
use crate::util::{self, human_bytes};

/// How long `mbx cache stats` may run before buildq gives up on it.
const STATS_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a measurement (or a failure to get one) is reused by GC and
/// admission. `cache ls` always measures afresh.
pub const CACHE_TTL_MS: u64 = 5 * 60_000;

/// The fields of `mbx cache stats --json` that buildq reads (version 1);
/// everything else is ignored. mbx counts logical bytes.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Stats {
    store: PathBuf,
    total_bytes: u64,
    target_bytes: u64,
    incremental_bytes: u64,
    generated_bytes: u64,
    combined_total_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MbxUsage {
    /// mbx's action-result store.
    pub store: PathBuf,
    /// mbx's byte counts, in mbx's logical bytes (not `accounting`).
    pub store_bytes: u64,
    pub target_bytes: u64,
    pub incremental_bytes: u64,
    pub generated_bytes: u64,
    pub combined_bytes: u64,
    /// Pool bytes (buildq's accounting) whose inode is also linked from the store.
    pub shared_with_mbx_bytes: u64,
    /// buildq total + mbx combined - shared; mixes mbx's logical bytes with
    /// buildq's accounting mode, so it is an estimate under `allocated`.
    pub combined_unique_bytes: u64,
}

impl MbxUsage {
    /// mbx's bytes that the pool does not already count: what the budget
    /// reserves for mbx.
    pub fn counted_bytes(&self) -> u64 {
        self.combined_bytes
            .saturating_sub(self.shared_with_mbx_bytes)
    }
}

/// The last measurement, shared by every buildq process (`mbx-usage.json`).
#[derive(Debug, Serialize, Deserialize)]
struct Cached {
    at_ms: u64,
    usage: Result<MbxUsage, String>,
}

fn cache_path(cfg: &Config) -> PathBuf {
    cfg.cache_dir.join("mbx-usage.json")
}

/// mbx's usage for a pool of `pool_total` bytes in `members`, reusing a
/// measurement younger than `max_age_ms`. Failures are cached too, so a
/// broken mbx is not spawned on every scan.
pub fn usage(
    cfg: &Config,
    members: &[&Path],
    pool_total: u64,
    max_age_ms: u64,
) -> Result<MbxUsage, String> {
    let path = cache_path(cfg);
    let cached = util::read_json::<Cached>(&path)
        .filter(|c| util::now_ms().saturating_sub(c.at_ms) < max_age_ms);
    let usage = match cached {
        Some(c) => c.usage,
        None => {
            let usage = scan(cfg, members);
            let _ = util::write_json(
                &path,
                &Cached {
                    at_ms: util::now_ms(),
                    usage: usage.clone(),
                },
            );
            usage
        }
    };
    usage.map(|mut m| {
        m.combined_unique_bytes =
            (pool_total + m.combined_bytes).saturating_sub(m.shared_with_mbx_bytes);
        m
    })
}

/// The store of the last successful measurement, without running mbx.
pub fn cached_store(cfg: &Config) -> Option<PathBuf> {
    util::read_json::<Cached>(&cache_path(cfg))?
        .usage
        .ok()
        .map(|m| m.store)
}

/// Runs `mbx cache stats --json` and measures what the pool shares with the
/// store. Errors become a note, never a failure. `combined_unique_bytes` is
/// left for `usage` to fill.
fn scan(cfg: &Config, members: &[&Path]) -> Result<MbxUsage, String> {
    let mbx = crate::job::find_mbx(cfg)?;
    let stats = parse_stats(&run_stats(&mbx)?)?;
    let shared = shared_bytes(&stats.store, members, cfg.accounting);
    Ok(MbxUsage {
        combined_unique_bytes: 0,
        store: stats.store,
        store_bytes: stats.total_bytes,
        target_bytes: stats.target_bytes,
        incremental_bytes: stats.incremental_bytes,
        generated_bytes: stats.generated_bytes,
        combined_bytes: stats.combined_total_bytes,
        shared_with_mbx_bytes: shared,
    })
}

fn parse_stats(json: &[u8]) -> Result<Stats, String> {
    serde_json::from_slice(json).map_err(|e| format!("unreadable `mbx cache stats --json`: {e}"))
}

fn run_stats(mbx: &Path) -> Result<Vec<u8>, String> {
    let mut child = Command::new(mbx)
        .args(["cache", "stats", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", mbx.display()))?;
    let drain = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf);
            buf
        })
    };
    let out = drain(Box::new(child.stdout.take().unwrap()));
    let err = drain(Box::new(child.stderr.take().unwrap()));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() < STATS_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("`mbx cache stats --json` timed out".into());
            }
            Err(e) => return Err(format!("`mbx cache stats --json` failed: {e}")),
        }
    };
    let out = out.join().unwrap_or_default();
    if !status.success() {
        let err = String::from_utf8_lossy(&err.join().unwrap_or_default()).into_owned();
        return Err(format!(
            "`mbx cache stats --json` exited with {status}: {}",
            err.lines().next().unwrap_or("").trim()
        ));
    }
    Ok(out)
}

/// Bytes of the pool members' files that are hard links to files in `store`,
/// each inode once. Only hard links on one filesystem are seen; a reflink
/// shares blocks but not the inode, so it counts as 0 here. Other
/// filesystems also give 0.
#[cfg(unix)]
pub fn shared_bytes(store: &Path, members: &[&Path], mode: CacheAccounting) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let mut linked = BTreeSet::new();
    util::walk(store, &mut |_, md| {
        if md.nlink() > 1 {
            linked.insert((md.dev(), md.ino()));
        }
    });
    if linked.is_empty() {
        return 0;
    }
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    for m in members {
        util::walk(m, &mut |p, md| {
            if !linked.contains(&(md.dev(), md.ino())) {
                return;
            }
            if let Ok((id, bytes)) = util::file_footprint(p, md, mode)
                && seen.insert(id)
            {
                total = total.saturating_add(bytes);
            }
        });
    }
    total
}

#[cfg(not(unix))]
pub fn shared_bytes(_: &Path, _: &[&Path], _: CacheAccounting) -> u64 {
    0
}

/// The "mbx store" block of `cache ls`.
pub fn render(m: &MbxUsage, pool_total: u64, accounting: CacheAccounting) -> String {
    format!(
        "mbx store {}: {} results, {} targets, {} incremental, {} generated \
         (mbx logical bytes, combined {})\n\
         shared with the pool (hard links): {}\n\
         combined unique: {} = pool {} [{} accounting] + mbx {} - shared\n",
        m.store.display(),
        human_bytes(m.store_bytes),
        human_bytes(m.target_bytes),
        human_bytes(m.incremental_bytes),
        human_bytes(m.generated_bytes),
        human_bytes(m.combined_bytes),
        human_bytes(m.shared_with_mbx_bytes),
        human_bytes(m.combined_unique_bytes),
        human_bytes(pool_total),
        accounting.name(),
        human_bytes(m.combined_bytes),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_ignore_unknown_and_missing_fields() {
        let s = parse_stats(
            br#"{"version":1,"store":"/x/actions","total_bytes":5,"combined_total_bytes":9,"extra":[1]}"#,
        )
        .unwrap();
        assert_eq!(s.store, Path::new("/x/actions"));
        assert_eq!(
            (s.total_bytes, s.combined_total_bytes, s.target_bytes),
            (5, 9, 0)
        );
        assert!(parse_stats(b"nope").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn shared_bytes_counts_hard_linked_inodes_once() {
        let d = tempfile::tempdir().unwrap();
        let store = d.path().join("store");
        let (m0, m1) = (d.path().join("m0"), d.path().join("m1"));
        for dir in [&store, &m0, &m1] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(store.join("a"), vec![1; 100]).unwrap();
        std::fs::write(store.join("b"), vec![2; 50]).unwrap(); // not linked
        std::fs::hard_link(store.join("a"), m0.join("a")).unwrap();
        std::fs::hard_link(store.join("a"), m1.join("a2")).unwrap();
        std::fs::write(m0.join("own"), vec![3; 7]).unwrap();
        let shared = shared_bytes(&store, &[&m0, &m1], CacheAccounting::Logical);
        assert_eq!(shared, 100);
        let none = d.path().join("none");
        assert_eq!(shared_bytes(&none, &[&m0], CacheAccounting::Logical), 0);
    }
}
