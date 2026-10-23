//! The remote protocol against a transport that treats a local directory as
//! the remote home: same git push / worktree / run / fetch steps, no ssh.

mod common;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use buildq::config::HostConfig;
use buildq::remote::{self, RemoteJob, Transport};

struct LocalHost {
    home: PathBuf,
    log: std::cell::RefCell<Vec<String>>,
}

impl LocalHost {
    fn path(&self, p: &str) -> PathBuf {
        if Path::new(p).is_absolute() {
            PathBuf::from(p)
        } else {
            self.home.join(p)
        }
    }
}

impl Transport for LocalHost {
    fn exec(
        &self,
        cwd: Option<&str>,
        env: &[(String, String)],
        argv: &[String],
        capture: bool,
    ) -> io::Result<(i32, String)> {
        self.log
            .borrow_mut()
            .push(remote::remote_script(cwd, env, argv));
        // `test -e` is a POSIX utility the remote would have; Windows has none
        // on PATH (git's bundled one is not guaranteed), so answer it here.
        if let [t, e, path] = argv
            && t == "test"
            && e == "-e"
        {
            let base = cwd
                .map(|d| self.path(d))
                .unwrap_or_else(|| self.home.clone());
            return Ok((if base.join(path).exists() { 0 } else { 1 }, String::new()));
        }
        let mut c = common::command(&argv[0]);
        c.args(&argv[1..]).current_dir(
            cwd.map(|d| self.path(d))
                .unwrap_or_else(|| self.home.clone()),
        );
        for (k, v) in env {
            c.env(k, v);
        }
        let out = c.stdin(Stdio::null()).stderr(Stdio::null()).output()?;
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        if !capture {
            print!("{stdout}");
        }
        Ok((out.status.code().unwrap_or(1), stdout))
    }

    fn git_url(&self, path: &str) -> String {
        self.path(path).display().to_string()
    }

    fn fetch(&self, remote: &str, into: &Path) -> io::Result<()> {
        let src = self.path(remote);
        fs::create_dir_all(into)?;
        fs::copy(&src, into.join(src.file_name().unwrap())).map(|_| ())
    }
}

fn git(dir: &Path, args: &[&str]) {
    let st = common::command("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "core.autocrlf=false",
        ])
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(st.success(), "git {args:?}");
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn sync_run_fetch_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let root = buildq::util::canonical(tmp.path());
    let repo = root.join("work");
    fs::create_dir_all(repo.join("sub")).unwrap();
    fs::write(repo.join("tracked.txt"), "v1").unwrap();
    fs::write(repo.join("sub/keep.txt"), "k").unwrap();
    fs::write(repo.join("gone.txt"), "x").unwrap();
    fs::write(repo.join(".gitignore"), "ignored.txt\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    // Uncommitted work must travel too; ignored files must not.
    fs::write(repo.join("tracked.txt"), "v2-dirty").unwrap();
    fs::write(repo.join("untracked.txt"), "new").unwrap();
    fs::write(repo.join("ignored.txt"), "secret").unwrap();
    fs::remove_file(repo.join("gone.txt")).unwrap();
    let head_before = fs::read_to_string(repo.join(".git/HEAD")).unwrap();

    let home = root.join("remote-home");
    fs::create_dir_all(&home).unwrap();
    let host = LocalHost {
        home: home.clone(),
        log: Default::default(),
    };
    let wt = buildq::git::discover(&repo).unwrap();
    let hc = HostConfig {
        root: Some(".buildq".into()),
        ..Default::default()
    };
    let l = remote::layout(".buildq", &wt);

    let job = RemoteJob {
        host: "box",
        cfg: &hc,
        argv: &s(&["git", "archive", "-o", "snap.tar", "HEAD"]),
        fetch: &s(&["snap.tar"]),
        fetch_into: root.join("fetched"),
    };
    let code = remote::run_on(&host, &job, &wt, &repo, &root.join("scratch")).unwrap();
    assert_eq!(code, 0);
    let rwt = home.join(&l.wt_dir);
    assert_eq!(
        fs::read_to_string(rwt.join("tracked.txt")).unwrap(),
        "v2-dirty"
    );
    assert_eq!(
        fs::read_to_string(rwt.join("untracked.txt")).unwrap(),
        "new"
    );
    assert!(!rwt.join("ignored.txt").exists());
    assert!(!rwt.join("gone.txt").exists());
    assert!(root.join("fetched/snap.tar").is_file(), "fetched artifact");
    // The local repo is untouched (HEAD, index, status).
    assert_eq!(
        fs::read_to_string(repo.join(".git/HEAD")).unwrap(),
        head_before
    );
    let st = common::command("git")
        .args(["status", "--porcelain"])
        .current_dir(&repo)
        .output()
        .unwrap();
    let st = String::from_utf8_lossy(&st.stdout);
    assert!(
        st.contains(" M tracked.txt") && st.contains("?? untracked.txt"),
        "{st}"
    );

    // Second round: an edit and a remote exit code that must come back.
    fs::write(repo.join("tracked.txt"), "v3").unwrap();
    let job = RemoteJob {
        argv: &s(&["git", "cat-file", "-e", "HEAD:no-such-file"]),
        fetch: &[],
        ..job
    };
    let code = remote::run_on(&host, &job, &wt, &repo.join("sub"), &root.join("scratch")).unwrap();
    assert_ne!(code, 0, "remote failure must surface");
    assert_eq!(fs::read_to_string(rwt.join("tracked.txt")).unwrap(), "v3");
    // The job ran in the same subdirectory as locally.
    let log = host.log.borrow();
    assert!(
        log.last()
            .unwrap()
            .starts_with(&format!("cd {}/sub && ", l.wt_dir)),
        "{log:?}"
    );
    // Second sync reused the remote worktree instead of adding a new one.
    assert_eq!(log.iter().filter(|c| c.contains("worktree add")).count(), 1);
}
