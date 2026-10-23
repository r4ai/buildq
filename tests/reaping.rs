//! Only this binary becomes a subreaper, so it cannot reap other tests' children.
#![cfg(target_os = "linux")]

use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};

const BQ: &str = env!("CARGO_BIN_EXE_buildq");

struct Reaper;

impl Reaper {
    fn settle(&self) -> (usize, bool) {
        let start = Instant::now();
        let mut collectors = 0;
        loop {
            let mut remaining = false;
            for entry in fs::read_dir("/proc").unwrap().flatten() {
                let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                    continue;
                };
                let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
                    continue;
                };
                let Some((name, fields)) = stat.rsplit_once(") ") else {
                    continue;
                };
                let parent = fields.split_whitespace().nth(1);
                if parent.and_then(|p| p.parse::<u32>().ok()) != Some(std::process::id()) {
                    continue;
                }
                // All direct children have already been waited for; only our
                // own adopted descendants can remain. Read the name before reap.
                let reaped = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
                if reaped == pid {
                    if name.ends_with("(buildq") {
                        collectors += 1;
                    }
                } else {
                    remaining = true;
                }
            }
            if !remaining {
                return (collectors, true);
            }
            if start.elapsed() > Duration::from_secs(10) {
                return (collectors, false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.settle();
    }
}

#[test]
fn inline_gc_and_recent_background_gc_leave_no_adopted_collectors() {
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
        0
    );
    let temp = tempfile::tempdir().unwrap();
    let reaper = Reaper;
    let root = temp.path();
    let ws = root.join("ws");
    fs::create_dir_all(ws.join("src")).unwrap();
    fs::write(
        ws.join("Cargo.toml"),
        "[package]\nname='reaper-fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(ws.join("src/main.rs"), "fn main() {}\n").unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    ] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&ws)
                .status()
                .unwrap()
                .success()
        );
    }
    fs::create_dir_all(root.join("q")).unwrap();
    let config = root.join("q/config.toml");
    let stamp = root.join("cache/gc.stamp");
    let configure = |mode: &str| {
        fs::write(
            &config,
            format!("slots=1\n[cache]\nmin_free='0'\nbudget='1G'\ngc_mode='{mode}'\n"),
        )
        .unwrap();
    };
    let build = |enabled: bool| {
        let out = Command::new(BQ)
            .args(["cargo", "build", "--offline"])
            .current_dir(&ws)
            .env("BUILDQ_DIR", root.join("q"))
            .env("BUILDQ_CACHE", root.join("cache"))
            .env("BUILDQ_CONFIG", &config)
            .env("BUILDQ_REAL_CARGO", env!("CARGO"))
            .env("BUILDQ_GC", if enabled { "1" } else { "0" })
            .env_remove("BUILDQ_HELD")
            .env_remove("CARGO_TARGET_DIR")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    configure("inline");
    build(false);
    assert_eq!(reaper.settle(), (0, true));
    for _ in 0..3 {
        let _ = fs::remove_file(&stamp);
        build(true);
        assert!(stamp.is_file(), "inline collection must actually run");
        assert_eq!(reaper.settle(), (0, true));
    }
    configure("background");
    let _ = fs::remove_file(&stamp);
    build(true);
    let (adopted, settled) = reaper.settle();
    assert!(
        settled && adopted > 0,
        "control must observe the detached collector"
    );
    assert!(stamp.is_file());
    for _ in 0..3 {
        build(true);
        assert_eq!(
            reaper.settle(),
            (0, true),
            "recent, under-budget jobs must not fork a collector"
        );
    }
}
