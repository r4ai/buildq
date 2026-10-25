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
/// path dependency inside one is compared by content like any other package;
/// None if one of them cannot be read.
pub fn manifest(wt: &Worktree) -> Option<crate::pool::Manifest> {
    manifest_in(&wt.root)
}

fn manifest_in(root: &Path) -> Option<crate::pool::Manifest> {
    // ls-files and status are independent: run them side by side.
    let status_root = root.to_path_buf();
    let status = std::thread::spawn(move || {
        let mut c = git(&status_root);
        c.args(["status", "--porcelain", "-z", "--untracked-files=all"]);
        output(c).map(|s| parse_porcelain_z(&s))
    });
    let mut c = git(root);
    c.args(["ls-files", "-s", "-z", "--debug"]);
    let listing = output(c)?;
    let mut m = crate::pool::Manifest::new();
    let mut submodules = Vec::new();
    // Git's stat cache compares whole seconds, so a same-size edit that keeps
    // the old mtime within the cached ctime's second looks clean to `status`.
    let mut stale = std::collections::BTreeSet::new();
    for (entry, cached) in parse_ls_files_debug(&listing) {
        if let Some((meta, path)) = entry.split_once('\t') {
            let mut meta = meta.split(' ');
            let mode = meta.next().unwrap_or("");
            let blob = meta.next().unwrap_or("").to_string();
            if mode == "160000" {
                submodules.push(path.to_string());
            }
            if matches!(mode, "100644" | "100755")
                && cached.is_some_and(|c| stat_differs(&root.join(path), &c))
            {
                stale.insert(path.to_string());
            }
            m.insert(path.to_string(), blob);
        }
    }
    let dirty = status.join().ok()??;
    for p in dirty {
        if root.join(&p).is_file() {
            stale.insert(p);
        } else {
            m.remove(&p);
        }
    }
    let to_hash: Vec<String> = stale.into_iter().collect();
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
    Some(m)
}

/// The stat data the index caches for an entry, as `ls-files --debug` prints
/// it. Git stores 32-bit values, so the live ones are truncated to compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CachedStat {
    ctime: (u32, u32),
    mtime: (u32, u32),
    ino: u32,
    size: u32,
}

/// Splits `ls-files -s -z --debug` into "<mode> <blob> <stage>\t<path>"
/// entries, each with the stat block that follows its NUL-terminated path.
fn parse_ls_files_debug(s: &str) -> Vec<(&str, Option<CachedStat>)> {
    const STAT_LINES: usize = 5;
    let mut out: Vec<(&str, Option<CachedStat>)> = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let (head, tail) = rest.split_once('\0').unwrap_or((rest, ""));
        let mut end = 0;
        for _ in 0..STAT_LINES {
            match head[end..].find('\n') {
                Some(i) => end += i + 1,
                None => break,
            }
        }
        // The first chunk has no stat block in front of it.
        let (block, entry) = if out.is_empty() {
            ("", head)
        } else {
            head.split_at(end)
        };
        if let Some(last) = out.last_mut() {
            last.1 = parse_stat_block(block);
        }
        if !entry.is_empty() {
            out.push((entry, None));
        }
        rest = tail;
    }
    out
}

fn parse_stat_block(block: &str) -> Option<CachedStat> {
    let mut fields = std::collections::HashMap::new();
    for part in block.split(['\n', '\t']) {
        if let Some((k, v)) = part.trim().split_once(": ") {
            fields.insert(k, v.trim());
        }
    }
    let time = |k: &str| -> Option<(u32, u32)> {
        let (s, ns) = fields.get(k)?.split_once(':')?;
        Some((s.parse().ok()?, ns.parse().ok()?))
    };
    let num = |k: &str| -> Option<u32> { fields.get(k)?.parse().ok() };
    Some(CachedStat {
        ctime: time("ctime")?,
        mtime: time("mtime")?,
        ino: num("ino")?,
        size: num("size")?,
    })
}

/// Whether the file's stat no longer matches what the index cached, at full
/// precision. Nanoseconds count only when the index recorded them.
#[cfg(unix)]
fn stat_differs(path: &Path, cached: &CachedStat) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(md) = fs::symlink_metadata(path) else {
        return false;
    };
    let with_ns = cached.ctime.1 != 0 || cached.mtime.1 != 0;
    let time = |s: i64, ns: i64| (s as u32, if with_ns { ns as u32 } else { 0 });
    md.is_file()
        && (time(md.ctime(), md.ctime_nsec()) != cached.ctime
            || time(md.mtime(), md.mtime_nsec()) != cached.mtime
            || md.ino() as u32 != cached.ino
            || md.size() as u32 != cached.size)
}

/// Windows has no ctime or inode to catch an edit Git's stat cache misses.
#[cfg(not(unix))]
fn stat_differs(_path: &Path, _cached: &CachedStat) -> bool {
    false
}

/// `git hash-object` for several worktree files at once.
pub fn hash_objects(wt: &Worktree, paths: &[String]) -> Option<Vec<(String, String)>> {
    hash_objects_in(&wt.root, paths)
}

fn hash_objects_in(root: &Path, paths: &[String]) -> Option<Vec<(String, String)>> {
    if paths.is_empty() {
        return Some(Vec::new());
    }
    let mut child = git(root)
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

    #[test]
    fn ls_files_debug_parse() {
        let block = |n: u32| {
            format!(
                "  ctime: {n}:5\n  mtime: {n}:6\n  dev: 1\tino: {n}\n  uid: 1\tgid: 1\n  size: {n}\tflags: 0\n"
            )
        };
        let s = format!(
            "100644 aa 0\ta.rs\0{}100755 bb 0\tline\nbreak.rs\0{}",
            block(1),
            block(2)
        );
        let stat = |n| CachedStat {
            ctime: (n, 5),
            mtime: (n, 6),
            ino: n,
            size: n,
        };
        assert_eq!(
            parse_ls_files_debug(&s),
            [
                ("100644 aa 0\ta.rs", Some(stat(1))),
                ("100755 bb 0\tline\nbreak.rs", Some(stat(2))),
            ]
        );
    }

    /// `checkStat=minimal` makes Git's false-clean result deterministic: it
    /// compares only mtime seconds and size, like the same-second race does.
    #[cfg(unix)]
    #[test]
    fn manifest_hashes_a_backdated_edit_git_status_calls_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let root = canonical(tmp.path());
        let run = |args: &[&str]| {
            let out = git(&root)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["init", "-q"]);
        run(&["config", "core.checkStat", "minimal"]);
        run(&["config", "core.trustctime", "false"]);
        let file = root.join("data.txt");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
        let write = |text: &str| {
            fs::write(&file, text).unwrap();
            fs::File::options()
                .write(true)
                .open(&file)
                .unwrap()
                .set_modified(old)
                .unwrap();
        };
        write("ALPHA");
        run(&["add", "data.txt"]);
        run(&["commit", "-q", "-m", "init"]);
        let alpha = run(&["rev-parse", "HEAD:data.txt"]);
        write("DELTA");
        let delta = run(&["hash-object", "data.txt"]);
        assert_ne!(alpha, delta);
        assert_eq!(
            run(&["status", "--porcelain"]),
            "",
            "git must call it clean"
        );
        let m = manifest_in(&root).unwrap();
        assert_eq!(m.get("data.txt"), Some(&delta));
    }
}
