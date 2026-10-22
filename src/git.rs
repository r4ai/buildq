//! The bits of git that buildq needs: worktree identity, a content manifest
//! of the working tree, and a snapshot commit for remote sync.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::util::{canonical, norm_key};

/// Where a build's sources come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// Top level of this worktree (git's spelling, forward slashes).
    pub root: PathBuf,
    /// The shared `.git` directory: identical for all worktrees of one repo.
    pub common_dir: PathBuf,
    /// HEAD commit, None in a repo without commits.
    pub head: Option<String>,
}

impl Worktree {
    /// Identity of the repository (all its worktrees share it).
    pub fn repo_key(&self) -> String {
        norm_key(&self.common_dir)
    }

    pub fn key(&self) -> String {
        norm_key(&self.root)
    }

    pub fn repo_name(&self) -> String {
        // `<repo>/.git` -> repo; `<repo>.git` (bare) -> repo.
        let c = &self.common_dir;
        let n = if c.file_name().map(|n| n == ".git").unwrap_or(false) {
            c.parent().and_then(|p| p.file_name())
        } else {
            c.file_stem()
        };
        n.map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".into())
    }
}

pub fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.current_dir(dir).stdin(Stdio::null());
    c
}

fn output(mut c: Command) -> Option<String> {
    let out = c.stderr(Stdio::null()).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

/// Finds the worktree containing `dir`.
pub fn discover(dir: &Path) -> Option<Worktree> {
    // One process in the common case; a repo without commits needs a second.
    let mut c = git(dir);
    c.args(["rev-parse", "--show-toplevel", "--git-common-dir", "HEAD"]);
    let (out, head) = match output(c) {
        Some(out) => {
            let head = out.lines().nth(2).map(|s| s.trim().to_string());
            (out, head)
        }
        None => {
            let mut c = git(dir);
            c.args(["rev-parse", "--show-toplevel", "--git-common-dir"]);
            (output(c)?, None)
        }
    };
    let mut lines = out.lines();
    let root = PathBuf::from(lines.next()?.trim());
    let common = PathBuf::from(lines.next()?.trim());
    let common = if common.is_absolute() {
        common
    } else {
        dir.join(common)
    };
    Some(Worktree {
        root,
        common_dir: canonical(&common),
        head: head.filter(|h| !h.is_empty()),
    })
}

fn split_z(s: &str) -> impl Iterator<Item = &str> {
    s.split('\0').filter(|p| !p.is_empty())
}

/// Uncommitted paths: modified, staged, renamed (both names), deleted and untracked.
pub fn dirty(wt: &Worktree) -> Vec<String> {
    let mut c = git(&wt.root);
    c.args(["status", "--porcelain", "-z", "--untracked-files=all"]);
    let Some(s) = output(c) else {
        return Vec::new();
    };
    parse_porcelain_z(&s)
}

pub fn parse_porcelain_z(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = s.split('\0').filter(|p| !p.is_empty());
    while let Some(entry) = it.next() {
        if entry.len() < 4 {
            continue;
        }
        let (xy, path) = entry.split_at(3);
        out.push(path.to_string());
        // Renames and copies are followed by the original name.
        if (xy.starts_with('R') || xy.starts_with('C'))
            && let Some(orig) = it.next()
        {
            out.push(orig.to_string());
        }
    }
    out
}

/// Content ids of every file in the working tree: index blob ids for clean
/// files, freshly hashed ids for modified and untracked ones (ignored files
/// are not included).
pub fn manifest(wt: &Worktree) -> Option<crate::pool::Manifest> {
    // ls-files and status are independent: run them side by side.
    let root = wt.root.clone();
    let status = std::thread::spawn(move || {
        let mut c = git(&root);
        c.args(["status", "--porcelain", "-z", "--untracked-files=all"]);
        output(c).map(|s| parse_porcelain_z(&s))
    });
    let mut c = git(&wt.root);
    c.args(["ls-files", "-s", "-z"]);
    let listing = output(c)?;
    let mut m = crate::pool::Manifest::new();
    for entry in split_z(&listing) {
        // "<mode> <blob> <stage>\t<path>"
        if let Some((meta, path)) = entry.split_once('\t') {
            let blob = meta.split(' ').nth(1).unwrap_or("").to_string();
            m.insert(path.to_string(), blob);
        }
    }
    let dirty = status.join().ok()??;
    let mut to_hash = Vec::new();
    for p in dirty {
        if wt.root.join(&p).is_file() {
            to_hash.push(p);
        } else {
            m.remove(&p);
        }
    }
    for (p, id) in hash_objects(wt, &to_hash)? {
        m.insert(p, id);
    }
    Some(m)
}

/// `git hash-object` for several worktree files at once.
pub fn hash_objects(wt: &Worktree, paths: &[String]) -> Option<Vec<(String, String)>> {
    if paths.is_empty() {
        return Some(Vec::new());
    }
    let mut child = git(&wt.root)
        .args(["hash-object", "--stdin-paths"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let input = paths.join("\n") + "\n";
    let mut stdin = child.stdin.take()?;
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        let _ = stdin.write_all(input.as_bytes());
    });
    let out = child.wait_with_output().ok()?;
    let _ = writer.join();
    let ids = String::from_utf8_lossy(&out.stdout);
    Some(
        paths
            .iter()
            .cloned()
            .zip(ids.lines().map(|l| l.trim().to_string()))
            .collect(),
    )
}

/// Commits the working tree (tracked + untracked, honouring .gitignore) as a
/// parentless commit without touching HEAD, the index or the files.
/// A copy of the real index keeps git's stat cache, so only changed files are hashed.
pub fn snapshot(wt: &Worktree, scratch: &Path) -> Result<String, String> {
    fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let tmp_index = scratch.join(format!("index-{}", std::process::id()));
    let mut c = git(&wt.root);
    c.args(["rev-parse", "--git-path", "index"]);
    if let Some(p) = output(c) {
        let p = PathBuf::from(p.trim());
        let p = if p.is_absolute() { p } else { wt.root.join(p) };
        let _ = fs::copy(&p, &tmp_index);
    }
    let run = |args: &[&str]| -> Result<String, String> {
        let out = git(&wt.root)
            .env("GIT_INDEX_FILE", &tmp_index)
            .env("GIT_AUTHOR_NAME", "buildq")
            .env("GIT_AUTHOR_EMAIL", "buildq@localhost")
            .env("GIT_COMMITTER_NAME", "buildq")
            .env("GIT_COMMITTER_EMAIL", "buildq@localhost")
            .args(args)
            .output()
            .map_err(|e| format!("git: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let result = (|| {
        run(&["add", "-A", "."])?;
        let tree = run(&["write-tree"])?;
        run(&["commit-tree", &tree, "-m", "buildq snapshot"])
    })();
    let _ = fs::remove_file(&tmp_index);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_parse() {
        let s = " M src/a.rs\0R  new.rs\0old.rs\0?? notes/x.txt\0";
        assert_eq!(
            parse_porcelain_z(s),
            ["src/a.rs", "new.rs", "old.rs", "notes/x.txt"]
        );
    }
}
