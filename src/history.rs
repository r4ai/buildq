//! Append-only job history (`<queue dir>/history.jsonl`), rotated at 8 MiB.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::lock;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Entry {
    pub pid: u32,
    pub class: String,
    pub cmd: String,
    pub cwd: String,
    pub enqueued_ms: u64,
    pub started_ms: u64,
    pub ended_ms: u64,
    pub exit: i32,
    /// Pool member (`<pool>/m<i>`) or host.
    pub target: Option<String>,
    /// Explicit target dir the job used outside the pool.
    pub external: Option<String>,
    /// Packages invalidated in the member before the job.
    pub invalidated: usize,
    pub fresh_member: bool,
    pub label: Option<String>,
}

const MAX: u64 = 8 << 20;

pub fn append(queue_dir: &Path, e: &Entry) {
    let _ = fs::create_dir_all(queue_dir);
    let Ok(_g) = lock::lock(&queue_dir.join("history.lock")) else {
        return;
    };
    let path = queue_dir.join("history.jsonl");
    if fs::metadata(&path).map(|m| m.len() > MAX).unwrap_or(false) {
        let _ = fs::rename(&path, queue_dir.join("history.1.jsonl"));
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path)
        && let Ok(line) = serde_json::to_string(e)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// The last `n` entries, oldest first.
pub fn read(queue_dir: &Path, n: usize) -> Vec<Entry> {
    let mut out = Vec::new();
    for name in ["history.1.jsonl", "history.jsonl"] {
        let Ok(f) = fs::File::open(queue_dir.join(name)) else {
            continue;
        };
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            if let Ok(e) = serde_json::from_str::<Entry>(&line) {
                out.push(e);
            }
        }
    }
    let skip = out.len().saturating_sub(n);
    out.split_off(skip)
}
