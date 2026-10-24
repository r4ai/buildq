//! End-to-end: the cargo shim with the real cargo on the sample workspace in
//! `tests/fixtures/sample-ws`, two git worktrees sharing one pool member.
//!
//! The compile-time path regression runs by default without dependencies.
//! The larger tests below are ignored by default; the sample workspace needs
//! the `regex` crate family in the local registry cache (`--offline`):
//!
//! ```text
//! cargo test --test e2e -- --ignored --nocapture
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime};

const BQ: &str = env!("CARGO_BIN_EXE_buildq");

#[test]
fn pooled_builds_refuse_an_unavailable_reserve_and_retry_cleanly() {
    let temp = tempfile::tempdir().unwrap();
    let root = buildq::util::canonical(temp.path());
    let w = tiny_world(
        &root,
        &[
            (
                "Cargo.toml",
                "[package]\nname='reserve-app'\nversion='0.1.0'\nedition='2021'\n",
            ),
            ("src/main.rs", "fn main() { println!(\"reserve-ok\"); }\n"),
        ],
    );
    git(&root.join("ws"), &["init", "-q"]);
    git(&root.join("ws"), &["add", "."]);
    git(&root.join("ws"), &["commit", "-qm", "fixture"]);
    fs::create_dir_all(root.join("q")).unwrap();
    let config = root.join("q/config.toml");
    let reserve = buildq::util::free_disk_bytes(&root).unwrap() + (1 << 30);
    fs::write(&config, format!("[cache]\nmin_free='{reserve}'\n")).unwrap();
    let run = |sub: &str| {
        Command::new(&w.shim_cargo)
            .args([sub, "--offline"])
            .current_dir(root.join("ws"))
            .env("PATH", &w.path)
            .env("BUILDQ_DIR", root.join("q"))
            .env("BUILDQ_CACHE", root.join("cache"))
            .env("BUILDQ_CONFIG", &config)
            .env("BUILDQ_GC", "0")
            .env("BUILDQ_REAL_CARGO", &w.real_cargo)
            .env_remove("BUILDQ_HELD")
            .env_remove("CARGO_TARGET_DIR")
            .output()
            .unwrap()
    };
    for sub in ["build", "run"] {
        let out = run(sub);
        assert_eq!(
            out.status.code(),
            Some(125),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stderr).contains("min_free"));
    }
    fs::write(&config, "[cache]\nmin_free='0'\n").unwrap();
    let out = run("run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stdout(&out), "reserve-ok");
    let status = Command::new(BQ)
        .args(["status", "--json"])
        .env("BUILDQ_DIR", root.join("q"))
        .env("BUILDQ_CONFIG", &config)
        .output()
        .unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    for class in status["classes"].as_array().unwrap() {
        assert!(class["waiters"].as_array().unwrap().is_empty());
        assert!(
            class["holders"]
                .as_array()
                .unwrap()
                .iter()
                .all(|h| h["busy"] == false)
        );
    }
}

#[test]
fn oversized_builds_warn_before_gc_discards_their_reusable_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let root = buildq::util::canonical(temp.path());
    let w = tiny_world(
        &root,
        &[
            (
                "Cargo.toml",
                "[package]\nname='oversized-app'\nversion='0.1.0'\nedition='2021'\n",
            ),
            ("src/main.rs", "fn main() { println!(\"oversized-ok\"); }\n"),
            (
                "build.rs",
                r#"fn main() {
            std::fs::write(std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("large"), vec![0u8; 2*1024*1024]).unwrap();
            println!("cargo:rerun-if-changed=build.rs");
        }"#,
            ),
        ],
    );
    let ws = root.join("ws");
    git(&ws, &["init", "-q"]);
    git(&ws, &["add", "."]);
    git(&ws, &["commit", "-qm", "fixture"]);
    fs::create_dir_all(root.join("q")).unwrap();
    fs::write(
        root.join("q/config.toml"),
        "slots=1\n[cargo]\nmirror_bins=false\n[cache]\nbudget='1M'\nmin_free='0'\n",
    )
    .unwrap();
    for sub in ["build", "run"] {
        let (out, _) = w.cargo(&ws, &[sub, "--offline"]);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("exceeds the entire cache budget"), "{err}");
        assert!(err.contains("next build will be cold"), "{err}");
        if sub == "run" {
            assert_eq!(stdout(&out), "oversized-ok");
        }
        let gc = Command::new(BQ)
            .args(["gc", "--auto", "--json"])
            .env("BUILDQ_DIR", root.join("q"))
            .env("BUILDQ_CONFIG", root.join("q/config.toml"))
            .env("BUILDQ_CACHE", root.join("cache"))
            .output()
            .unwrap();
        assert!(gc.status.success());
        let report: serde_json::Value = serde_json::from_slice(&gc.stdout).unwrap();
        assert_eq!(report["after"], 0, "{report}");
        assert_eq!(report["over_budget"], false);
    }
}

#[test]
fn copied_runs_honor_cargo_runner_config_environment_and_cli() {
    let temp = tempfile::tempdir().unwrap();
    let root = buildq::util::canonical(temp.path());
    let w = tiny_world(
        &root,
        &[
            (
                "Cargo.toml",
                "[package]\nname='runner-app'\nversion='0.1.0'\nedition='2021'\n",
            ),
            (
                "src/bin/app.rs",
                r#"fn main() {
            println!("{}|{}", std::env::var("BQ_RUNNER_TAG").unwrap_or_default(), std::env::args().skip(1).collect::<Vec<_>>().join("|"));
        }"#,
            ),
            (
                "src/bin/runner.rs",
                r#"fn main() {
            let mut args = std::env::args_os().skip(1);
            let tag = args.next().unwrap();
            let exe = args.next().unwrap();
            let status = std::process::Command::new(exe).args(args).env("BQ_RUNNER_TAG", tag).status().unwrap();
            std::process::exit(status.code().unwrap_or(125));
        }"#,
            ),
            (".gitignore", "target/\n"),
        ],
    );
    let ws = root.join("ws");
    git(&ws, &["init", "-q"]);
    git(&ws, &["add", "."]);
    git(&ws, &["commit", "-qm", "fixture"]);
    let own = root.join("runner-target");
    w.plain(&ws, &own, &["build", "--offline", "--bin", "runner"]);
    let runner = own.join("debug").join(if cfg!(windows) {
        "runner.exe"
    } else {
        "runner"
    });
    let triple = buildq::job::probe_toolchain(&w.real_cargo, None)
        .unwrap()
        .host;
    fs::create_dir_all(ws.join(".cargo")).unwrap();
    fs::create_dir_all(root.join("q")).unwrap();
    fs::write(root.join("q/config.toml"), "[cache]\nmin_free='0'\n").unwrap();
    let run = |extra: &[String], env_runner: Option<String>| {
        let mut cmd = Command::new(&w.shim_cargo);
        cmd.args(["run", "--offline", "--bin", "app"])
            .args(extra)
            .args(["--", "space arg", "tail"])
            .current_dir(&ws)
            .env("PATH", &w.path)
            .env("BUILDQ_DIR", root.join("q"))
            .env("BUILDQ_CACHE", root.join("cache"))
            .env("BUILDQ_CONFIG", root.join("q/config.toml"))
            .env("BUILDQ_GC", "0")
            .env_remove("BUILDQ_HELD")
            .env_remove("CARGO_TARGET_DIR");
        let key = format!(
            "CARGO_TARGET_{}_RUNNER",
            triple.to_uppercase().replace('-', "_")
        );
        if let Some(value) = env_runner {
            cmd.env(key, value);
        } else {
            cmd.env_remove(key);
        }
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        stdout(&out)
    };
    for key in [
        &triple,
        "cfg(all(not(target_os = \"none\"), any(unix, windows)))",
    ] {
        fs::write(
            ws.join(".cargo/config.toml"),
            format!(
                "[target.{}]\nrunner={}\n",
                serde_json::to_string(key).unwrap(),
                serde_json::json!([runner, "config tag"])
            ),
        )
        .unwrap();
        let (control, _) = w.plain(
            &ws,
            &own,
            &[
                "run",
                "--offline",
                "--bin",
                "app",
                "--",
                "space arg",
                "tail",
            ],
        );
        assert_eq!(stdout(&control), "config tag|space arg|tail");
        assert_eq!(run(&[], None), stdout(&control));
    }
    fs::write(ws.join(".cargo/config.toml"), "").unwrap();
    assert_eq!(
        run(&[], Some(format!("{} env-tag", runner.display()))),
        "env-tag|space arg|tail"
    );
    let cli = format!(
        "target.{}.runner={}",
        triple,
        serde_json::json!([runner, "cli tag"])
    );
    assert_eq!(
        run(
            &["--config".into(), cli],
            Some(format!("{} env-tag", runner.display()))
        ),
        "cli tag|space arg|tail"
    );
}

