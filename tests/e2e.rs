//! End-to-end: the cargo shim with the real cargo on the sample workspace in
//! `tests/fixtures/sample-ws`, two git worktrees sharing one pool member.
//!
//! Needs the `regex` crate family in the local cargo registry cache (it runs
//! `--offline`) and compiles it once, so it is ignored by default:
//!
//! ```text
//! cargo test --test e2e -- --ignored --nocapture
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime};

const BQ: &str = env!("CARGO_BIN_EXE_buildq");

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
        let t = Instant::now();
        let out = Command::new(&self.shim_cargo)
            .args(args)
            .current_dir(dir)
            .env("PATH", &self.path)
            .env("BUILDQ_DIR", self.root.join("q"))
            .env("BUILDQ_CACHE", self.root.join("cache"))
            .env("BUILDQ_GC", "0")
            .env_remove("BUILDQ_HELD")
            .env_remove("CARGO_TARGET_DIR")
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
            .env("CARGO_BUILD_JOBS", "8")
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

    // 2. Worktree 2, same commit: everything is reused, workspace crates included.
    let (o, d2) = w.cargo(&wt2, &build);
    rows.push(("wt2 first build, shared member".into(), d2, compiled(&o)));
    assert_eq!(compiled(&o), 0, "{}", String::from_utf8_lossy(&o.stderr));

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
