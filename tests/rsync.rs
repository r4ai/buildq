//! Real rsync and Cargo, with a local shell standing in for SSH.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

const BQ: &str = env!("CARGO_BIN_EXE_buildq");

fn git(repo: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .current_dir(repo)
            .args(["-c", "user.name=test", "-c", "user.email=test@localhost"])
            .args(args)
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
#[ignore = "requires POSIX rsync; CI runs this on Linux and macOS"]
fn rsync_sends_ignored_sources_preserves_incremental_builds_and_fetches() {
    assert!(
        Command::new("rsync")
            .arg("--version")
            .output()
            .unwrap()
            .status
            .success()
    );
    let tmp = tempfile::tempdir().unwrap();
    let root = buildq::util::canonical(tmp.path()).join("local with spaces");
    let repo = root.join("source");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::create_dir_all(repo.join("generated")).unwrap();
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname='rsync-app'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(
        repo.join("src/main.rs"),
        "fn main() { print!(\"{}\", include_str!(\"../generated/message\")); }\n",
    )
    .unwrap();
    fs::write(repo.join(".gitignore"), "/target/\n/generated/\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "initial"]);
    fs::write(repo.join("generated/message"), "v1").unwrap();
    let input_mtime = fs::metadata(repo.join("generated/message"))
        .unwrap()
        .modified()
        .unwrap();
    fs::write(repo.join("untracked"), "remove later").unwrap();
    fs::write(
        repo.join("src/main.rs"),
        "fn main() { println!(\"{}\", include_str!(\"../generated/message\")); }\n",
    )
    .unwrap();
    fs::create_dir_all(repo.join("target")).unwrap();
    fs::write(repo.join("target/local-only"), "do not transfer").unwrap();
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let ssh = bin.join("ssh");
    fs::write(&ssh, "#!/bin/sh\nshift\nexec sh -c \"$*\"\n").unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let remote = root.join("remote with spaces");
    let config = root.join("config.toml");
    let write_config = |transport: &str| {
        fs::write(
            &config,
            format!(
                "[hosts.box]\ntransport='{transport}'\nroot={}\n",
                serde_json::to_string(&remote).unwrap()
            ),
        )
        .unwrap();
    };
    let run = || -> Output {
        Command::new(BQ)
            .current_dir(&repo)
            .env("PATH", &path)
            .env("BUILDQ_CONFIG", &config)
            .env("BUILDQ_DIR", root.join("q"))
            .env("BUILDQ_CACHE", root.join("cache"))
            .env("BUILDQ_GC", "0")
            .env_remove("BUILDQ_HELD")
            .env_remove("CARGO_TARGET_DIR")
            .env("RUSTC_WRAPPER", "")
            .args([
                "run",
                "--host",
                "box",
                "--fetch",
                "target/debug/rsync-app",
                "--fetch-into",
            ])
            .arg(root.join("fetched"))
            .args([
                "--",
                env!("CARGO"),
                "build",
                "--offline",
                "--message-format=json",
            ])
            .output()
            .unwrap()
    };
    // The existing Git transport cannot send this ignored build input.
    write_config("git");
    let missing_input = run();
    assert_eq!(missing_input.status.code(), Some(101));
    assert!(
        String::from_utf8_lossy(&missing_input.stdout).contains("generated/message"),
        "{}",
        String::from_utf8_lossy(&missing_input.stderr)
    );
    write_config("rsync");
    let out = run();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let wt = buildq::git::discover(&repo).unwrap();
    let layout = buildq::remote::layout(remote.to_str().unwrap(), &wt);
    let mirror = Path::new(&layout.wt_dir);
    assert!(mirror.join(".git").is_file());
    assert!(!mirror.join("target/local-only").exists());
    fs::write(mirror.join("target/remote-only"), "keep").unwrap();
    let out = run();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .any(|v| v["reason"] == "compiler-artifact" && v["fresh"] == true)
    );
    assert!(mirror.join("target/remote-only").exists());
    fs::write(repo.join("generated/message"), "v2 updated").unwrap();
    fs::File::options()
        .write(true)
        .open(repo.join("generated/message"))
        .unwrap()
        .set_modified(input_mtime)
        .unwrap();
    fs::remove_file(repo.join("untracked")).unwrap();
    let out = run();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!mirror.join("untracked").exists());
    let fetched = Command::new(root.join("fetched/rsync-app"))
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&fetched.stdout).trim(),
        "v2 updated"
    );
    let status = Command::new("git")
        .current_dir(&repo)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&status.stdout).contains(" M src/main.rs"));
    assert!(
        !repo.join("Cargo.lock").exists(),
        "local source tree remains unchanged"
    );
}