#[test]
fn run_manifest_path_selects_the_requested_member_instead_of_the_cwd_package() {
    let temp = tempfile::tempdir().unwrap();
    let root = buildq::util::canonical(temp.path());
    let w = tiny_world(
        &root,
        &[
            (
                "Cargo.toml",
                "[package]\nname='parent-app'\nversion='0.1.0'\nedition='2021'\n[workspace]\nmembers=['a']\nresolver='2'\n",
            ),
            ("src/main.rs", "fn main(){println!(\"parent-app\");}"),
            (
                "a/Cargo.toml",
                "[package]\nname='child-app'\nversion='0.1.0'\nedition='2021'\n",
            ),
            ("a/src/main.rs", "fn main(){println!(\"child-app\");}"),
            (".gitignore", "target/\n"),
        ],
    );
    let ws = root.join("ws");
    git(&ws, &["init", "-q"]);
    git(&ws, &["add", "."]);
    git(&ws, &["commit", "-qm", "fixture"]);
    let absolute = ws.join("a/Cargo.toml");
    for manifest in ["a/Cargo.toml", absolute.to_str().unwrap()] {
        let args = ["run", "--offline", "--manifest-path", manifest];
        let (plain, _) = w.plain(&ws, &root.join("control"), &args);
        assert_eq!(stdout(&plain), "child-app");
        let (pooled, _) = w.cargo(&ws, &args);
        assert_eq!(stdout(&pooled), stdout(&plain));
    }
    assert_eq!(stdout(&w.cargo(&ws, &["run", "--offline"]).0), "parent-app");
}

