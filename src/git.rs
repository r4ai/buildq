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

/// Git for the content manifest. The Git LFS clean filter writes into
/// `.git/lfs/tmp`, which a sandbox may mount read-only; without it a modified
/// LFS file is hashed by its raw content, which still tells edits apart.
fn manifest_git(dir: &Path) -> Command {
    let mut c = git(dir);
    c.args([
        "-c",
        "filter.lfs.clean=",
        "-c",
        "filter.lfs.process=",
        "-c",
        "filter.lfs.required=false",
    ]);
    c
}

/// Stdout of a successful run, else what failed with Git's last error line.
fn checked(mut c: Command, what: &str) -> Result<String, String> {
    let dir = c
        .get_current_dir()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let fail = |e: &str| format!("git {what} failed in {}: {e}", dir.display());
    let out = c.output().map_err(|e| fail(&e.to_string()))?;
    if !out.status.success() {
        return Err(fail(last_line(&out.stderr)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn last_line(stderr: &[u8]) -> &str {
    std::str::from_utf8(stderr)
        .unwrap_or("")
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
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

/// Uncommitted paths: modified, staged, renamed (both names), deleted and
/// untracked, including those inside initialized submodules.
pub fn dirty(wt: &Worktree) -> Vec<String> {
    dirty_in(&wt.root)
}

fn dirty_in(root: &Path) -> Vec<String> {
    let mut c = git(root);
    c.args(["status", "--porcelain", "-z", "--untracked-files=all"]);
    let Some(s) = output(c) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for p in parse_porcelain_z(&s) {
        // A changed submodule is reported as one entry; list its files instead.
        if is_submodule_checkout(root, &p) {
            out.extend(
                dirty_in(&root.join(&p))
                    .into_iter()
                    .map(|f| format!("{p}/{f}")),
            );
        } else {
            out.push(p);
        }
    }
    out
}

/// An initialized submodule (or nested checkout) at a tracked path. Untracked
/// directories are reported with a trailing slash and are not descended into.
fn is_submodule_checkout(root: &Path, rel: &str) -> bool {
    !rel.ends_with('/') && root.join(rel).join(".git").exists()
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
/// are not included). Initialized submodules contribute their own files, so a
/// path dependency inside one is compared by content like any other package.
/// Fails with Git's reason if one of them cannot be read.
pub fn manifest(wt: &Worktree) -> Result<crate::pool::Manifest, String> {
    manifest_in(&wt.root)
}

fn manifest_in(root: &Path) -> Result<crate::pool::Manifest, String> {
    // ls-files and status are independent: run them side by side.
    let status_root = root.to_path_buf();
    let status = std::thread::spawn(move || {
        let mut c = manifest_git(&status_root);
        c.args(["status", "--porcelain", "-z", "--untracked-files=all"]);
        checked(c, "status").map(|s| parse_porcelain_z(&s))
    });
    let mut c = git(root);
    c.args(["ls-files", "-s", "-z"]);
    let listing = checked(c, "ls-files")?;
    let mut m = crate::pool::Manifest::new();
    let mut submodules = Vec::new();
    for entry in split_z(&listing) {
        // "<mode> <blob> <stage>\t<path>"
        if let Some((meta, path)) = entry.split_once('\t') {
            let mut meta = meta.split(' ');
            let mode = meta.next().unwrap_or("");
            let blob = meta.next().unwrap_or("").to_string();
            if mode == "160000" {
                submodules.push(path.to_string());
            }
            m.insert(path.to_string(), blob);
        }
    }
    let dirty = status
        .join()
        .map_err(|_| "git status: thread panicked".to_string())??;
    let mut to_hash = Vec::new();
    for p in dirty {
        if root.join(&p).is_file() {
            to_hash.push(p);
        } else {
            m.remove(&p);
        }
    }
    for (p, id) in hash_objects_in(root, &to_hash)? {
        m.insert(p, id);
    }
    // The gitlink only names a commit; an uninitialized submodule keeps it.
    for sub in submodules {
        if !is_submodule_checkout(root, &sub) {
            continue;
        }
        m.remove(&sub);
        for (p, id) in manifest_in(&root.join(&sub))? {
            m.insert(format!("{sub}/{p}"), id);
        }
    }
    Ok(m)
}

/// `git hash-object` for several worktree files at once.
pub fn hash_objects(wt: &Worktree, paths: &[String]) -> Result<Vec<(String, String)>, String> {
    hash_objects_in(&wt.root, paths)
}

fn hash_objects_in(root: &Path, paths: &[String]) -> Result<Vec<(String, String)>, String> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let fail =
        |e: &dyn std::fmt::Display| format!("git hash-object failed in {}: {e}", root.display());
    let mut child = manifest_git(root)
        .args(["hash-object", "--stdin-paths"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| fail(&e))?;
    let input = paths.join("\n") + "\n";
    let mut stdin = child.stdin.take().ok_or_else(|| fail(&"no stdin"))?;
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        let _ = stdin.write_all(input.as_bytes());
    });
    let out = child.wait_with_output().map_err(|e| fail(&e))?;
    let _ = writer.join();
    let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .collect();
    // A partial answer would leave some files at their stale index ids.
    if !out.status.success() || ids.len() != paths.len() {
        return Err(fail(&last_line(&out.stderr)));
    }
    Ok(paths.iter().cloned().zip(ids).collect())
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
