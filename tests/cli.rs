//! Process-level tests against the real binary: separate processes, real
//! OS locks, real kills.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const BQ: &str = env!("CARGO_BIN_EXE_buildq");

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = buildq::util::canonical(dir.path());
        Env { _dir: dir, root }
    }

    fn cmd(&self, slots: usize) -> Command {
        let mut c = Command::new(BQ);
        c.env("BUILDQ_DIR", self.root.join("q"))
            .env("BUILDQ_CACHE", self.root.join("cache"))
            .env("BUILDQ_SLOTS", slots.to_string())
            .env("BUILDQ_GC", "0")
            .env_remove("BUILDQ_HELD")
            .env_remove("BUILDQ_CONFIG")
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("BUILDQ_REAL_CARGO");
        c
    }

    fn status(&self) -> serde_json::Value {
        let out = self.cmd(1).args(["status", "--json"]).output().unwrap();
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

fn busy(status: &serde_json::Value, class: &str) -> usize {
    status["classes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["class"] == class)
        .map(|c| {
            c["holders"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|h| h["busy"] == true)
                .count()
        })
        .unwrap_or(0)
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(
            t.elapsed() < Duration::from_secs(30),
            "timed out waiting for {what}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn concurrency_never_exceeds_slots_across_processes() {
    let e = Env::new();
    let spans = e.root.join("spans");
    fs::create_dir_all(&spans).unwrap();
    let kids: Vec<Child> = (0..6)
        .map(|_| {
            e.cmd(2)
                .args(["run", "--", BQ, "__helper", "span"])
                .arg(&spans)
                .arg("400")
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut k in kids {
        assert!(k.wait().unwrap().success());
    }
    let mut events = Vec::new();
    for f in fs::read_dir(&spans).unwrap().flatten() {
        let s = fs::read_to_string(f.path()).unwrap();
        let (a, b) = s.split_once(' ').unwrap();
        events.push((a.parse::<u64>().unwrap(), 1i32));
        events.push((b.parse::<u64>().unwrap(), -1));
    }
    assert_eq!(events.len(), 12);
    // Ends sort before starts at equal timestamps.
    events.sort();
    let (mut cur, mut max) = (0, 0);
    for (_, d) in events {
        cur += d;
        max = max.max(cur);
    }
    assert!(max <= 2, "{max} jobs overlapped with 2 slots");
    assert_eq!(max, 2, "the two slots should both be used");
}

#[test]
fn exit_code_passes_through() {
    let e = Env::new();
    let st = e
        .cmd(1)
        .args(["run", "--", BQ, "__helper", "exit", "7"])
        .status()
        .unwrap();
    assert_eq!(st.code(), Some(7));
    let hist = e.cmd(1).args(["history", "--json"]).output().unwrap();
    let h: serde_json::Value = serde_json::from_slice(&hist.stdout).unwrap();
    assert_eq!(h[0]["exit"], 7);
    // A command that cannot start is an error of buildq itself.
    let st = e
        .cmd(1)
        .args(["run", "--", "definitely-not-a-command-xyz"])
        .status()
        .unwrap();
    assert_eq!(st.code(), Some(125));
}

#[test]
fn killed_holder_releases_its_slot() {
    let e = Env::new();
    let mut holder = e
        .cmd(1)
        .args(["run", "--", BQ, "__helper", "sleep", "60000"])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("holder to start", || busy(&e.status(), "build") == 1);
    // A second job queues behind it.
    let mut waiter = e
        .cmd(1)
        .args(["run", "--", BQ, "__helper", "exit", "0"])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("waiter to enqueue", || {
        e.status()["classes"][0]["waiters"]
            .as_array()
            .unwrap()
            .len()
            == 1
    });
    holder.kill().unwrap();
    holder.wait().unwrap();
    let t = Instant::now();
    assert!(waiter.wait().unwrap().success());
    assert!(t.elapsed() < Duration::from_secs(10));
    assert_eq!(busy(&e.status(), "build"), 0);
}

#[test]
fn nested_calls_do_not_deadlock() {
    let e = Env::new();
    let out = e
        .cmd(1)
        .args([
            "run",
            "--",
            BQ,
            "run",
            "--",
            BQ,
            "__helper",
            "env",
            "BUILDQ_HELD",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "BUILDQ_HELD=build"
    );
    // Another class is still queued (and marked) separately.
    let out = e
        .cmd(1)
        .args([
            "run",
            "--",
            BQ,
            "run",
            "--class",
            "gpu",
            "--",
            BQ,
            "__helper",
            "env",
            "BUILDQ_HELD",
        ])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "BUILDQ_HELD=build,gpu"
    );
}

#[test]
fn priority_overtakes_waiting_jobs() {
    let e = Env::new();
    let order = e.root.join("order");
    fs::create_dir_all(&order).unwrap();
    let mut blocker = e
        .cmd(1)
        .args(["run", "--", BQ, "__helper", "sleep", "1500"])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("blocker", || busy(&e.status(), "build") == 1);
    let mut kids = Vec::new();
    for (i, prio) in [(0, "0"), (1, "0"), (2, "10")] {
        kids.push(
            e.cmd(1)
                .args(["run", "-p", prio, "--", BQ, "__helper", "span"])
                .arg(order.join(i.to_string()))
                .arg("50")
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        fs::create_dir_all(order.join(i.to_string())).unwrap();
        wait_until("enqueue", || {
            e.status()["classes"][0]["waiters"]
                .as_array()
                .unwrap()
                .len()
                == i + 1
        });
    }
    blocker.wait().unwrap();
    for mut k in kids {
        k.wait().unwrap();
    }
    let start = |i: usize| -> u64 {
        let d = order.join(i.to_string());
        let f = fs::read_dir(d).unwrap().flatten().next().unwrap().path();
        fs::read_to_string(f)
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    assert!(start(2) < start(0), "priority 10 runs first");
    assert!(start(0) < start(1), "FIFO within a priority");
}

// ---------------------------------------------------------------------------
// The cargo shim, with a fake "real cargo" that reports what it was given.

fn fake_cargo(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    if cfg!(windows) {
        fs::write(
            dir.join("cargo.cmd"),
            "@echo off\r\necho ARGS %*\r\necho TARGET=%CARGO_TARGET_DIR%\r\necho JOBS=%CARGO_BUILD_JOBS%\r\necho HELD=%BUILDQ_HELD%\r\nexit /b %FAKE_EXIT%\r\n",
        )
        .unwrap();
    } else {
        let p = dir.join("cargo");
        fs::write(
            &p,
            "#!/bin/sh\necho ARGS \"$@\"\necho TARGET=$CARGO_TARGET_DIR\necho JOBS=$CARGO_BUILD_JOBS\necho HELD=$BUILDQ_HELD\nexit ${FAKE_EXIT:-0}\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
}

fn git(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
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

struct ShimSetup {
    e: Env,
    shim: PathBuf,
    repo: PathBuf,
    path: std::ffi::OsString,
}

fn shim_setup() -> ShimSetup {
    let e = Env::new();
    let shim = e.root.join("shim");
    let out = e
        .cmd(1)
        .args(["shim", "install", "cargo", "--into"])
        .arg(&shim)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let real = e.root.join("realbin");
    fake_cargo(&real);
    let repo = e.root.join("repo");
    fs::create_dir_all(repo.join(".cargo")).unwrap();
    fs::create_dir_all(repo.join("crates/a/src")).unwrap();
    fs::write(
        repo.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/a\"]\n",
    )
    .unwrap();
    fs::write(
        repo.join("crates/a/Cargo.toml"),
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(repo.join("crates/a/src/lib.rs"), "").unwrap();
    fs::write(
        repo.join(".cargo/config.toml"),
        "[alias]\nlint = \"clippy --all-targets\"\nfl = \"fmt --check\"\n",
    )
    .unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let mut paths = vec![shim.clone(), real];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let path = std::env::join_paths(paths).unwrap();
    ShimSetup {
        e,
        shim,
        repo,
        path,
    }
}

impl ShimSetup {
    fn cargo(&self, args: &[&str], exit: i32) -> Output {
        self.cargo_with(args, exit, &[])
    }

    fn cargo_with(&self, args: &[&str], exit: i32, envs: &[(&str, &str)]) -> Output {
        let exe = self
            .shim
            .join(if cfg!(windows) { "cargo.exe" } else { "cargo" });
        let c = self.e.cmd(2);
        let mut c2 = Command::new(exe);
        for (k, v) in c.get_envs() {
            match v {
                Some(v) => c2.env(k, v),
                None => c2.env_remove(k),
            };
        }
        c2.envs(envs.iter().copied())
            .args(args)
            .current_dir(&self.repo)
            .env("PATH", &self.path)
            .env("FAKE_EXIT", exit.to_string())
            .output()
            .unwrap()
    }

    fn history_len(&self) -> usize {
        let out = self.e.cmd(1).args(["history", "--json"]).output().unwrap();
        serde_json::from_slice::<serde_json::Value>(&out.stdout)
            .unwrap()
            .as_array()
            .unwrap()
            .len()
    }
}

fn line<'a>(out: &'a Output, key: &str) -> &'a str {
    let s = std::str::from_utf8(&out.stdout).unwrap();
    s.lines()
        .find_map(|l| l.strip_prefix(key))
        .unwrap_or_else(|| {
            panic!(
                "no {key} in {s}\nstderr: {}",
                String::from_utf8_lossy(&out.stderr)
            )
        })
        .trim()
}

#[test]
fn shim_routes_heavy_commands_through_queue_and_pool() {
    let s = shim_setup();
    let out = s.cargo(&["build", "-p", "a"], 0);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(line(&out, "ARGS"), "build -p a");
    let target = line(&out, "TARGET=");
    let cache = s.e.root.join("cache").join("pools");
    assert!(
        Path::new(target).starts_with(&cache),
        "target {target} should be a pool member under {}",
        cache.display()
    );
    assert_eq!(line(&out, "HELD="), "build");
    assert!(!line(&out, "JOBS=").is_empty());
    assert_eq!(s.history_len(), 1);

    // Alias to a heavy command: queued, args untouched (cargo expands it itself).
    let out = s.cargo(&["lint"], 0);
    assert_eq!(line(&out, "ARGS"), "lint");
    assert!(line(&out, "TARGET=").contains("pools"));
    assert_eq!(s.history_len(), 2);

    // Exit codes come back unchanged.
    let out = s.cargo(&["test"], 101);
    assert_eq!(out.status.code(), Some(101));
    assert_eq!(s.history_len(), 3);
}

#[test]
fn shim_passes_light_commands_straight_through() {
    let s = shim_setup();
    for args in [
        &["fmt"][..],
        &["fl"],
        &["--version"],
        &["metadata", "--no-deps"],
        &["build", "--help"],
    ] {
        let out = s.cargo(args, 3);
        assert_eq!(out.status.code(), Some(3), "{args:?}");
        assert_eq!(line(&out, "ARGS"), args.join(" "));
        let target = line(&out, "TARGET=");
        assert!(
            target.is_empty() || target.contains("CARGO_TARGET_DIR"),
            "{args:?}: {target}"
        );
    }
    assert_eq!(s.history_len(), 0);
}

#[test]
fn shim_inside_a_held_job_is_passthrough() {
    let s = shim_setup();
    let exe = s
        .shim
        .join(if cfg!(windows) { "cargo.exe" } else { "cargo" });
    // What a build script or test calling cargo sees: the slot is already held.
    let out = Command::new(exe)
        .args(["build"])
        .current_dir(&s.repo)
        .env("BUILDQ_DIR", s.e.root.join("q"))
        .env("BUILDQ_HELD", "build")
        .env("PATH", &s.path)
        .env("FAKE_EXIT", "0")
        .env_remove("BUILDQ_REAL_CARGO")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(line(&out, "ARGS"), "build");
    assert!(!line(&out, "TARGET=").contains("pools"));
    assert_eq!(s.history_len(), 0);
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

#[test]
fn explicit_target_dir_is_announced_and_counted() {
    let s = shim_setup();
    let base = s.repo.join("target-base");
    fs::create_dir_all(base.join("release")).unwrap();
    fs::write(base.join("release/app"), vec![0u8; 4096]).unwrap();

    let out = s.cargo(&["build", "--target-dir", "target-base"], 0);
    assert!(out.status.success(), "{}", stderr(&out));
    // Not pooled: args untouched, no CARGO_TARGET_DIR from buildq.
    assert_eq!(line(&out, "ARGS"), "build --target-dir target-base");
    assert_eq!(line(&out, "TARGET="), "");
    let err = stderr(&out);
    assert!(
        err.contains("explicit target dir") && err.contains("not pooled"),
        "{err}"
    );
    assert!(err.contains("pool_explicit"), "inside the worktree: {err}");

    let hist = s.e.cmd(1).args(["history", "--json"]).output().unwrap();
    let hist: serde_json::Value = serde_json::from_slice(&hist.stdout).unwrap();
    let ext = hist[0]["external"].as_str().unwrap();
    assert!(Path::new(ext).ends_with("target-base"), "{ext}");

    let ls = s.e.cmd(1).args(["cache", "ls", "--json"]).output().unwrap();
    let ls: serde_json::Value = serde_json::from_slice(&ls.stdout).unwrap();
    assert_eq!(ls["external"].as_array().unwrap().len(), 1, "{ls}");
    assert!(ls["external_total"].as_u64().unwrap() >= 4096, "{ls}");

    // Over budget because of it: gc warns instead of deleting it.
    let gc = s.e.cmd(1).args(["gc", "--budget", "1K"]).output().unwrap();
    let text = String::from_utf8_lossy(&gc.stdout);
    assert!(text.contains("warning:"), "{text}");
    assert!(base.join("release/app").exists());

    // BUILDQ_QUIET silences the notice.
    let out = s.cargo_with(
        &["build"],
        0,
        &[("CARGO_TARGET_DIR", "/elsewhere/t"), ("BUILDQ_QUIET", "1")],
    );
    assert_eq!(line(&out, "TARGET="), "/elsewhere/t");
    assert!(!stderr(&out).contains("explicit target dir"));
}

#[test]
fn pool_explicit_reroots_target_dirs_inside_the_worktree() {
    let s = shim_setup();
    let pools = s.e.root.join("cache").join("pools");
    let on = [("BUILDQ_POOL_EXPLICIT", "1")];

    let out = s.cargo_with(&["build", "--target-dir=target-base"], 0, &on);
    assert!(out.status.success(), "{}", stderr(&out));
    let args = line(&out, "ARGS");
    let target = line(&out, "TARGET=");
    let rewritten = args.strip_prefix("build --target-dir=").unwrap();
    assert_eq!(rewritten, target.replace('\\', "/"));
    assert!(Path::new(target).starts_with(&pools), "{target}");
    assert!(
        Path::new(target).ends_with(Path::new("sub").join("target-base")),
        "{target}"
    );

    // From the environment, too.
    let out = s.cargo_with(
        &["build"],
        0,
        &[
            ("BUILDQ_POOL_EXPLICIT", "1"),
            ("CARGO_TARGET_DIR", "target/ab"),
        ],
    );
    let target = line(&out, "TARGET=");
    assert!(
        Path::new(target).starts_with(&pools)
            && Path::new(target).ends_with(Path::new("sub").join("ab")),
        "{target}"
    );

    // Outside the worktree it stays explicit and unpooled.
    let outside = s.e.root.join("outside");
    let out = s.cargo_with(
        &["build", "--target-dir", outside.to_str().unwrap()],
        0,
        &on,
    );
    assert_eq!(line(&out, "TARGET="), "");
    assert!(stderr(&out).contains("not pooled"), "{}", stderr(&out));
}
