//! `buildq run --host` through a real OpenSSH server: a private `sshd` on
//! 127.0.0.1 with throwaway keys stands in for a remote machine, so the git
//! push, remote worktree, command execution, exit code and `--fetch` steps
//! all cross real ssh/scp (and rsync) instead of the local stand-ins of the
//! other remote tests (#4).
//!
//! ```text
//! cargo test --test ssh -- --ignored --nocapture
//! ```
//!
//! Needs `sshd`, `ssh`, `ssh-keygen`, `scp` and `rsync`; the test prints a
//! note and passes when any is missing.
#![cfg(unix)]

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BQ: &str = env!("CARGO_BIN_EXE_buildq");

fn git(repo: &Path, args: &[&str]) {
    let st = Command::new("git")
        .current_dir(repo)
        .args(["-c", "user.name=test", "-c", "user.email=test@localhost"])
        .args(args)
        .status()
        .unwrap();
    assert!(st.success(), "git {args:?}");
}

fn find(name: &str) -> Option<PathBuf> {
    let dirs = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    dirs.into_iter()
        .chain([PathBuf::from("/usr/sbin"), PathBuf::from("/usr/local/sbin")])
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// A private sshd, killed on drop.
struct Sshd {
    child: Child,
    port: u16,
    key: PathBuf,
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Sshd {
    fn start(sshd: &Path, dir: &Path) -> Sshd {
        let keygen = |path: &Path| {
            let st = Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(path)
                .status()
                .unwrap();
            assert!(st.success());
        };
        let host_key = dir.join("host_key");
        let key = dir.join("client_key");
        keygen(&host_key);
        keygen(&key);
        let authorized = dir.join("authorized_keys");
        fs::copy(key.with_extension("pub"), &authorized).unwrap();
        fs::set_permissions(&authorized, fs::Permissions::from_mode(0o600)).unwrap();
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let config = dir.join("sshd_config");
        fs::write(
            &config,
            format!(
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}\n\
                 PidFile {}\nPasswordAuthentication no\nKbdInteractiveAuthentication no\n\
                 UsePAM no\nStrictModes no\nSubsystem sftp internal-sftp\n",
                host_key.display(),
                authorized.display(),
                dir.join("sshd.pid").display(),
            ),
        )
        .unwrap();
        // Privilege separation wants this directory when sshd runs as root.
        let _ = fs::create_dir_all("/run/sshd");
        let child = Command::new(sshd)
            .args(["-D", "-e", "-f"])
            .arg(&config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(dir.join("sshd.log")).unwrap())
            .spawn()
            .unwrap();
        let s = Sshd { child, port, key };
        let deadline = Instant::now() + Duration::from_secs(20);
        while Command::new("ssh")
            .args(s.args())
            .args(["127.0.0.1", "true"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|st| !st.success())
            .unwrap_or(true)
        {
            assert!(
                Instant::now() < deadline,
                "sshd did not come up: {}",
                fs::read_to_string(dir.join("sshd.log")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        s
    }

    fn args(&self) -> Vec<String> {
        [
            "-p",
            &self.port.to_string(),
            "-i",
            &self.key.display().to_string(),
            "-o",
            "BatchMode=yes",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "LogLevel=ERROR",
        ]
        .map(str::to_string)
        .to_vec()
    }
}

#[test]
#[ignore = "starts a private sshd; run with --ignored"]
fn run_on_a_real_ssh_host_with_git_and_rsync_transports() {
    let tools = ["sshd", "ssh", "ssh-keygen", "scp", "rsync"];
    let Some(sshd) = find("sshd") else {
        eprintln!("note: no sshd; skipping");
        return;
    };
    if let Some(missing) = tools[1..].iter().find(|t| find(t).is_none()) {
        eprintln!("note: no {missing}; skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = buildq::util::canonical(tmp.path());
    let server = Sshd::start(&sshd, &root);

    let repo = root.join("work tree");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("input.txt"), "from the local worktree\n").unwrap();
    fs::write(repo.join(".gitignore"), "/generated/\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "initial"]);
    // Uncommitted and untracked changes travel too.
    fs::write(repo.join("input.txt"), "edited, not committed\n").unwrap();
    fs::write(repo.join("untracked.txt"), "untracked\n").unwrap();
    // Ignored: only the rsync transport sends it.
    fs::create_dir_all(repo.join("generated")).unwrap();
    fs::write(repo.join("generated/message"), "ignored input\n").unwrap();

    let remote_root = root.join("remote home/.buildq");
    let config = root.join("config.toml");
    let write_config = |transport: &str| {
        fs::write(
            &config,
            format!(
                "[hosts.box]\ntransport = '{transport}'\nssh = '127.0.0.1'\nssh_args = {}\n\
                 root = {}\nslots = 1\n[hosts.box.env]\nBUILDQ_TEST_REMOTE = 'yes'\n",
                serde_json::to_string(&server.args()).unwrap(),
                serde_json::to_string(&remote_root).unwrap(),
            ),
        )
        .unwrap();
    };
    let fetched = root.join("fetched");
    let run = |script: &str| -> Output {
        Command::new(BQ)
            .current_dir(&repo)
            .env("BUILDQ_CONFIG", &config)
            .env("BUILDQ_DIR", root.join("q"))
            .env("BUILDQ_CACHE", root.join("cache"))
            .env("BUILDQ_GC", "0")
            .env_remove("BUILDQ_HELD")
            .args(["run", "--host", "box", "--fetch", "out", "--fetch-into"])
            .arg(&fetched)
            .args(["--", "sh", "-c", script])
            .output()
            .unwrap()
    };
    let script = "mkdir -p out && cat input.txt untracked.txt > out/result && \
                  echo \"$BUILDQ_TEST_REMOTE\" >> out/result && \
                  { cat generated/message 2>/dev/null || echo no-generated; } >> out/result && \
                  pwd > out/where && exit 7";

    for (transport, generated) in [("git", "no-generated\n"), ("rsync", "ignored input\n")] {
        write_config(transport);
        let _ = fs::remove_dir_all(&fetched);
        let out = run(script);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(7), "{transport}: {stderr}");
        let result = fs::read_to_string(fetched.join("out/result"))
            .unwrap_or_else(|e| panic!("{transport}: {e}: {stderr}"));
        assert_eq!(
            result,
            format!("edited, not committed\nuntracked\nyes\n{generated}"),
            "{transport}"
        );
        // The command ran in the host's checkout, not in the local worktree.
        let wt = buildq::git::discover(&repo).unwrap();
        let layout = buildq::remote::layout(remote_root.to_str().unwrap(), &wt);
        let remote_wt = fs::read_to_string(fetched.join("out/where")).unwrap();
        assert_eq!(remote_wt.trim(), layout.wt_dir, "{transport}");
        assert!(!repo.join("out").exists(), "{transport}");
    }

    // A second git run after an edit only sends the change and sees it.
    write_config("git");
    fs::write(repo.join("untracked.txt"), "changed again\n").unwrap();
    let out = run(
        "cat untracked.txt > out/again 2>/dev/null || { mkdir -p out && cat untracked.txt > out/again; }",
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(fetched.join("out/again")).unwrap(),
        "changed again\n"
    );
}
