//! Opt-in Linux SSH integration test. Never sets up keys, hosts, or sshd.
#![cfg(target_os = "linux")]

use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use buildq::config::{FileConfig, HostConfig};
use buildq::remote::{self, Ssh, Transport};

const BQ: &str = env!("CARGO_BIN_EXE_buildq");

fn checked(out: Output) -> Output {
    assert!(
        out.status.success(),
        "status={}\nstdout={}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn remote_ok(t: &Ssh, argv: &[&str]) -> String {
    let argv = argv.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let (code, out) = t.exec(None, &[], &argv, true).unwrap();
    assert_eq!(code, 0, "remote {argv:?}");
    out
}

struct RemoteTemp<'a>(&'a Ssh, String);
impl Drop for RemoteTemp<'_> {
    fn drop(&mut self) {
        let _ = self.0.exec(
            None,
            &[],
            &["rm".into(), "-rf".into(), "--".into(), self.1.clone()],
            true,
        );
    }
}

fn cli(repo: &Path, config: &Path, queue: &Path) -> Command {
    let mut c = Command::new(BQ);
    c.current_dir(repo)
        .env("BUILDQ_CONFIG", config)
        .env("BUILDQ_DIR", queue)
        .env_remove("BUILDQ_HELD")
        .env_remove("BUILDQ_AS_CLI");
    c
}

fn remote_run(repo: &Path, config: &Path, queue: &Path, args: &[&str]) -> Output {
    cli(repo, config, queue)
        .args(["run", "--host", "test", "--"])
        .args(args)
        .output()
        .unwrap()
}

