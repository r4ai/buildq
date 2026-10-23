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
        fs::create_dir_all(root.join("q")).unwrap();
        fs::write(
            root.join("q/config.toml"),
            "[cache]\nmin_free='0'\naccounting='logical'\n",
        )
        .unwrap();
        Env { _dir: dir, root }
    }

    fn cmd(&self, slots: usize) -> Command {
        let mut c = Command::new(BQ);
        c.env("BUILDQ_DIR", self.root.join("q"))
            .env("BUILDQ_CACHE", self.root.join("cache"))
            .env("BUILDQ_SLOTS", slots.to_string())
            .env("BUILDQ_GC", "0")
            .env_remove("BUILDQ_MAX_PER_WORKTREE")
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
#[cfg(unix)]
fn automatic_host_selection_uses_live_remote_load() {
    use std::os::unix::fs::PermissionsExt;
    let e = Env::new();
    let bin = e.root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let ssh = bin.join("ssh");
    fs::write(&ssh, "#!/bin/sh\nprintf '%s\\n' '{\"classes\":[{\"class\":\"build\",\"slots\":1,\"holders\":[{\"slot\":0,\"busy\":true,\"job\":null}],\"waiters\":[]}]}'\n").unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir_all(e.root.join("q")).unwrap();
    fs::write(
        e.root.join("q/config.toml"),
        "auto_hosts=['local','box']\n[hosts.box]\nbuildq='buildq'\nslots=1\n",
    )
    .unwrap();
    let wt = buildq::git::discover(&std::env::current_dir().unwrap()).unwrap();
    buildq::util::write_json(
        &e.root.join("q/affinity.json"),
        &std::collections::BTreeMap::from([(wt.key(), "box")]),
    )
    .unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let out = e
        .cmd(1)
        .env("PATH", &path)
        .args(["run", "--host", "auto", "--", BQ, "__helper", "exit", "0"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("picked host local"));
    let top = e
        .cmd(1)
        .env("PATH", &path)
        .args(["top", "--once"])
        .output()
        .unwrap();
    assert!(top.status.success());
    assert!(String::from_utf8_lossy(&top.stdout).contains("box / build: 1/1 busy"));
}

#[test]
fn top_snapshot_shows_a_running_job_and_a_waiter_without_terminal_codes() {
    let e = Env::new();
    let mut holder = e
        .cmd(1)
        .args([
            "run",
            "--label",
            "active build",
            "--",
            BQ,
            "__helper",
            "sleep",
            "10000",
        ])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("top holder", || busy(&e.status(), "build") == 1);
    let mut waiter = e
        .cmd(1)
        .args(["run", "--", BQ, "__helper", "exit", "0"])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("top waiter", || {
        e.status()["classes"][0]["waiters"]
            .as_array()
            .unwrap()
            .len()
            == 1
    });
    // A redirected top defaults to one snapshot and terminates on its own.
    let out = e.cmd(1).args(["top", "--no-hosts"]).output().unwrap();
    holder.kill().unwrap();
    holder.wait().unwrap();
    assert!(waiter.wait().unwrap().success());
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    for expected in [
        "Cache:",
        "effective budget",
        "build: 1/1 busy, 1 waiting",
        "RUN",
        "WAIT",
        "active build",
        "prio 0",
    ] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }
    assert!(!text.contains('\x1b'));
    let invalid = e.cmd(1).args(["top", "--interval", "0"]).output().unwrap();
    assert_eq!(invalid.status.code(), Some(2));
}

#[test]
fn a_member_cap_below_effective_slots_is_reported() {
    let e = Env::new();
    fs::create_dir_all(e.root.join("q")).unwrap();
    fs::write(
        e.root.join("q/config.toml"),
        "slots=1\n[cache]\nmax_members=1\n",
    )
    .unwrap();
    let out = e.cmd(2).arg("config").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("max 2 members"));
    let warning = String::from_utf8_lossy(&out.stderr);
    assert!(
        warning.contains("cache.max_members=1")
            && warning.contains("slots=2")
            && warning.contains("effective cap 2"),
        "{warning}"
    );
    let valid = e.cmd(1).arg("config").output().unwrap();
    assert!(valid.status.success());
    assert!(!String::from_utf8_lossy(&valid.stderr).contains("cache.max_members"));
}

