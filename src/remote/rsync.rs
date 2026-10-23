//! Rsync source mirroring and artifact retrieval over SSH.

use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;

use super::{Layout, Ssh, Transport, ok, s, sh_quote};
use crate::git::Worktree;

/// SSH execution with rsync source and artifact transfer. Remote Git metadata
/// is created on the host, so worktree identity and remote pooling still work.
pub struct Rsync {
    pub ssh: Ssh,
    pub exclude: Vec<String>,
}

impl Rsync {
    fn command(&self) -> Command {
        let mut cmd = Command::new("rsync");
        // Rsync parses this as a command line; quote each SSH argument once.
        let shell = std::iter::once("ssh".to_string())
            .chain(self.ssh.args.iter().map(|a| sh_quote(a)))
            .collect::<Vec<_>>()
            .join(" ");
        cmd.args(["--archive", "--rsh", &shell]);
        for pattern in &self.exclude {
            cmd.arg(format!("--exclude={pattern}"));
        }
        // We quote the remote path for its shell ourselves. Keep that same
        // behavior with both macOS's older rsync and newer rsync releases.
        cmd.env("RSYNC_OLD_ARGS", "1")
            .env("RSYNC_PROTECT_ARGS", "0");
        cmd
    }

    fn remote_path(&self, path: &str) -> String {
        format!("{}:{}", self.ssh.dest, sh_quote(path))
    }
}

impl Transport for Rsync {
    fn sync(&self, _: &str, wt: &Worktree, l: &Layout, _: &Path) -> Result<String, String> {
        if cfg!(windows) {
            return Err(
                "rsync transport requires a POSIX client; use WSL or transport = git".into(),
            );
        }
        ok(
            self.exec(
                None,
                &[],
                &s(&["git", "init", "--bare", "-q", &l.repo_dir]),
                true,
            ),
            "remote rsync repository",
        )?;
        let exists =
            self.exec(
                None,
                &[],
                &s(&["test", "-e", &format!("{}/.git", l.wt_dir)]),
                true,
            )
            .map_err(|e| e.to_string())?
            .0 == 0;
        if !exists {
            let tree = ok(
                self.exec(
                    None,
                    &[],
                    &s(&["git", "--git-dir", &l.repo_dir, "mktree"]),
                    true,
                ),
                "remote empty tree",
            )?;
            let sha = remote_commit(self, &l.repo_dir, tree.trim())?;
            ok(
                self.exec(
                    None,
                    &[],
                    &s(&[
                        "git",
                        "--git-dir",
                        &l.repo_dir,
                        "worktree",
                        "add",
                        "-f",
                        "--detach",
                        &l.wt_dir,
                        &sha,
                    ]),
                    true,
                ),
                "remote rsync worktree",
            )?;
        }
        let mut cmd = self.command();
        cmd.args([
            "--checksum",
            "--delete",
            "--exclude=/.git",
            "--exclude=/target/",
            "--",
        ])
        .arg(format!("{}/", wt.root.display()))
        .arg(self.remote_path(&format!("{}/", l.wt_dir)));
        let code = crate::exec::run(&mut cmd, true).map_err(|e| format!("rsync sources: {e}"))?;
        if code != 0 {
            return Err(format!("rsync sources: exit {code}"));
        }
        // Include transferred generated files in the remote content manifest.
        // Keep target artifacts outside the index and the transfer entirely.
        ok(
            self.exec(
                Some(&l.wt_dir),
                &[],
                &s(&[
                    "git",
                    "add",
                    "--force",
                    "-A",
                    "--",
                    ".",
                    ":(top,exclude)target",
                ]),
                true,
            ),
            "remote rsync index",
        )?;
        // Rsync preserves source mtimes, whose precision may be coarser than
        // the compiler's output timestamps. Mark only content changes newer
        // than the last build, including generated inputs with old mtimes.
        let changed = ok(
            self.exec(
                Some(&l.wt_dir),
                &[],
                &s(&[
                    "git",
                    "diff",
                    "--cached",
                    "--name-only",
                    "--diff-filter=ACMR",
                    "-z",
                ]),
                true,
            ),
            "remote rsync changes",
        )?;
        let paths: Vec<_> = changed.split('\0').filter(|p| !p.is_empty()).collect();
        for batch in paths.chunks(128) {
            let args = ["touch", "-h", "--"]
                .into_iter()
                .chain(batch.iter().copied())
                .map(str::to_string)
                .collect::<Vec<_>>();
            ok(
                self.exec(Some(&l.wt_dir), &[], &args, true),
                "remote rsync source timestamps",
            )?;
        }
        let tree = ok(
            self.exec(Some(&l.wt_dir), &[], &s(&["git", "write-tree"]), true),
            "remote rsync tree",
        )?;
        let sha = remote_commit(self, &l.repo_dir, tree.trim())?;
        ok(
            self.exec(
                Some(&l.wt_dir),
                &[],
                &s(&["git", "reset", "--mixed", "-q", &sha]),
                true,
            ),
            "remote rsync snapshot",
        )?;
        Ok(sha)
    }

    fn exec(
        &self,
        cwd: Option<&str>,
        env: &[(String, String)],
        argv: &[String],
        capture: bool,
    ) -> io::Result<(i32, String)> {
        self.ssh.exec(cwd, env, argv, capture)
    }
    fn probe(&self, env: &[(String, String)], argv: &[String]) -> io::Result<(i32, String)> {
        self.ssh.probe(env, argv)
    }
    fn git_url(&self, path: &str) -> String {
        self.ssh.git_url(path)
    }
    fn fetch(&self, remote: &str, into: &Path) -> io::Result<()> {
        fs::create_dir_all(into)?;
        let code = crate::exec::run(
            self.command()
                .arg("--checksum")
                .arg("--")
                .arg(self.remote_path(remote))
                .arg(into),
            true,
        )?;
        if code == 0 {
            Ok(())
        } else {
            Err(io::Error::other(format!("rsync fetch: exit {code}")))
        }
    }
}

fn remote_commit(t: &dyn Transport, repo: &str, tree: &str) -> Result<String, String> {
    ok(
        t.exec(
            None,
            &[],
            &s(&[
                "git",
                "--git-dir",
                repo,
                "-c",
                "user.name=buildq",
                "-c",
                "user.email=buildq@localhost",
                "commit-tree",
                tree,
                "-m",
                "buildq rsync snapshot",
            ]),
            true,
        ),
        "remote rsync commit",
    )
    .map(|sha| sha.trim().to_string())
}
