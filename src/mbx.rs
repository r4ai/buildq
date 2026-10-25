//! mbx's own storage next to buildq's pool (`backend = "mbx"`), reported by
//! `cache ls`. Informational only: GC decisions ignore it.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::{CacheAccounting, Config};
use crate::gc::Usage;
use crate::util::{self, human_bytes};

/// How long `mbx cache stats` may run before `cache ls` gives up on it.
const STATS_TIMEOUT: Duration = Duration::from_secs(30);

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

#[derive(Debug, Clone, Serialize)]
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

/// Runs `mbx cache stats --json` and measures what the pool shares with the
/// store. Errors become a note in `cache ls`, never a failure.
pub fn scan(cfg: &Config, u: &Usage) -> Result<MbxUsage, String> {
    let mbx = crate::job::find_mbx(cfg)?;
    let stats = parse_stats(&run_stats(&mbx)?)?;
    let members: Vec<&Path> = u
        .pools
        .iter()
        .flat_map(|p| &p.members)
        .map(|m| m.dir.as_path())
        .collect();
    let shared = shared_bytes(&stats.store, &members, cfg.accounting);
    Ok(MbxUsage {
        combined_unique_bytes: (u.total + stats.combined_total_bytes).saturating_sub(shared),
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