#[test]
#[cfg(unix)]
fn failed_gc_deletions_stay_counted_and_do_not_claim_reclamation() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    fn writable(path: &Path) {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    writable(&entry.path());
                }
            }
        }
    }
    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            writable(&self.0);
        }
    }
    let e = Env::new();
    let _restore = Restore(e.root.clone());
    fs::create_dir_all(e.root.join("q")).unwrap();
    fs::write(
        e.root.join("q/config.toml"),
        "[cache]\nbudget='1M'\nmin_free='0'\naccounting='logical'\n",
    )
    .unwrap();
    let member = e.root.join("cache/pools/p/m0");
    fs::create_dir_all(member.join("locked")).unwrap();
    fs::write(member.join("locked/artifact"), vec![0; 2 * 1024 * 1024]).unwrap();
    buildq::util::write_json(
        &member.with_extension("json"),
        &buildq::pool::MemberMeta {
            last_used_ms: buildq::util::now_ms(),
            ..Default::default()
        },
    )
    .unwrap();
    fs::set_permissions(member.join("locked"), fs::Permissions::from_mode(0o555)).unwrap();
    for attempt in 0..3 {
        let out = e.cmd(1).args(["gc", "--json"]).output().unwrap();
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(out.status.code(), Some(1), "{report}");
        assert_eq!(report["freed"], 0);
        assert!(report["failed_deletions"].as_u64().unwrap() > 0);
        assert!(report["after"].as_u64().unwrap() >= 2 * 1024 * 1024);
        assert!(!report["warnings"].as_array().unwrap().is_empty());
        let usage = e
            .cmd(1)
            .args(["cache", "ls", "--refresh", "--json"])
            .output()
            .unwrap();
        let usage: serde_json::Value = serde_json::from_slice(&usage.stdout).unwrap();
        assert!(usage["trash_bytes"].as_u64().unwrap() >= 2 * 1024 * 1024);
        assert!(usage["total"].as_u64().unwrap() >= 2 * 1024 * 1024);
        let retained = fs::read_dir(e.root.join("cache/trash"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("locked");
        if attempt == 0 {
            // The next retry also cannot measure the remaining files.
            fs::set_permissions(&retained, fs::Permissions::from_mode(0o000)).unwrap();
        } else if attempt == 1 {
            fs::set_permissions(&retained, fs::Permissions::from_mode(0o555)).unwrap();
        }
    }
    writable(&e.root);
    let out = e.cmd(1).args(["gc", "--json"]).output().unwrap();
    assert!(out.status.success());
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["freed"], 2 * 1024 * 1024);
    assert_eq!(report["after"], 0);
}

