//! Real rsync protocol with a test-only SSH stand-in; no network or SSH setup.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use buildq::config::{FileConfig, HostConfig, SyncTransport};

#[test]
fn rsync_cli_sync_run_and_fetch() {
    if Command::new("rsync").arg("--version").output().is_err() {
        eprintln!("skipping: rsync is not installed");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let repo = root.join("source with spaces");
    let bin = root.join("bin");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&bin).unwrap();
    // Both ssh exec's single shell command and rsync's server argv run locally.
    let ssh = bin.join("ssh");
    fs::write(
        &ssh,
        "#!/bin/sh\n[ \"$1\" = fixture ] || exit 99\nshift\nexec sh -c \"$*\"\n",
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    for args in [
        ["init", "-q"].as_slice(),
        &["config", "user.name", "fixture"],
        &["config", "user.email", "test@example.invalid"],
    ] {
        assert!(
            Command::new("git")
                .current_dir(&repo)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    fs::write(repo.join(".gitignore"), "generated\nsecret\n").unwrap();
    fs::write(repo.join("tracked"), "initial").unwrap();
    for args in [["add", "."].as_slice(), &["commit", "-qm", "fixture"]] {
        assert!(
            Command::new("git")
                .current_dir(&repo)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    fs::write(repo.join("generated"), "ignored generated source").unwrap();
    fs::write(repo.join("secret"), "excluded").unwrap();
    let h = HostConfig {
        ssh: Some("fixture".into()),
        root: Some(root.join("remote root").display().to_string()),
        transport: SyncTransport::Rsync,
        rsync_exclude: vec!["/secret".into()],
        ..Default::default()
    };
    let mut f = FileConfig::default();
    f.hosts.insert("fixture".into(), h.clone());
    let config = root.join("config.toml");
    fs::write(&config, toml::to_string(&f).unwrap()).unwrap();
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let path = std::env::join_paths(paths).unwrap();
    let fetched = root.join("fetch with spaces");
    let run = |script: &str| {
        Command::new(env!("CARGO_BIN_EXE_buildq"))
            .current_dir(&repo)
            .env("PATH", &path)
            .env("BUILDQ_CONFIG", &config)
            .env("BUILDQ_DIR", root.join("queue"))
            .env_remove("BUILDQ_HELD")
            .args([
                "run",
                "--host",
                "fixture",
                "--fetch",
                "output with spaces",
                "--fetch-into",
            ])
            .arg(&fetched)
            .args(["--", "sh", "-c", script])
            .output()
            .unwrap()
    };
    let out = run("cat generated > 'output with spaces'; test ! -e secret");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(fetched.join("output with spaces")).unwrap(),
        "ignored generated source"
    );
    let wt = buildq::git::discover(&repo).unwrap();
    let l = buildq::remote::layout(h.root.as_ref().unwrap(), &wt);
    let remote = std::path::Path::new(&l.wt_dir);
    assert!(!remote.join(".git").exists());
    fs::create_dir_all(remote.join("target")).unwrap();
    fs::write(remote.join("target/keep"), "cache").unwrap();
    fs::remove_file(repo.join("tracked")).unwrap();
    let out = run(
        "test ! -e tracked && test -f target/keep || exit 38; printf second > 'output with spaces'; exit 37",
    );
    assert_eq!(
        out.status.code(),
        Some(37),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!remote.join("tracked").exists());
    assert!(remote.join("target/keep").is_file());
    assert!(repo.join(".git").is_dir());
}