#[test]
fn ambiguous_runs_do_not_build_or_launch_an_arbitrary_workspace_binary() {
    let temp = tempfile::tempdir().unwrap();
    let root = buildq::util::canonical(temp.path());
    let w = tiny_world(
        &root,
        &[
            (
                "Cargo.toml",
                "[workspace]\nmembers=['a','b']\nresolver='2'\n",
            ),
            (
                "a/Cargo.toml",
                "[package]\nname='child-a'\nversion='0.1.0'\nedition='2021'\n",
            ),
            (
                "b/Cargo.toml",
                "[package]\nname='child-b'\nversion='0.1.0'\nedition='2021'\n",
            ),
            (
                "a/src/main.rs",
                "fn main(){std::fs::write(\"launched-a\",\"yes\").unwrap();println!(\"child-a\");}",
            ),
            (
                "b/src/main.rs",
                "fn main(){std::fs::write(\"launched-b\",\"yes\").unwrap();println!(\"child-b\");}",
            ),
            ("a/examples/demo.rs", "fn main(){println!(\"demo\");}"),
            (".gitignore", "target/\nlaunched-*\n"),
        ],
    );
    let ws = root.join("ws");
    git(&ws, &["init", "-q"]);
    git(&ws, &["add", "."]);
    git(&ws, &["commit", "-qm", "fixture"]);
    fs::create_dir_all(root.join("q")).unwrap();
    fs::write(root.join("q/config.toml"), "[cache]\nmin_free='0'\n").unwrap();
    let run = |args: &[&str]| {
        Command::new(&w.shim_cargo)
            .args(args)
            .current_dir(&ws)
            .env("PATH", &w.path)
            .env("BUILDQ_DIR", root.join("q"))
            .env("BUILDQ_CACHE", root.join("cache"))
            .env("BUILDQ_CONFIG", root.join("q/config.toml"))
            .env("BUILDQ_GC", "0")
            .env_remove("BUILDQ_HELD")
            .env_remove("CARGO_TARGET_DIR")
            .output()
            .unwrap()
    };
    let plain = Command::new(&w.real_cargo)
        .args(["run", "--offline"])
        .current_dir(&ws)
        .output()
        .unwrap();
    assert_eq!(plain.status.code(), Some(101));
    let out = run(&["run", "--offline"]);
    assert_eq!(
        out.status.code(),
        Some(101),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!ws.join("launched-a").exists() && !ws.join("launched-b").exists());
    assert!(
        !root.join("cache/pools").exists(),
        "ambiguous runs must be rejected before building"
    );
    for (args, expected) in [
        (vec!["run", "--offline", "--bin", "child-a"], "child-a"),
        (vec!["run", "--offline", "-p", "child-b"], "child-b"),
        (vec!["run", "--offline", "--example", "demo"], "demo"),
    ] {
        let (control, _) = w.plain(&ws, &root.join("control"), &args);
        let out = run(&args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(stdout(&out), expected);
        assert_eq!(stdout(&out), stdout(&control));
    }
    fs::write(
        ws.join("Cargo.toml"),
        "[workspace]\nmembers=['a','b']\ndefault-members=['b']\nresolver='2'\n",
    )
    .unwrap();
    assert_eq!(stdout(&w.cargo(&ws, &["run", "--offline"]).0), "child-b");
    fs::create_dir_all(ws.join("a/src/bin")).unwrap();
    fs::write(
        ws.join("a/src/bin/alternate.rs"),
        "fn main(){println!(\"alternate\");}",
    )
    .unwrap();
    fs::write(
        ws.join("a/Cargo.toml"),
        "[package]\nname='child-a'\nversion='0.1.0'\nedition='2021'\ndefault-run='child-a'\n",
    )
    .unwrap();
    assert_eq!(
        stdout(&w.cargo(&ws, &["run", "--offline", "-p", "child-a"]).0),
        "child-a"
    );
}

#[test]
#[cfg(unix)]
fn native_runtime_leases_survive_gc_while_queued_and_during_late_loading() {
    use std::process::Child;
    struct Children(Vec<Child>);
    impl Drop for Children {
        fn drop(&mut self) {
            for child in &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    fn wait(mut predicate: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(30);
        while !predicate() {
            assert!(Instant::now() < until, "native runtime barrier timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let root = buildq::util::canonical(temp.path());
    let w = tiny_world(
        &root,
        &[
            (
                "Cargo.toml",
                "[package]\nname='native-runtime'\nversion='0.1.0'\nedition='2021'\n",
            ),
            ("native.c", "int answer(void){return 42;}"),
            (
                "build.rs",
                r#"fn main() {
            let out = std::env::var("OUT_DIR").unwrap();
            let ext = if cfg!(target_os="macos") { "dylib" } else { "so" };
            for name in ["qa_native", "qa_late"] {
                let mut cc = std::process::Command::new("cc");
                cc.args(["-shared", "-fPIC", "native.c", "-o"])
                    .arg(std::path::Path::new(&out).join(format!("lib{name}.{ext}")));
                assert!(cc.status().unwrap().success());
            }
            println!("cargo:rustc-link-search=native={out}");
            println!("cargo:rustc-link-lib=dylib=qa_native");
            println!("cargo:rerun-if-changed=native.c");
        }"#,
            ),
            (
                "src/main.rs",
                r#"use std::{ffi::CString, path::Path, time::{Duration,Instant}};
            extern "C" { fn answer() -> i32; }
            #[cfg_attr(not(target_os="macos"), link(name="dl"))]
            extern "C" { fn dlopen(path:*const std::ffi::c_char, flags:i32)->*mut std::ffi::c_void; fn dlsym(handle:*mut std::ffi::c_void,name:*const std::ffi::c_char)->*mut std::ffi::c_void; }
            fn main() {
                assert_eq!(unsafe { answer() }, 42);
                let args:Vec<_> = std::env::args().collect();
                std::fs::write(&args[1], "ready").unwrap();
                let until=Instant::now()+Duration::from_secs(30);
                while !Path::new(&args[2]).exists() { assert!(Instant::now()<until); std::thread::sleep(Duration::from_millis(10)); }
                let lib = CString::new(if cfg!(target_os="macos") { "libqa_late.dylib" } else { "libqa_late.so" }).unwrap();
                unsafe {
                    let handle=dlopen(lib.as_ptr(),2); assert!(!handle.is_null());
                    let symbol=dlsym(handle,CString::new("answer").unwrap().as_ptr()); assert!(!symbol.is_null());
                    let late:unsafe extern "C" fn()->i32=std::mem::transmute(symbol);
                    println!("answer={} late={}",answer(),late());
                }
            }"#,
            ),
            (".gitignore", "target/\n"),
        ],
    );
    let ws = root.join("ws");
    git(&ws, &["init", "-q"]);
    git(&ws, &["add", "."]);
    git(&ws, &["commit", "-qm", "fixture"]);
    fs::create_dir_all(root.join("q")).unwrap();
    fs::write(
        root.join("q/config.toml"),
        "slots=1\nrun_slots=1\n[cargo]\nmirror_bins=false\n[cache]\nmin_free='0'\nbudget='1M'\n",
    )
    .unwrap();
    let command = || {
        let mut c = Command::new(BQ);
        c.env("BUILDQ_DIR", root.join("q"))
            .env("BUILDQ_CONFIG", root.join("q/config.toml"))
            .env("BUILDQ_CACHE", root.join("cache"))
            .env("PATH", &w.path)
            .env_remove("BUILDQ_HELD")
            .env_remove("CARGO_TARGET_DIR");
        c
    };
    let mut children = Children(Vec::new());
    children.0.push(
        command()
            .args(["run", "--class", "run", "--", BQ, "__helper", "gate"])
            .arg(root.join("holder-ready"))
            .arg(root.join("holder-release"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait(|| root.join("holder-ready").exists());
    children.0.push(
        command()
            .args(["cargo", "run", "--offline", "--"])
            .arg(root.join("app-ready"))
            .arg(root.join("app-release"))
            .env("BUILDQ_GC", "1")
            .current_dir(&ws)
            .stdout(fs::File::create(root.join("app.out")).unwrap())
            .stderr(fs::File::create(root.join("app.err")).unwrap())
            .spawn()
            .unwrap(),
    );
    wait(|| {
        fs::read_to_string(root.join("app.err"))
            .unwrap_or_default()
            .contains("waiting for a run slot")
    });
    wait(|| root.join("cache/gc.stamp").exists());
    let pool = fs::read_dir(root.join("cache/pools"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(
        pool.join("m0").exists(),
        "queued native dependencies must remain leased"
    );
    let gc = || {
        let out = command().args(["gc", "--json"]).output().unwrap();
        assert!(out.status.success());
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };
    assert_eq!(gc()["over_budget"], true);
    fs::write(root.join("holder-release"), "").unwrap();
    assert!(children.0[0].wait().unwrap().success());
    wait(|| root.join("app-ready").exists() || children.0[1].try_wait().unwrap().is_some());
    assert!(
        root.join("app-ready").exists(),
        "{}",
        fs::read_to_string(root.join("app.err")).unwrap()
    );
    assert_eq!(gc()["over_budget"], true);
    assert!(
        pool.join("m0").exists(),
        "active runtime dependencies must survive GC"
    );
    fs::write(root.join("app-release"), "").unwrap();
    assert!(
        children.0[1].wait().unwrap().success(),
        "{}",
        fs::read_to_string(root.join("app.err")).unwrap()
    );
    assert_eq!(
        fs::read_to_string(root.join("app.out")).unwrap().trim(),
        "answer=42 late=42"
    );
    wait(|| !pool.join("m0").exists() && !buildq::lock::is_held(&root.join("cache/gc.lock")));
}

#[test]
fn nested_helper_workspaces_do_not_inherit_the_parent_pool_target() {
    let temp = tempfile::tempdir().unwrap();
    let root = buildq::util::canonical(temp.path());
    let script = |manifest: &str| {
        format!(
            r#"fn main() {{
        let mut child=std::process::Command::new(std::env::var("CARGO").unwrap())
            .args(["build","--offline","--manifest-path","{manifest}"]).spawn().unwrap();
        let until=std::time::Instant::now()+std::time::Duration::from_secs(15);
        loop {{
            if let Some(status)=child.try_wait().unwrap() {{ assert!(status.success()); break; }}
            if std::time::Instant::now()>=until {{ let _=child.kill();let _=child.wait();panic!("nested Cargo target lock timed out"); }}
            std::thread::sleep(std::time::Duration::from_millis(10));
        }}
        println!("cargo:rerun-if-changed=build.rs");
    }}"#
        )
    };
    let parent_script = script("helper/Cargo.toml");
    let helper_script = script("grand/Cargo.toml");
    let w = tiny_world(
        &root,
        &[
            (
                "Cargo.toml",
                "[package]\nname='nested-parent'\nversion='0.1.0'\nedition='2021'\n[workspace]\nexclude=['helper']\n",
            ),
            ("src/main.rs", "fn main(){}"),
            ("build.rs", &parent_script),
            (
                "helper/Cargo.toml",
                "[package]\nname='nested-helper'\nversion='0.1.0'\nedition='2021'\n[workspace]\nexclude=['grand']\n",
            ),
            ("helper/src/main.rs", "fn main(){}"),
            ("helper/build.rs", &helper_script),
            (
                "helper/grand/Cargo.toml",
                "[package]\nname='nested-grand'\nversion='0.1.0'\nedition='2021'\n[workspace]\n",
            ),
            ("helper/grand/src/main.rs", "fn main(){}"),
            (".gitignore", "**/target/\n"),
        ],
    );
    let ws = root.join("ws");
    git(&ws, &["init", "-q"]);
    git(&ws, &["add", "."]);
    git(&ws, &["commit", "-qm", "fixture"]);
    let control = Command::new(&w.real_cargo)
        .args(["build", "--offline", "--target-dir"])
        .arg(root.join("control"))
        .current_dir(&ws)
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .unwrap();
    assert!(
        control.status.success(),
        "{}",
        String::from_utf8_lossy(&control.stderr)
    );
    for dir in [ws.join("helper/target"), ws.join("helper/grand/target")] {
        fs::remove_dir_all(dir).unwrap();
    }
    w.cargo(&ws, &["build", "--offline"]);
    for dir in [ws.join("helper/target"), ws.join("helper/grand/target")] {
        assert!(dir.exists());
    }
    let report = Command::new(BQ)
        .args(["cache", "ls", "--refresh", "--json"])
        .env("BUILDQ_DIR", root.join("q"))
        .env("BUILDQ_CONFIG", root.join("q/config.toml"))
        .env("BUILDQ_CACHE", root.join("cache"))
        .output()
        .unwrap();
    let usage: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
    assert!(usage["external_total"].as_u64().unwrap() > 0);
    assert_eq!(usage["external"].as_array().unwrap().len(), 2, "{usage}");
}

#[test]
#[cfg(unix)]
fn a_real_cargo_target_fitting_the_disk_budget_stays_warm_after_gc() {
    let temp = tempfile::tempdir().unwrap();
    let root = buildq::util::canonical(temp.path());
    let w = tiny_world(
        &root,
        &[
            (
                "Cargo.toml",
                "[package]\nname='hardlink-app'\nversion='0.1.0'\nedition='2021'\n",
            ),
            ("src/main.rs", "fn main(){println!(\"ok\")}"),
            (".gitignore", "target/\n"),
        ],
    );
    let ws = root.join("ws");
    assert!(
        Command::new(&w.real_cargo)
            .args(["generate-lockfile", "--offline"])
            .current_dir(&ws)
            .output()
            .unwrap()
            .status
            .success()
    );
    git(&ws, &["init", "-q"]);
    git(&ws, &["add", "."]);
    git(&ws, &["commit", "-qm", "fixture"]);
    w.cargo(&ws, &["build", "--offline"]);
    let pool = fs::read_dir(root.join("cache/pools"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let member = pool.join("m0");
    let du = Command::new("du").arg("-sk").arg(&member).output().unwrap();
    assert!(du.status.success());
    let allocated = String::from_utf8(du.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse::<u64>()
        .unwrap()
        * 1024;
    // du includes directory metadata; leave a small margin without allowing
    // the old double-counted executable footprint to fit.
    let budget = allocated + 1024 * 1024;
    let gc = Command::new(BQ)
        .args(["gc", "--json", "--budget"])
        .arg(budget.to_string())
        .env("BUILDQ_DIR", root.join("q"))
        .env("BUILDQ_CONFIG", root.join("q/config.toml"))
        .env("BUILDQ_CACHE", root.join("cache"))
        .output()
        .unwrap();
    assert!(gc.status.success());
    let report: serde_json::Value = serde_json::from_slice(&gc.stdout).unwrap();
    assert_eq!(report["accounting"], "allocated");
    assert!(report["before"].as_u64().unwrap() <= allocated, "{report}");
    assert_eq!(report["freed"], 0, "{report}");
    assert!(member.exists());
    let (warm, _) = w.cargo(&ws, &["build", "--offline", "--verbose"]);
    assert_eq!(
        compiled(&warm),
        0,
        "{}",
        String::from_utf8_lossy(&warm.stderr)
    );
}

#[test]
fn project_cache_clean_is_explicit_scoped_and_respects_live_leases() {
    let temp = tempfile::tempdir().unwrap();
    let root = buildq::util::canonical(temp.path());
    let w = tiny_world(
        &root,
        &[
            (
                "Cargo.toml",
                "[package]\nname='clean-app'\nversion='0.1.0'\nedition='2021'\n",
            ),
            ("src/main.rs", "fn main(){}"),
            (".gitignore", "target/\n"),
        ],
    );
    let ws = root.join("ws");
    assert!(
        Command::new(&w.real_cargo)
            .args(["generate-lockfile", "--offline"])
            .current_dir(&ws)
            .output()
            .unwrap()
            .status
            .success()
    );
    git(&ws, &["init", "-q"]);
    git(&ws, &["add", "."]);
    git(&ws, &["commit", "-qm", "fixture"]);
    w.cargo(&ws, &["build", "--offline"]);
    let pool = fs::read_dir(root.join("cache/pools"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let (local_clean, _) = w.cargo(&ws, &["clean", "--offline"]);
    assert!(String::from_utf8_lossy(&local_clean.stderr).contains("pooled caches are unchanged"));
    assert!(pool.join("m0").exists());
    assert_eq!(
        compiled(&w.cargo(&ws, &["build", "--offline", "--verbose"]).0),
        0
    );
    let command = || {
        let mut c = Command::new(BQ);
        c.current_dir(&ws)
            .env("BUILDQ_DIR", root.join("q"))
            .env("BUILDQ_CONFIG", root.join("q/config.toml"))
            .env("BUILDQ_CACHE", root.join("cache"))
            .env("BUILDQ_GC", "0")
            .env("PATH", &w.path)
            .env_remove("BUILDQ_HELD")
            .env_remove("CARGO_TARGET_DIR");
        c
    };
    let other = root.join("other");
    fs::create_dir_all(other.join("src")).unwrap();
    fs::write(
        other.join("Cargo.toml"),
        "[package]\nname='other-app'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(other.join("src/main.rs"), "fn main(){}").unwrap();
    git(&other, &["init", "-q"]);
    git(&other, &["add", "."]);
    git(&other, &["commit", "-qm", "fixture"]);
    assert!(
        command()
            .current_dir(&other)
            .args(["cargo", "build", "--offline"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let other_pool = fs::read_dir(root.join("cache/pools"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p != &pool)
        .unwrap();
    fs::create_dir_all(root.join("cache/runs/r9")).unwrap();
    fs::write(root.join("cache/runs/r9/copied-app"), "keep").unwrap();
    let held = buildq::lock::lock(&pool.join("m0.lock")).unwrap();
    let busy = command()
        .args(["cache", "clean", "--json"])
        .output()
        .unwrap();
    assert_eq!(busy.status.code(), Some(1));
    assert!(pool.join("m0").exists());
    let unsafe_direct = command()
        .args(["cargo", "clean", "--target-dir"])
        .arg(pool.join("m0"))
        .output()
        .unwrap();
    assert_eq!(unsafe_direct.status.code(), Some(125));
    assert!(pool.join("m0").exists());
    fs::create_dir_all(ws.join(".cargo")).unwrap();
    fs::write(
        ws.join(".cargo/config.toml"),
        format!(
            "[build]\ntarget-dir={}\n",
            serde_json::to_string(&pool.join("m0").to_string_lossy()).unwrap()
        ),
    )
    .unwrap();
    assert_eq!(
        command()
            .args(["cargo", "clean"])
            .output()
            .unwrap()
            .status
            .code(),
        Some(125)
    );
    let explicit_local = command()
        .args(["cargo", "clean", "--target-dir"])
        .arg(root.join("local-only"))
        .output()
        .unwrap();
    assert!(
        explicit_local.status.success(),
        "{}",
        String::from_utf8_lossy(&explicit_local.stderr)
    );
    assert!(pool.join("m0").exists());
    fs::remove_file(ws.join(".cargo/config.toml")).unwrap();
    drop(held);
    let preview = command()
        .args(["cache", "clean", "--dry-run", "--json"])
        .output()
        .unwrap();
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let preview: serde_json::Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert!(preview["freed"].as_u64().unwrap() > 0);
    assert!(pool.join("m0").exists());
    let actual = command()
        .args(["cache", "clean", "--json"])
        .output()
        .unwrap();
    assert!(
        actual.status.success(),
        "{}",
        String::from_utf8_lossy(&actual.stderr)
    );
    let actual: serde_json::Value = serde_json::from_slice(&actual.stdout).unwrap();
    assert_eq!(actual["freed"], preview["freed"]);
    assert!(!pool.join("m0").exists());
    assert!(pool.join("m0.lock").exists());
    assert!(pool.join("pool.lock").exists());
    assert!(other_pool.join("m0").exists());
    assert!(root.join("cache/runs/r9/copied-app").exists());
    assert!(compiled(&w.cargo(&ws, &["build", "--offline", "--verbose"]).0) > 0);
    let outside = command()
        .current_dir(&root)
        .args(["cache", "clean", "--json"])
        .output()
        .unwrap();
    assert_eq!(outside.status.code(), Some(125));
    let all = command()
        .current_dir(&root)
        .args(["cache", "clean", "--all", "--json"])
        .output()
        .unwrap();
    assert!(
        all.status.success(),
        "{}",
        String::from_utf8_lossy(&all.stderr)
    );
    assert!(!pool.join("m0").exists());
    assert!(!other_pool.join("m0").exists());
    assert!(other_pool.join("pool.lock").exists());
    assert!(root.join("cache/runs/r9/copied-app").exists());
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap().flatten() {
        let p = e.path();
        if e.file_name() == "target" {
            continue;
        }
        if p.is_dir() {
            copy_dir(&p, &to.join(e.file_name()));
        } else {
            fs::copy(&p, to.join(e.file_name())).unwrap();
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

struct World {
    root: PathBuf,
    shim_cargo: PathBuf,
    real_cargo: PathBuf,
    path: std::ffi::OsString,
}

impl World {
    /// The shim invocation in the isolated queue/cache environment.
    fn command(&self, dir: &Path, args: &[&str]) -> Command {
        let config = self.root.join("q/config.toml");
        if !config.exists() {
            fs::create_dir_all(config.parent().unwrap()).unwrap();
            fs::write(&config, "[cache]\nmin_free='0'\n").unwrap();
        }
        let mut cmd = Command::new(&self.shim_cargo);
        cmd.args(args)
            .current_dir(dir)
            .env("PATH", &self.path)
            .env("BUILDQ_DIR", self.root.join("q"))
            .env("BUILDQ_CONFIG", &config)
            .env("BUILDQ_CACHE", self.root.join("cache"))
            .env("BUILDQ_GC", "0")
            .env_remove("BUILDQ_HELD")
            .env_remove("CARGO_TARGET_DIR")
            // CI forces colour; `compiled` matches plain "Compiling" lines.
            .env("CARGO_TERM_COLOR", "never");
        cmd
    }

    fn cargo(&self, dir: &Path, args: &[&str]) -> (Output, Duration) {
        let t = Instant::now();
        let out = self.command(dir, args).output().unwrap();
        let d = t.elapsed();
        assert!(
            out.status.success(),
            "cargo {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        (out, d)
    }

    fn plain(&self, dir: &Path, target: &Path, args: &[&str]) -> (Output, Duration) {
        let t = Instant::now();
        let out = Command::new(&self.real_cargo)
            .args(args)
            .current_dir(dir)
            .env("CARGO_TARGET_DIR", target)
            .env("RUSTC_WRAPPER", "")
            .env("CARGO_BUILD_JOBS", "4")
            .output()
            .unwrap();
        let d = t.elapsed();
        assert!(
            out.status.success(),
            "plain cargo {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        (out, d)
    }
}

fn compiled(out: &Output) -> usize {
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| l.trim_start().starts_with("Compiling"))
        .count()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn set_old_mtime(p: &Path) {
    let t = SystemTime::now() - Duration::from_secs(5 * 365 * 86_400);
    fs::File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

#[test]
#[ignore = "compiles a small real workspace; run with --ignored"]
fn two_worktrees_share_a_pool_member() {
    let tmp = tempfile::tempdir().unwrap();
    let root = buildq::util::canonical(tmp.path());
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample-ws");
    let wt1 = root.join("wt1");
    let wt2 = root.join("wt2");
    copy_dir(&fixture, &wt1);
    git(&wt1, &["init", "-q"]);
    git(&wt1, &["add", "-A"]);
    git(&wt1, &["commit", "-q", "-m", "init"]);
    git(&wt1, &["worktree", "add", "-q", wt2.to_str().unwrap()]);

    let shim = root.join("shim");
    let st = Command::new(BQ)
        .args(["shim", "install", "cargo", "--into"])
        .arg(&shim)
        .output()
        .unwrap();
    assert!(st.status.success());
    let mut paths = vec![shim.clone()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let real_cargo = buildq::cargo::find_real_cargo(None, Some(&shim)).expect("real cargo");
    let w = World {
        shim_cargo: shim.join(if cfg!(windows) { "cargo.exe" } else { "cargo" }),
        path: std::env::join_paths(paths).unwrap(),
        real_cargo,
        root: root.clone(),
    };
    let build = ["build", "--offline"];
    let mut rows: Vec<(String, Duration, usize)> = Vec::new();

    // 1. Cold build in worktree 1: a new pool member.
    let (o, d) = w.cargo(&wt1, &build);
    assert!(String::from_utf8_lossy(&o.stderr).contains("new member"));
    rows.push(("wt1 cold build (new member)".into(), d, compiled(&o)));

    // 2. Worktree 2, same commit: local crates rebuild with current paths;
    //    registry dependencies stay cached.
    let (o, d2) = w.cargo(&wt2, &build);
    rows.push(("wt2 first build, shared member".into(), d2, compiled(&o)));
    assert_eq!(compiled(&o), 3, "{}", String::from_utf8_lossy(&o.stderr));

    // Control: the same first build with a target dir of its own.
    let (o, dc) = w.plain(&wt2, &root.join("own-target"), &build);
    rows.push(("control: wt2 with its own target/".into(), dc, compiled(&o)));

    // 3. Edit the bottom crate in wt2 and give it an old mtime: only the
    //    workspace crates rebuild, and the result reflects wt2's source.
    let lib = wt2.join("crates/core/src/lib.rs");
    fs::write(
        &lib,
        fs::read_to_string(&lib)
            .unwrap()
            .replace("greeting v1", "greeting v2"),
    )
    .unwrap();
    set_old_mtime(&lib);
    let (o, d) = w.cargo(
        &wt2,
        &["run", "--offline", "-p", "sample-app", "--", "a", "b", "c"],
    );
    assert_eq!(stdout(&o), "greeting v2 (3 words)");
    rows.push(("wt2 edit core + run (old mtime)".into(), d, compiled(&o)));

    // 4. Back to wt1 (v1): the member flips back by rebuilding 3 small crates only.
    let (o, d) = w.cargo(&wt1, &["--offline", "sample", "x"]);
    assert!(
        stdout(&o).ends_with("greeting v1 (1 words)"),
        "{}",
        stdout(&o)
    );
    rows.push(("wt1 run via alias after wt2 edit".into(), d, compiled(&o)));

    // Control for 3: plain cargo sharing one target dir silently runs stale code.
    let shared = root.join("naive-shared");
    w.plain(&wt1, &shared, &["build", "--offline", "-q"]);
    set_old_mtime(&lib);
    let (o, _) = w.plain(
        &wt2,
        &shared,
        &[
            "run",
            "--offline",
            "-q",
            "-p",
            "sample-app",
            "--",
            "a",
            "b",
            "c",
        ],
    );
    let naive = stdout(&o);

    let size = |p: &Path| buildq::util::dir_size(p);
    let pools = root.join("cache/pools");
    let member = fs::read_dir(&pools)
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .path()
        .join("m0");
    println!("\n{:<38} {:>9} {:>9}", "step", "wall", "compiled");
    for (name, d, n) in &rows {
        println!("{name:<38} {:>8.2}s {n:>9}", d.as_secs_f64());
    }
    println!(
        "\npool member m0: {:.1} MiB for both worktrees; one own target/: {:.1} MiB",
        size(&member) as f64 / 1048576.0,
        size(&root.join("own-target")) as f64 / 1048576.0
    );
    println!("naive shared CARGO_TARGET_DIR after the same edit prints: {naive:?} (stale)");
    assert!(d2 < dc, "shared member must beat a cold own target dir");
}

/// Writes a throwaway workspace (no registry deps) and returns the shim env.
fn tiny_world(root: &Path, files: &[(&str, &str)]) -> World {
    for (name, text) in files {
        let p = root.join("ws").join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    let shim = root.join("shim");
    let st = Command::new(BQ)
        .args(["shim", "install", "cargo", "--into"])
        .arg(&shim)
        .output()
        .unwrap();
    assert!(st.status.success());
    let mut paths = vec![shim.clone()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    World {
        shim_cargo: shim.join(if cfg!(windows) { "cargo.exe" } else { "cargo" }),
        path: std::env::join_paths(paths).unwrap(),
        real_cargo: buildq::cargo::find_real_cargo(None, Some(&shim)).expect("real cargo"),
        root: root.to_path_buf(),
    }
}

const DYLIB_WS: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[workspace]\nmembers = [\"dyl\", \"app\"]\nresolver = \"2\"\n",
    ),
    // `prefer-dynamic` makes every dylib (and the app) need the toolchain's
    // std shared library at run time, as `bevy_dylib` does.
    (
        ".cargo/config.toml",
        "[build]\nrustflags = [\"-C\", \"prefer-dynamic\"]\n\n[alias]\ngo = [\"run\", \"-p\", \"app\"]\n",
    ),
    (
        "dyl/Cargo.toml",
        "[package]\nname = \"dyl\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\ncrate-type = [\"dylib\"]\n",
    ),
    ("dyl/src/lib.rs", "pub fn answer() -> u32 { 42 }\n"),
    (
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ndyl = { path = \"../dyl\" }\n",
    ),
    (
        "app/src/main.rs",
        "fn main() { println!(\"answer={}\", dyl::answer()); }\n",
    ),
];

/// A binary linked against a `dylib` crate and the shared std starts from the
/// copied run dir, through `cargo run` and through a run alias, with a `PATH`
/// that holds nothing of the toolchain.
#[test]
#[ignore = "compiles a tiny real workspace; run with --ignored"]
fn run_starts_a_dylib_linked_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let root = buildq::util::canonical(tmp.path());
    let w = tiny_world(&root, DYLIB_WS);
    let ws = root.join("ws");
    for args in [["run", "-p", "app"], ["go", "--", ""]] {
        let args: Vec<&str> = args.iter().copied().filter(|a| !a.is_empty()).collect();
        let (o, _) = w.cargo(&ws, &args);
        assert_eq!(stdout(&o), "answer=42", "{args:?}");
    }
}

/// A shared library that a build script drops into `deps` (as `ort` and the
/// like do) is not an artifact of any compile unit; `cargo run` finds it
/// through `deps` on the library path, and so must the copied binary.
#[cfg(all(windows, target_env = "msvc"))]
#[test]
#[ignore = "compiles a tiny real workspace; run with --ignored"]
fn run_finds_a_dll_a_build_script_left_in_deps() {
    let tmp = tempfile::tempdir().unwrap();
    let root = buildq::util::canonical(tmp.path());
    let files: &[(&str, &str)] = &[
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"app\"]\nresolver = \"2\"\n",
        ),
        (
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n",
        ),
        (
            "app/native.rs",
            "#[no_mangle]\npub extern \"C\" fn native_answer() -> u32 { 42 }\n",
        ),
        (
            "app/build.rs",
            r#"use std::{env, fs, path::PathBuf, process::Command};
fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let lib = out.join("implib");
    fs::create_dir_all(&lib).unwrap();
    let src = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("native.rs");
    let dll = out.join("native.dll");
    let st = Command::new(env::var("RUSTC").unwrap())
        .args(["--crate-type", "cdylib", "--crate-name", "native", "--edition", "2021"])
        .arg("-o").arg(&dll).arg(&src)
        .status().unwrap();
    assert!(st.success());
    // The import library goes to its own dir, the DLL only to `deps`: the
    // build script's linked paths then do not lead to the DLL.
    fs::rename(out.join("native.dll.lib"), lib.join("native.lib")).unwrap();
    let deps = out.ancestors().nth(3).unwrap().join("deps");
    fs::copy(&dll, deps.join("native.dll")).unwrap();
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=dylib=native");
    println!("cargo:rerun-if-changed=native.rs");
}
"#,
        ),
        (
            "app/src/main.rs",
            "extern \"C\" { fn native_answer() -> u32; }\nfn main() { println!(\"answer={}\", unsafe { native_answer() }); }\n",
        ),
    ];
    let w = tiny_world(&root, files);
    let (o, _) = w.cargo(&root.join("ws"), &["run", "-p", "app"]);
    assert_eq!(stdout(&o), "answer=42");
}

/// Identical source must not retain another worktree's compile-time paths.
/// No registry dependencies: this regression runs in the normal test suite.
#[test]
fn pooled_compile_time_paths_follow_the_current_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let root = buildq::util::canonical(tmp.path());
    let w = tiny_world(
        &root,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"paths\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            (".gitignore", "data.txt\n"),
            (
                "Cargo.lock",
                "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"paths\"\nversion = \"0.1.0\"\n",
            ),
            (
                "src/lib.rs",
                r#"#[test]
fn reads_own_data() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    assert_eq!(std::fs::read_to_string(root.join("data.txt")).unwrap(),
               std::env::var("EXPECTED_DATA").unwrap());
    assert_eq!(root, std::env::current_dir().unwrap());
}

"#,
            ),
        ],
    );
    let a = root.join("ws");
    let b = root.join("other");
    git(&a, &["init", "-q"]);
    git(&a, &["add", "-A"]);
    git(&a, &["commit", "-q", "-m", "init"]);
    git(
        &a,
        &["worktree", "add", "-q", "--detach", b.to_str().unwrap()],
    );
    fs::write(a.join("data.txt"), "a").unwrap();
    fs::write(b.join("data.txt"), "b").unwrap();
    for (dir, expected, rebuilds) in [(&a, "a", 1), (&b, "b", 1), (&b, "b", 0), (&a, "a", 1)] {
        let (out, _) = w.cargo(
            dir,
            &[
                "test",
                "--offline",
                "--config",
                &format!("env.EXPECTED_DATA=\"{expected}\""),
            ],
        );
        assert_eq!(
            compiled(&out),
            rebuilds,
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stderr).contains("/m0 ("));
    }
}

/// Compare the launched environment with Cargo itself (no registry dependencies).
#[test]
fn run_applies_effective_cargo_config_environment() {
    let tmp = tempfile::tempdir().unwrap();
    let root = buildq::util::canonical(tmp.path());
    let w = tiny_world(
        &root,
        &[
            (
                "Cargo.toml",
                "[package]\nname='env-app'\nversion='0.1.0'\nedition='2021'\n",
            ),
            (
                "src/main.rs",
                r#"fn main() {
            for name in ["BQ_DEFAULT", "BQ_KEEP", "BQ_FORCE", "BQ_RELATIVE", "BQ_HOME", "BQ_ORIGIN", "BQ_CLI"] {
                println!("{name}={}", std::env::var(name).unwrap());
            }
        }"#,
            ),
            (".cargo/config.toml", "[env]\nBQ_DEFAULT='ignored'\n"),
            (
                ".cargo/config",
                "[env]\nBQ_DEFAULT='local'\nBQ_KEEP='config'\nBQ_FORCE={value='forced',force=true}\nBQ_RELATIVE={value='assets',relative=true}\nBQ_ORIGIN={force=true}\nBQ_CLI={value='local'}\n",
            ),
            (
                ".cargo/extra.toml",
                "[env]\nBQ_CLI={value='extra',relative=true}\n",
            ),
        ],
    );
    let home = root.join("cargo-home");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        home.join("config.toml"),
        "[env]\nBQ_HOME='home'\nBQ_DEFAULT='home'\nBQ_ORIGIN={value='origin',relative=true}\n",
    )
    .unwrap();
    let args = [
        "run",
        "--offline",
        "--config",
        ".cargo/extra.toml",
        "--config=env.BQ_CLI.value='cli'",
    ];
    let run = |cargo: &Path, shim: bool| {
        let mut cmd = Command::new(cargo);
        cmd.args(args)
            .current_dir(root.join("ws"))
            .env("PATH", &w.path)
            .env("CARGO_HOME", &home)
            .env("CARGO_TARGET_DIR", root.join("target"))
            .env("BUILDQ_DIR", root.join("q"))
            .env("BUILDQ_CACHE", root.join("cache"))
            .env("BUILDQ_GC", "0")
            .env("BUILDQ_POOL", "0")
            .env("BUILDQ_REAL_CARGO", &w.real_cargo)
            .env("BQ_KEEP", "inherited")
            .env("BQ_FORCE", "inherited")
            .env_remove("BUILDQ_HELD");
        if !shim {
            cmd.env("RUSTC_WRAPPER", "");
        }
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        stdout(&out)
    };
    let expected = run(&w.real_cargo, false);
    assert!(expected.contains("BQ_DEFAULT=local"));
    assert!(expected.contains("BQ_KEEP=inherited"));
    assert!(expected.contains("BQ_FORCE=forced"));
    assert_eq!(run(&w.shim_cargo, true), expected);
}

/// A path dependency in a Git submodule (with a nested submodule) follows each
/// worktree's own submodule contents: dirty edits, new submodule commits and
/// nested edits, with normal timestamps and one shared pool member.
#[test]
fn pooled_builds_follow_each_worktrees_submodule_sources() {
    let tmp = tempfile::tempdir().unwrap();
    let root = buildq::util::canonical(tmp.path());
    let w = tiny_world(
        &root,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"qa-submodule-app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nqa-subdep = { path = \"dep\" }\n\n[workspace]\nmembers = [\"dep\"]\n",
            ),
            (
                "src/main.rs",
                "fn main() { println!(\"{}-{}\", qa_subdep::value(), qa_subdep::nested()); }\n",
            ),
        ],
    );
    let lib = |v: &str| {
        format!(
            "pub fn value() -> &'static str {{ \"{v}\" }}\npub fn nested() -> &'static str {{ qa_nested::value() }}\n"
        )
    };
    let commit = |dir: &Path| {
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", "c"]);
    };
    let sub = |dir: &Path, args: &[&str]| {
        let mut all = vec!["-c", "protocol.file.allow=always"];
        all.extend_from_slice(args);
        git(dir, &all);
    };
    let nested = root.join("nested-origin");
    fs::create_dir_all(nested.join("src")).unwrap();
    fs::write(
        nested.join("Cargo.toml"),
        "[package]\nname = \"qa-nested\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        nested.join("src/lib.rs"),
        "pub fn value() -> &'static str { \"N1\" }\n",
    )
    .unwrap();
    git(&nested, &["init", "-q"]);
    commit(&nested);
    let dep = root.join("dep-origin");
    fs::create_dir_all(dep.join("src")).unwrap();
    fs::write(
        dep.join("Cargo.toml"),
        "[package]\nname = \"qa-subdep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nqa-nested = { path = \"nested\" }\n",
    )
    .unwrap();
    fs::write(dep.join("src/lib.rs"), lib("A")).unwrap();
    git(&dep, &["init", "-q"]);
    sub(
        &dep,
        &["submodule", "add", "-q", nested.to_str().unwrap(), "nested"],
    );
    commit(&dep);

    let a = root.join("ws");
    let b = root.join("other");
    git(&a, &["init", "-q"]);
    sub(
        &a,
        &["submodule", "add", "-q", dep.to_str().unwrap(), "dep"],
    );
    sub(&a, &["submodule", "update", "-q", "--init", "--recursive"]);
    w.plain(
        &a,
        &root.join("lock-target"),
        &["generate-lockfile", "--offline"],
    );
    commit(&a);
    git(
        &a,
        &["worktree", "add", "-q", "--detach", b.to_str().unwrap()],
    );
    sub(&b, &["submodule", "update", "-q", "--init", "--recursive"]);
    // B is edited before A builds, so Cargo's mtime check cannot help.
    fs::write(b.join("dep/src/lib.rs"), lib("B")).unwrap();
    std::thread::sleep(Duration::from_secs(1));

    let run = |dir: &Path, expected: &str| {
        let (out, _) = w.cargo(dir, &["run", "--offline", "-p", "qa-submodule-app"]);
        assert_eq!(
            stdout(&out),
            expected,
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stderr).contains("/m0 ("));
    };
    run(&a, "A-N1");
    run(&b, "B-N1");
    run(&a, "A-N1");
    // Successive dirty edits, in the submodule and in the nested one.
    fs::write(b.join("dep/src/lib.rs"), lib("B2")).unwrap();
    run(&b, "B2-N1");
    fs::write(
        b.join("dep/nested/src/lib.rs"),
        "pub fn value() -> &'static str { \"N2\" }\n",
    )
    .unwrap();
    run(&b, "B2-N2");
    run(&a, "A-N1");
    // A new submodule commit, with a clean checkout.
    fs::write(a.join("dep/src/lib.rs"), lib("C")).unwrap();
    commit(&a.join("dep"));
    commit(&a);
    run(&a, "C-N1");
    run(&b, "B2-N2");
    // Control: plain Cargo with an independent target dir agrees.
    let (out, _) = w.plain(
        &b,
        &root.join("plain-target"),
        &["run", "--offline", "-p", "qa-submodule-app"],
    );
    assert_eq!(stdout(&out), "B2-N2");
}

/// Kills still-running children when a test fails before releasing them.
struct Children(Vec<std::process::Child>);

impl Drop for Children {
    fn drop(&mut self) {
        for c in &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A build that waits for its repository's busy member must not hold a build
/// slot: an independent repository builds meanwhile, and the waiters resume
/// once the native applications that retain the members exit.
#[cfg(unix)]
fn member_wait_frees_slots(slots: usize) {
    let tmp = tempfile::tempdir().unwrap();
    let root = buildq::util::canonical(tmp.path());
    let w = tiny_world(
        &root,
        &[
            (
                "Cargo.toml",
                "[package]\nname='qa'\nversion='0.1.0'\nedition='2021'\nbuild='build.rs'\n",
            ),
            (
                "native.rs",
                "#[no_mangle]\npub extern \"C\" fn native_answer() -> u32 { 42 }\n",
            ),
            (
                "build.rs",
                r#"use std::{env, path::PathBuf, process::Command};
fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let src = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("native.rs");
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let st = Command::new(env::var("RUSTC").unwrap())
        .args(["--crate-type", "cdylib", "--crate-name", "qa_native", "--edition", "2021"])
        .arg("-o").arg(out.join(format!("libqa_native.{ext}"))).arg(&src)
        .status().unwrap();
    assert!(st.success());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=dylib=qa_native");
    println!("cargo:rerun-if-changed=native.rs");
}
"#,
            ),
            (
                "src/main.rs",
                r#"extern "C" { fn native_answer() -> u32; }
fn main() {
    let a: Vec<String> = std::env::args().collect();
    std::fs::write(&a[1], unsafe { native_answer() }.to_string()).unwrap();
    while !std::path::Path::new(&a[2]).exists() {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
"#,
            ),
        ],
    );
    let a = root.join("ws");
    let b = root.join("other");
    fs::create_dir_all(b.join("src")).unwrap();
    fs::write(
        b.join("Cargo.toml"),
        "[package]\nname='indep'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(b.join("src/main.rs"), "fn main() {}\n").unwrap();
    for dir in [&a, &b] {
        git(dir, &["init", "-q"]);
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", "init"]);
    }
    fs::create_dir_all(root.join("q")).unwrap();
    fs::write(
        root.join("q/config.toml"),
        format!(
            "slots = {slots}\nrun_slots = 4\npoll_ms = 20\n[cache]\nmin_free='0'\nmax_members = {slots}\n"
        ),
    )
    .unwrap();
    let release = root.join("release");
    let mut kids = Children(Vec::new());
    // One native application per allowed member keeps the whole pool leased.
    for i in 0..slots {
        let marker = root.join(format!("ready{i}"));
        let app = w
            .command(&a, &["run", "--offline", "--"])
            .args([&marker, &release])
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        kids.0.push(app);
        wait_until("native app", || marker.exists());
    }
    // As many builds as slots now wait for a member.
    let mut logs = Vec::new();
    for i in 0..slots {
        let log = root.join(format!("wait{i}.log"));
        let build = w
            .command(&a, &["build", "--offline"])
            .stderr(fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        kids.0.push(build);
        logs.push(log);
    }
    for log in &logs {
        wait_until("member wait", || {
            fs::read_to_string(log).is_ok_and(|s| s.contains("waiting for one"))
        });
    }
    let mut other = w
        .command(&b, &["build", "--offline"])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("independent repository build", || {
        other.try_wait().unwrap().is_some()
    });
    assert!(other.wait().unwrap().success());
    for waiter in &mut kids.0[slots..] {
        assert!(
            waiter.try_wait().unwrap().is_none(),
            "waiter finished early"
        );
    }
    fs::write(&release, "").unwrap();
    for kid in &mut kids.0 {
        assert!(kid.wait().unwrap().success());
    }
}

#[cfg(unix)]
#[test]
fn member_wait_does_not_hold_the_only_build_slot() {
    member_wait_frees_slots(1);
}

#[cfg(unix)]
#[test]
fn member_waits_do_not_hold_any_build_slot() {
    member_wait_frees_slots(2);
}

/// A source edit made while a leased build waits for a slot, with an old
/// mtime, still reaches the member's manifest before Cargo builds.
#[cfg(unix)]
#[test]
fn edits_during_a_slot_wait_reach_the_member_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let root = buildq::util::canonical(tmp.path());
    let w = tiny_world(
        &root,
        &[
            (
                "Cargo.toml",
                "[package]\nname='waiter'\nversion='0.1.0'\nedition='2021'\n",
            ),
            ("src/lib.rs", "pub fn v() -> u32 { 1 }\n"),
        ],
    );
    let ws = root.join("ws");
    git(&ws, &["init", "-q"]);
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-q", "-m", "init"]);
    fs::create_dir_all(root.join("q")).unwrap();
    fs::write(
        root.join("q/config.toml"),
        "slots = 1\n[cache]\nmin_free='0'\nmax_members=1\n",
    )
    .unwrap();
    w.cargo(&ws, &["build", "--offline"]);

    let (held, release) = (root.join("held"), root.join("release"));
    let mut children = Children(Vec::new());
    children.0.push(
        Command::new(BQ)
            .args(["run", "--", "sh", "-c"])
            .arg(format!(
                "touch '{}'; while [ ! -f '{}' ]; do sleep 0.05; done",
                held.display(),
                release.display()
            ))
            .env("BUILDQ_DIR", root.join("q"))
            .env("BUILDQ_CONFIG", root.join("q/config.toml"))
            .env_remove("BUILDQ_HELD")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_until("the slot holder", || held.exists());
    children.0.push(
        w.command(&ws, &["build", "--offline"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let waiters = || {
        let out = Command::new(BQ)
            .args(["status", "--json"])
            .env("BUILDQ_DIR", root.join("q"))
            .env("BUILDQ_CONFIG", root.join("q/config.toml"))
            .output()
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        v["classes"][0]["waiters"].as_array().map_or(0, Vec::len)
    };
    wait_until("the build to wait for a slot", || waiters() > 0);
    let lib = ws.join("src/lib.rs");
    fs::write(&lib, "pub fn v() -> u32 { 2 }\n").unwrap();
    set_old_mtime(&lib);
    fs::write(&release, "").unwrap();
    let build = children.0.pop().unwrap();
    assert!(build.wait_with_output().unwrap().status.success());

    let blob = Command::new("git")
        .args(["hash-object", "src/lib.rs"])
        .current_dir(&ws)
        .output()
        .unwrap();
    let blob = String::from_utf8_lossy(&blob.stdout).trim().to_string();
    let pools = fs::read_dir(root.join("cache/pools")).unwrap();
    let pool = pools.flatten().find(|e| e.path().is_dir()).unwrap().path();
    let manifest = fs::read_to_string(pool.join("m0.manifest")).unwrap();
    assert!(
        manifest.lines().any(|l| l == format!("{blob} src/lib.rs")),
        "{manifest}"
    );
}