#[test]
fn a_guarded_process_tree_stops_before_pressure_failure_returns() {
    fn active(pid: u32) -> bool {
        #[cfg(unix)]
        {
            buildq::exec::process_alive(i32::try_from(pid).unwrap())
        }
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::System::Threading::{
                GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            };
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut code = 0;
            let ok = GetExitCodeProcess(handle, &mut code);
            windows_sys::Win32::Foundation::CloseHandle(handle);
            assert_ne!(ok, 0);
            code == 259 // STILL_ACTIVE
        }
    }
    let e = Env::new();
    let pids = e.root.join("pids");
    fs::create_dir_all(&pids).unwrap();
    let mut cmd = e.cmd(1);
    cmd.args(["__helper", "tree"])
        .arg(&pids)
        .arg("1")
        .stderr(Stdio::null());
    let started = Instant::now();
    let result = buildq::exec::run_guarded(&mut cmd, &mut || {
        if fs::read_dir(&pids).unwrap().count() == 2 {
            Err(std::io::Error::other("simulated min_free breach"))
        } else {
            Ok(())
        }
    });
    assert!(result.unwrap_err().to_string().contains("min_free"));
    let ids: Vec<u32> = fs::read_dir(&pids)
        .unwrap()
        .map(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .strip_prefix("pid-")
                .unwrap()
                .parse()
                .unwrap()
        })
        .collect();
    assert_eq!(ids.len(), 2);
    wait_until("guarded children to stop", || {
        ids.iter().all(|pid| !active(*pid))
    });
    assert!(started.elapsed() < Duration::from_secs(3));
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
    let pids = e.root.join("pids");
    fs::create_dir_all(&pids).unwrap();
    // The holder's command has a child of its own, like cargo and rustc.
    let mut holder = e
        .cmd(1)
        .args(["run", "--", BQ, "__helper", "tree"])
        .arg(&pids)
        .arg("1")
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("holder to start", || {
        busy(&e.status(), "build") == 1 && fs::read_dir(&pids).unwrap().count() == 2
    });
    // A second job queues behind it; on Unix it checks that the killed
    // holder's whole command tree is gone by the time it gets the slot.
    let probe: &[&str] = if cfg!(unix) {
        &["dead"]
    } else {
        &["exit", "0"]
    };
    let mut waiter = e
        .cmd(1)
        .args(["run", "--", BQ, "__helper"])
        .args(probe)
        .arg(&pids)
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

// Gates keep admission assertions independent of scheduler timing and job duration.
fn gated_job(e: &Env, cwd: &Path, name: &str, cap: usize, nested: bool) -> Child {
    let mut cmd = e.cmd(2);
    cmd.current_dir(cwd)
        .env("BUILDQ_MAX_PER_WORKTREE", cap.to_string())
        .args(["run", "--"]);
    if nested {
        cmd.args([BQ, "run", "--"]);
    }
    cmd.args([BQ, "__helper", "gate"])
        .arg(e.root.join(format!("{name}.started")))
        .arg(e.root.join(format!("{name}.release")))
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn release_job(e: &Env, name: &str, child: &mut Child) {
    fs::write(e.root.join(format!("{name}.release")), "").unwrap();
    assert!(child.wait().unwrap().success());
}

#[test]
fn worktree_cap_defers_extra_jobs_and_preserves_fifo() {
    use buildq::queue::{ClassQueue, JobInfo};
    let e = Env::new();
    let a = e.root.join("a");
    let b = e.root.join("b");
    fs::create_dir_all(a.join("sub")).unwrap();
    git(&a, &["init", "-q"]);
    git(&a, &["commit", "--allow-empty", "-qm", "init"]);
    git(
        &a,
        &["worktree", "add", "-qb", "other", b.to_str().unwrap()],
    );
    let q = ClassQueue::new(&e.root.join("q"), "build", 2, Duration::from_millis(10))
        .with_max_per_worktree(1);
    let blocker = || q.acquire(JobInfo::default(), &mut |_| {}).unwrap();
    let block0 = blocker();
    let block1 = blocker();
    let mut jobs = Vec::new();
    for (i, cwd) in [a.clone(), a.join("sub"), a.clone(), b].iter().enumerate() {
        jobs.push(gated_job(&e, cwd, &i.to_string(), 1, i == 0));
        wait_until("ticket", || q.status().waiters.len() == i + 1);
    }
    drop(block0);
    wait_until("first worktree job (including nested call)", || {
        e.root.join("0.started").exists()
    });
    assert!(!e.root.join("1.started").exists());
    let st = q.status();
    assert_eq!(
        st.waiters[0].worktree.as_deref(),
        Some(buildq::util::norm_key(&e.root.join("b")).as_str())
    );
    assert!(
        st.waiters[1..]
            .iter()
            .all(|w| w.wait_reason.as_deref() == Some("worktree cap"))
    );
    let out = e
        .cmd(2)
        .env("BUILDQ_MAX_PER_WORKTREE", "1")
        .arg("status")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("waiting: worktree cap"));
    drop(block1);
    wait_until("competing worktree", || e.root.join("3.started").exists());
    assert!(!e.root.join("1.started").exists());
    assert!(!e.root.join("2.started").exists());
    release_job(&e, "0", &mut jobs[0]);
    wait_until("second FIFO job", || e.root.join("1.started").exists());
    assert!(!e.root.join("2.started").exists());
    release_job(&e, "3", &mut jobs[3]);
    // A already holds its cap, but B has no waiting ticket: use the idle slot.
    wait_until("work-conserving third job", || {
        e.root.join("2.started").exists()
    });
    release_job(&e, "1", &mut jobs[1]);
    release_job(&e, "2", &mut jobs[2]);
}