/// Exercise an actual terminal Ctrl+C: ssh must allocate a remote PTY and the
/// remote shell must exit 130. The process group belongs only to this test.
fn ctrl_c(mut cmd: Command) {
    let (mut master_fd, mut slave_fd) = (-1, -1);
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0
    );
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    cmd.stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
        let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
        assert_eq!(
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK),
            0
        );
    }
    let mut child = cmd.spawn().unwrap();
    let start = Instant::now();
    let mut transcript = String::new();
    let mut sent = false;
    loop {
        let mut buf = [0u8; 4096];
        if let Ok(n) = master.read(&mut buf) {
            transcript.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        if !sent
            && transcript
                .lines()
                .any(|line| line.trim() == "SSH_SIGNAL_READY")
        {
            master.write_all(&[3]).unwrap();
            sent = true;
        }
        if let Some(status) = child.try_wait().unwrap() {
            assert!(sent, "remote never became ready: {transcript}");
            assert_eq!(status.code(), Some(130), "{transcript}");
            return;
        }
        if start.elapsed() > Duration::from_secs(60) {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            panic!("SSH Ctrl+C timed out: {transcript}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn real_ssh_dirty_incremental_fetch_status_and_ctrl_c() {
    let Ok(dest) = std::env::var("BUILDQ_SSH_TEST_HOST") else {
        eprintln!("skipping real SSH test: set BUILDQ_SSH_TEST_HOST to opt in");
        return;
    };
    assert!(
        !dest.trim().is_empty(),
        "BUILDQ_SSH_TEST_HOST must name a host"
    );
    let t = Ssh {
        dest: dest.clone(),
        args: [
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=1",
        ]
        .map(String::from)
        .to_vec(),
    };
    let bq = std::env::var("BUILDQ_SSH_TEST_BUILDQ").unwrap_or_else(|_| "buildq".into());
    remote_ok(&t, &[&bq, "--version"]);
    let root = remote_ok(&t, &["mktemp", "-d", "/tmp/buildq-ssh-e2e.XXXXXXXXXX"])
        .trim()
        .to_string();
    assert!(root.starts_with("/tmp/buildq-ssh-e2e.") && !root.contains('\n'));
    let _cleanup = RemoteTemp(&t, root.clone());
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("dirty source");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = 'buildq-ssh-fixture'\nversion = '0.1.0'\nedition = '2021'\n",
    )
    .unwrap();
    fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(repo.join("deleted"), "delete me").unwrap();
    fs::write(repo.join(".gitignore"), "target/\nignored\n").unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=buildq-test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    ] {
        checked(
            Command::new("git")
                .current_dir(&repo)
                .args(args)
                .output()
                .unwrap(),
        );
    }
    fs::write(
        repo.join("src/main.rs"),
        "mod extra; fn main() { println!(\"{}\", extra::VALUE); }\n",
    )
    .unwrap();
    fs::write(
        repo.join("src/extra.rs"),
        "pub const VALUE: &str = \"dirty\";\n",
    )
    .unwrap();
    fs::write(repo.join("ignored"), "must not travel").unwrap();
    fs::remove_file(repo.join("deleted")).unwrap();
    let head = fs::read(repo.join(".git/HEAD")).unwrap();
    let index = fs::read(repo.join(".git/index")).unwrap();
    let h = HostConfig {
        ssh: Some(dest),
        ssh_args: t.args.clone(),
        root: Some(format!("{root}/sources with spaces")),
        buildq: Some(bq),
        ..Default::default()
    };
    let mut f = FileConfig::default();
    f.hosts.insert("test".into(), h.clone());
    let config = tmp.path().join("config.toml");
    fs::write(&config, toml::to_string(&f).unwrap()).unwrap();
    let queue = tmp.path().join("queue");
    let wt = buildq::git::discover(&repo).unwrap();
    let l = remote::layout(h.root.as_ref().unwrap(), &wt);
    let mut executable = String::new();
    let mut mtime = String::new();
    for round in 0..2 {
        let out = checked(remote_run(
            &repo,
            &config,
            &queue,
            &["cargo", "build", "--offline", "-v", "--message-format=json"],
        ));
        let stderr = String::from_utf8_lossy(&out.stderr);
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
                && let Some(path) = v["executable"].as_str()
            {
                executable = path.into();
            }
        }
        assert!(!executable.is_empty(), "no compiler artifact: {stderr}");
        let now = remote_ok(&t, &["stat", "-c", "%y", &executable]);
        if round == 1 {
            assert_eq!(now, mtime, "second build rewrote executable");
            assert!(stderr.contains("Fresh buildq-ssh-fixture"), "{stderr}");
        }
        mtime = now;
    }
    remote_ok(&t, &["test", "!", "-e", &format!("{}/ignored", l.wt_dir)]);
    remote_ok(&t, &["test", "!", "-e", &format!("{}/deleted", l.wt_dir)]);
    assert_eq!(remote_ok(&t, &[&executable]).trim(), "dirty");
    let fetch = tmp.path().join("fetched");
    checked(
        cli(&repo, &config, &queue)
            .args([
                "run",
                "--host",
                "test",
                "--fetch",
                "artifact with spaces.txt",
                "--fetch-into",
            ])
            .arg(&fetch)
            .args([
                "--",
                "sh",
                "-c",
                "printf artifact > 'artifact with spaces.txt'",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(
        fs::read_to_string(fetch.join("artifact with spaces.txt")).unwrap(),
        "artifact"
    );
    assert_eq!(
        remote_run(&repo, &config, &queue, &["sh", "-c", "exit 37"])
            .status
            .code(),
        Some(37)
    );
    let out = checked(
        cli(&repo, &config, &queue)
            .args(["status", "--hosts", "--json"])
            .output()
            .unwrap(),
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["hosts"]["test"]["classes"].is_array(), "{v}");
    let mut signal = cli(&repo, &config, &queue);
    signal.args([
        "run",
        "--host",
        "test",
        "--",
        "sh",
        "-c",
        "trap 'exit 130' INT; printf 'SSH_SIGNAL_READY\\n'; while :; do sleep 1; done",
    ]);
    ctrl_c(signal);
    assert_eq!(fs::read(repo.join(".git/HEAD")).unwrap(), head);
    assert_eq!(fs::read(repo.join(".git/index")).unwrap(), index);
}
