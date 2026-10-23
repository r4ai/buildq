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
    fn cargo(&self, dir: &Path, args: &[&str]) -> (Output, Duration) {
        let config = self.root.join("q/config.toml");
        if !config.exists() {
            fs::create_dir_all(config.parent().unwrap()).unwrap();
            fs::write(&config, "[cache]\nmin_free='0'\n").unwrap();
        }
        let t = Instant::now();
        let out = Command::new(&self.shim_cargo)
            .args(args)
            .current_dir(dir)
            .env("PATH", &self.path)
            .env("BUILDQ_DIR", self.root.join("q"))
            .env("BUILDQ_CONFIG", &config)
            .env("BUILDQ_CACHE", self.root.join("cache"))
            .env("BUILDQ_GC", "0")
            .env_remove("BUILDQ_HELD")
            .env_remove("CARGO_TARGET_DIR")
            // CI forces colour; `compiled` matches plain "Compiling" lines.
            .env("CARGO_TERM_COLOR", "never")
            .output()
            .unwrap();
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

#[test]
fn rebuild_graph_reads_real_cargo_workspace_metadata() {
    let out = Command::new("cargo")
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--offline",
            "--manifest-path",
        ])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample-ws/Cargo.toml"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let graph =
        buildq::pool::RebuildGraph::from_metadata(&serde_json::from_slice(&out.stdout).unwrap())
            .unwrap();
    let set = |name: &str| std::collections::BTreeSet::from([name.to_string()]);
    assert_eq!(graph.cost(&set("sample-core")), 3);
    assert_eq!(graph.cost(&set("sample-util")), 2);
    assert_eq!(graph.cost(&set("sample-app")), 1);
}