#[test]
fn worktree_cap_allows_idle_slots_without_competitors() {
    let e = Env::new();
    let mut first = gated_job(&e, &e.root, "first", 1, false);
    wait_until("first", || e.root.join("first.started").exists());
    let mut second = gated_job(&e, &e.root, "second", 1, false);
    wait_until("second despite cap", || {
        e.root.join("second.started").exists()
    });
    release_job(&e, "first", &mut first);
    release_job(&e, "second", &mut second);
}

#[test]
fn worktree_cap_configuration_and_environment_override() {
    let e = Env::new();
    let config = || {
        let out = e.cmd(3).arg("config").output().unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    };
    assert!(
        config()
            .lines()
            .any(|s| s.starts_with("max per worktree") && s.ends_with('2'))
    );
    fs::create_dir_all(e.root.join("q")).unwrap();
    fs::write(e.root.join("q/config.toml"), "max_per_worktree = 3\n").unwrap();
    assert!(
        config()
            .lines()
            .any(|s| s.starts_with("max per worktree") && s.ends_with('3'))
    );
    let out = e
        .cmd(3)
        .env("BUILDQ_MAX_PER_WORKTREE", "0")
        .arg("config")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|s| s.starts_with("max per worktree") && s.ends_with('0'))
    );
}

#[test]
fn disabled_worktree_cap_keeps_fifo_across_worktrees() {
    use buildq::queue::{ClassQueue, JobInfo};
    let e = Env::new();
    let other = e.root.join("other");
    fs::create_dir_all(&other).unwrap();
    let q = ClassQueue::new(&e.root.join("q"), "build", 2, Duration::from_millis(10))
        .with_max_per_worktree(0);
    let block = q.acquire(JobInfo::default(), &mut |_| {}).unwrap();
    let mut first = gated_job(&e, &e.root, "first", 0, false);
    wait_until("first", || e.root.join("first.started").exists());
    let mut second = gated_job(&e, &e.root, "second", 0, false);
    wait_until("second queued", || q.status().waiters.len() == 1);
    let mut competitor = gated_job(&e, &other, "competitor", 0, false);
    wait_until("competitor queued", || q.status().waiters.len() == 2);
    drop(block);
    wait_until("second follows FIFO", || {
        e.root.join("second.started").exists()
    });
    assert!(!e.root.join("competitor.started").exists());
    release_job(&e, "first", &mut first);
    wait_until("competitor", || e.root.join("competitor.started").exists());
    release_job(&e, "second", &mut second);
    release_job(&e, "competitor", &mut competitor);
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

fn scoped_target(out: &Output) -> String {
    let args = line(out, "ARGS").replace('"', "");
    args.split_once("--target-dir=")
        .expect("scoped target argument")
        .1
        .trim()
        .into()
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
    assert!(line(&out, "ARGS").starts_with("build -p a "));
    assert_eq!(
        line(&out, "TARGET="),
        "",
        "buildq must not export the pool target"
    );
    let target = scoped_target(&out);
    let cache = s.e.root.join("cache").join("pools");
    assert!(
        Path::new(&target).starts_with(&cache),
        "target {target} should be a pool member under {}",
        cache.display()
    );
    assert_eq!(line(&out, "HELD="), "build");
    assert!(!line(&out, "JOBS=").is_empty());
    assert_eq!(s.history_len(), 1);

    // Cargo still expands the alias; buildq adds an invocation-scoped target.
    let out = s.cargo(&["lint"], 0);
    assert!(line(&out, "ARGS").starts_with("lint "));
    assert!(scoped_target(&out).contains("pools"));
    assert_eq!(line(&out, "TARGET="), "");
    assert_eq!(s.history_len(), 2);

    // Exit codes come back unchanged.
    let out = s.cargo(&["test"], 101);
    assert_eq!(out.status.code(), Some(101));
    assert_eq!(s.history_len(), 3);
    let out = s.cargo(&["test", "--", "--nocapture"], 0);
    let args = line(&out, "ARGS");
    assert!(args.contains("--target-dir="));
    assert!(args.ends_with("-- --nocapture"), "{args}");
    let out = s.cargo(&["llvm-cov", "run"], 0);
    assert_eq!(line(&out, "ARGS"), "llvm-cov run");
    assert!(
        line(&out, "TARGET=").contains("pools"),
        "plugins keep their CLI contract"
    );
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
fn shim_touches_only_changed_outside_files_in_built_members() {
    let s = shim_setup();
    let same = s.repo.join("unchanged.txt");
    let changed = s.repo.join("changed.txt");
    fs::write(&same, "same").unwrap();
    fs::write(&changed, "before").unwrap();
    git(&s.repo, &["add", "-A"]);
    git(&s.repo, &["commit", "-q", "-m", "outside files"]);
    let old = std::time::SystemTime::now() - Duration::from_secs(86400);
    let set_old = |p: &Path| {
        fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(old)
            .unwrap();
        fs::metadata(p).unwrap().modified().unwrap()
    };
    let same_mtime = set_old(&same);
    let changed_mtime = set_old(&changed);
    let first = s.cargo(&["build"], 0);
    assert!(first.status.success(), "{}", stderr(&first));
    assert!(!stderr(&first).contains("outside packages touched"));
    assert_eq!(fs::metadata(&same).unwrap().modified().unwrap(), same_mtime);
    assert_eq!(
        fs::metadata(&changed).unwrap().modified().unwrap(),
        changed_mtime
    );

    fs::write(&changed, "after").unwrap();
    let changed_mtime = set_old(&changed);
    let warm = s.cargo(&["build"], 0);
    assert!(warm.status.success(), "{}", stderr(&warm));
    assert!(
        stderr(&warm).contains("1 file(s) outside packages touched"),
        "{}",
        stderr(&warm)
    );
    assert_eq!(fs::metadata(&same).unwrap().modified().unwrap(), same_mtime);
    assert!(fs::metadata(&changed).unwrap().modified().unwrap() > changed_mtime);

    let unchanged = s.cargo(&["build"], 0);
    assert!(unchanged.status.success(), "{}", stderr(&unchanged));
    assert!(!stderr(&unchanged).contains("outside packages touched"));

    // A member directory may survive without a manifest (e.g. interrupted setup).
    let target = PathBuf::from(scoped_target(&first));
    let manifest = target.with_extension("manifest");
    fs::remove_file(&manifest).unwrap();
    let changed_mtime = set_old(&changed);
    let no_manifest = s.cargo(&["build"], 0);
    assert!(no_manifest.status.success(), "{}", stderr(&no_manifest));
    assert!(!stderr(&no_manifest).contains("outside packages touched"));
    assert_eq!(
        fs::metadata(&changed).unwrap().modified().unwrap(),
        changed_mtime
    );
    assert!(manifest.exists());
}

#[test]
fn gc_budget_removes_recent_run_copies_but_preserves_locked_runs() {
    let e = Env::new();
    let runs = e.root.join("cache/runs");
    for id in 0..2 {
        fs::create_dir_all(runs.join(format!("r{id}"))).unwrap();
        fs::write(runs.join(format!("r{id}/app")), vec![0u8; 2 * 1024 * 1024]).unwrap();
        fs::write(runs.join(format!("r{id}.json")), "{}").unwrap();
    }
    let busy = buildq::lock::try_lock(&runs.join("r1.lock"))
        .unwrap()
        .unwrap();
    let gc = |dry: bool| {
        let mut cmd = e.cmd(1);
        cmd.args(["gc", "--budget", "1M", "--json"]);
        if dry {
            cmd.arg("--dry-run");
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };
    let dry = gc(true);
    assert_eq!(dry["after"], 2 * 1024 * 1024);
    assert!(runs.join("r0/app").exists());
    let report = gc(false);
    assert_eq!(report["after"], 2 * 1024 * 1024);
    assert!(!runs.join("r0").exists());
    assert!(runs.join("r1/app").exists());
    drop(busy);
    let report = gc(false);
    assert_eq!(report["after"], 0);
    assert!(!runs.join("r1").exists());
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
    // Rust quotes arguments it hands to a .cmd shim on Windows; cargo itself
    // sees them unquoted, so compare without the quotes.
    let args = line(&out, "ARGS").replace('"', "");
    let target = scoped_target(&out);
    assert_eq!(line(&out, "TARGET="), "");
    assert!(args.starts_with("build --target-dir="));
    assert!(Path::new(&target).starts_with(&pools), "{target}");
    assert!(
        Path::new(&target).ends_with(Path::new("sub").join("target-base")),
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
    let target = scoped_target(&out);
    assert_eq!(
        line(&out, "TARGET="),
        "target/ab",
        "preserve the caller's explicit environment"
    );
    assert!(
        Path::new(&target).starts_with(&pools)
            && Path::new(&target).ends_with(Path::new("sub").join("ab")),
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

#[test]
fn cached_member_sizes_refresh_before_gc_budget_decisions() {
    let e = Env::new();
    let member = e.root.join("cache/pools/p/m0");
    fs::create_dir_all(member.join("debug/deps")).unwrap();
    let artifact = member.join("debug/deps/artifact");
    fs::write(&artifact, vec![0; 1024]).unwrap();
    buildq::util::write_json(
        &member.with_extension("json"),
        &buildq::pool::MemberMeta {
            last_used_ms: buildq::util::now_ms(),
            ..Default::default()
        },
    )
    .unwrap();
    let usage = |refresh: bool| {
        let mut cmd = e.cmd(1);
        cmd.args(["cache", "ls", "--json"]);
        if refresh {
            cmd.arg("--refresh");
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["total"]
            .as_u64()
            .unwrap()
    };
    assert_eq!(usage(false), 1024);
    fs::write(&artifact, vec![0; 2 * 1024 * 1024]).unwrap();
    assert_eq!(usage(false), 1024);
    assert_eq!(usage(true), 2 * 1024 * 1024);
    fs::write(&artifact, vec![0; 1024]).unwrap();
    // A cached overestimate must not evict a member that now fits the budget.
    let out = e
        .cmd(1)
        .args(["gc", "--budget", "1M", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["before"], 1024);
    assert_eq!(report["after"], 1024);
    assert!(member.exists());
    // Conversely, an underestimate must not hide growth from eviction.
    fs::write(&artifact, vec![0; 2 * 1024 * 1024]).unwrap();
    let out = e
        .cmd(1)
        .args(["gc", "--budget", "1M", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["after"], 0);
    assert!(!member.exists());
}
