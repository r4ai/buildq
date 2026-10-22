//! Understanding a cargo command line: global options, `+toolchain`, aliases
//! from `.cargo/config.toml`, and whether the command is resource-heavy.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Global options that take a value (`--config x`, `-Z x`, ...).
const VALUE_OPTS: &[&str] = &["--config", "-Z", "--color", "-C"];
/// Options that make cargo do something other than run a subcommand.
const INFO_OPTS: &[&str] = &["-V", "--version", "--list", "-h", "--help", "--explain"];

const BUILTIN_ALIASES: &[(&str, &str)] = &[
    ("b", "build"),
    ("c", "check"),
    ("d", "doc"),
    ("t", "test"),
    ("r", "run"),
    ("rm", "remove"),
];

/// Subcommands built into cargo: user aliases cannot shadow them.
const BUILTINS: &[&str] = &[
    "add",
    "bench",
    "build",
    "check",
    "clean",
    "clippy",
    "config",
    "doc",
    "fetch",
    "fix",
    "fmt",
    "generate-lockfile",
    "help",
    "info",
    "init",
    "install",
    "locate-project",
    "login",
    "logout",
    "metadata",
    "miri",
    "new",
    "owner",
    "package",
    "pkgid",
    "publish",
    "remove",
    "report",
    "run",
    "rustc",
    "rustdoc",
    "search",
    "test",
    "tree",
    "uninstall",
    "update",
    "vendor",
    "verify-project",
    "version",
    "yank",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    /// `+nightly` (without the plus).
    pub toolchain: Option<String>,
    /// Arguments without the `+toolchain`, aliases expanded.
    pub args: Vec<String>,
    /// Index of the subcommand in `args`.
    pub sub_index: Option<usize>,
    /// The original alias name when one was expanded.
    pub alias: Option<String>,
}

impl Parsed {
    pub fn sub(&self) -> Option<&str> {
        self.sub_index.map(|i| self.args[i].as_str())
    }

    /// Arguments after the subcommand, up to `--`.
    pub fn sub_args(&self) -> &[String] {
        let start = self.sub_index.map(|i| i + 1).unwrap_or(self.args.len());
        let rest = &self.args[start..];
        let end = rest.iter().position(|a| a == "--").unwrap_or(rest.len());
        &rest[..end]
    }

    /// Arguments after `--` (program/test arguments).
    pub fn trailing(&self) -> &[String] {
        let start = self.sub_index.map(|i| i + 1).unwrap_or(self.args.len());
        let rest = &self.args[start..];
        match rest.iter().position(|a| a == "--") {
            Some(i) => &rest[i + 1..],
            None => &[],
        }
    }

    pub fn wants_help(&self) -> bool {
        self.sub_args().iter().any(|a| a == "-h" || a == "--help")
    }

    /// Value of an option after the subcommand (`--bin x` / `--bin=x`).
    pub fn opt(&self, names: &[&str]) -> Option<String> {
        opt_value(self.sub_args(), names)
    }

    pub fn has_flag(&self, name: &str) -> bool {
        self.sub_args().iter().any(|a| a == name)
    }
}

pub fn opt_value(args: &[String], names: &[&str]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        for n in names {
            if a == n {
                return it.next().cloned();
            }
            if let Some(v) = a.strip_prefix(&format!("{n}=")) {
                return Some(v.to_string());
            }
            // -pfoo
            if n.len() == 2 && !n.starts_with("--") && a.len() > 2 && a.starts_with(n) {
                return Some(a[2..].to_string());
            }
        }
    }
    None
}

/// Index of the first non-option argument, skipping global options.
fn find_sub(args: &[String]) -> Option<usize> {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            return None;
        }
        if INFO_OPTS.contains(&a.as_str()) {
            return None;
        }
        if VALUE_OPTS.contains(&a.as_str()) {
            i += 2;
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        return Some(i);
    }
    None
}

/// Splits off `+toolchain` and expands aliases (recursively, like cargo).
pub fn parse(args: &[String], aliases: &BTreeMap<String, Vec<String>>) -> Parsed {
    let mut args: Vec<String> = args.to_vec();
    let toolchain = match args.first() {
        Some(a) if a.starts_with('+') => Some(args.remove(0)[1..].to_string()),
        _ => None,
    };
    let mut alias = None;
    let mut sub_index = find_sub(&args);
    for _ in 0..16 {
        let Some(i) = sub_index else { break };
        let name = args[i].clone();
        if BUILTINS.contains(&name.as_str()) {
            break;
        }
        let expansion = aliases.get(&name).cloned().or_else(|| {
            BUILTIN_ALIASES
                .iter()
                .find(|(a, _)| *a == name)
                .map(|(_, b)| vec![b.to_string()])
        });
        let Some(exp) = expansion else { break };
        alias.get_or_insert(name);
        args.splice(i..=i, exp);
        // The expansion may start with global options (`--config ... run`).
        sub_index = find_sub(&args);
    }
    Parsed {
        toolchain,
        args,
        sub_index,
        alias,
    }
}

/// `[alias]` tables from every config cargo would read for `cwd`, lowest
/// precedence first: $CARGO_HOME/config.toml, then ancestors down to `cwd`.
/// `CARGO_ALIAS_<NAME>` environment variables win.
pub fn load_aliases(cwd: &Path) -> BTreeMap<String, Vec<String>> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        for name in ["config.toml", "config"] {
            let p = d.join(".cargo").join(name);
            if p.is_file() {
                files.push(p);
                break;
            }
        }
        dir = d.parent();
    }
    if let Some(home) = cargo_home() {
        for name in ["config.toml", "config"] {
            let p = home.join(name);
            if p.is_file() {
                if !files.iter().any(|f| crate::util::same_path(f, &p)) {
                    files.push(p);
                }
                break;
            }
        }
    }
    let mut out = BTreeMap::new();
    // `files` is deepest first; apply in reverse so deeper wins.
    for f in files.iter().rev() {
        let Ok(text) = fs::read_to_string(f) else {
            continue;
        };
        let Ok(table) = text.parse::<toml::Table>() else {
            continue;
        };
        if let Some(toml::Value::Table(aliases)) = table.get("alias") {
            for (k, v) in aliases {
                if let Some(words) = alias_words(v) {
                    out.insert(k.clone(), words);
                }
            }
        }
    }
    for (k, v) in env::vars() {
        if let Some(name) = k.strip_prefix("CARGO_ALIAS_") {
            let name = name.to_lowercase().replace('_', "-");
            out.insert(name, v.split_whitespace().map(str::to_string).collect());
        }
    }
    out
}

fn alias_words(v: &toml::Value) -> Option<Vec<String>> {
    match v {
        toml::Value::String(s) => Some(s.split_whitespace().map(str::to_string).collect()),
        toml::Value::Array(a) => a.iter().map(|x| x.as_str().map(str::to_string)).collect(),
        _ => None,
    }
}

pub fn cargo_home() -> Option<PathBuf> {
    if let Some(h) = env::var_os("CARGO_HOME") {
        return Some(PathBuf::from(h));
    }
    let home = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME"))?;
    Some(PathBuf::from(home).join(".cargo"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Straight to the real cargo.
    Passthrough,
    /// Through the queue (and the pool when possible).
    Queue,
}

pub fn route(parsed: &Parsed, is_heavy: &dyn Fn(&str) -> bool) -> Route {
    match parsed.sub() {
        Some(sub) if is_heavy(sub) && !parsed.wants_help() => Route::Queue,
        _ => Route::Passthrough,
    }
}

/// How the command picks its target dir, if it does so explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetDirChoice {
    /// Nothing explicit: buildq may point it at a pool member.
    Default,
    /// `--target-dir`, or `CARGO_TARGET_DIR` already set: respected, not pooled.
    Explicit,
    /// `--config build.target-dir=X` at args index `usize`: re-rooted inside the member.
    Config(usize, String),
}

pub fn target_dir_choice(parsed: &Parsed) -> TargetDirChoice {
    if env::var_os("CARGO_TARGET_DIR").is_some() || env::var_os("CARGO_BUILD_TARGET_DIR").is_some()
    {
        return TargetDirChoice::Explicit;
    }
    let end = parsed
        .args
        .iter()
        .position(|a| a == "--")
        .unwrap_or(parsed.args.len());
    let args = &parsed.args[..end];
    for (i, a) in args.iter().enumerate() {
        if a == "--target-dir" || a.starts_with("--target-dir=") {
            return TargetDirChoice::Explicit;
        }
        let value = if a == "--config" {
            args.get(i + 1).map(|v| (i + 1, v.clone()))
        } else {
            a.strip_prefix("--config=").map(|v| (i, v.to_string()))
        };
        if let Some((idx, v)) = value
            && let Some(rest) = v.trim().strip_prefix("build.target-dir")
            && let Some(val) = rest.trim().strip_prefix('=')
        {
            let val = val.trim().trim_matches('"').trim_matches('\'').to_string();
            return TargetDirChoice::Config(idx, val);
        }
    }
    TargetDirChoice::Default
}

/// `target/native` -> `<member>/native`; absolute paths stay.
pub fn reroot_target_dir(value: &str, member: &Path) -> Option<PathBuf> {
    let p = Path::new(value);
    if p.is_absolute() {
        return None;
    }
    let norm = value.replace('\\', "/");
    let rel = norm.trim_start_matches("./");
    let rel = rel.strip_prefix("target/").unwrap_or(rel);
    let rel = if rel == "target" { "" } else { rel };
    Some(if rel.is_empty() {
        member.to_path_buf()
    } else {
        member.join("sub").join(rel)
    })
}

/// Finds the real cargo: `BUILDQ_REAL_CARGO`/config, else the first `cargo`
/// on PATH that is not a buildq shim (dirs holding a `.buildq-shim` marker
/// or the shim's own dir are skipped).
pub fn find_real_cargo(configured: Option<&Path>, skip_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = configured {
        return Some(p.to_path_buf());
    }
    let me = env::current_exe().ok().map(|p| crate::util::canonical(&p));
    let path = env::var_os("PATH")?;
    let names: &[&str] = if cfg!(windows) {
        &["cargo.exe", "cargo.cmd", "cargo.bat"]
    } else {
        &["cargo"]
    };
    for dir in env::split_paths(&path) {
        if dir.as_os_str().is_empty() || dir.join(crate::shim::MARKER).exists() {
            continue;
        }
        if let Some(skip) = skip_dir
            && crate::util::same_path(&dir, skip)
        {
            continue;
        }
        for n in names {
            let cand = dir.join(n);
            if cand.is_file() {
                if me
                    .as_ref()
                    .is_some_and(|m| crate::util::same_path(m, &cand))
                {
                    continue;
                }
                return Some(cand);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    fn aliases() -> BTreeMap<String, Vec<String>> {
        let mut m = BTreeMap::new();
        m.insert("dev".into(), v("run -p app --features dev"));
        m.insert("lint".into(), v("clippy --all-targets"));
        m.insert("chain".into(), v("dev --release"));
        m.insert(
            "trainer".into(),
            v("--config build.target-dir=\"target/native\" run --release -p tr --"),
        );
        m.insert("build".into(), v("fmt")); // cannot shadow a builtin
        m
    }

    #[test]
    fn routes_heavy_and_passthrough() {
        let heavy = |s: &str| crate::config::DEFAULT_HEAVY.contains(&s);
        let a = aliases();
        for (cmd, expect, sub) in [
            ("build", Route::Queue, Some("build")),
            ("-v test -p x", Route::Queue, Some("test")),
            ("--config a=1 check", Route::Queue, Some("check")),
            ("t", Route::Queue, Some("test")),
            ("dev", Route::Queue, Some("run")),
            ("chain", Route::Queue, Some("run")),
            ("lint", Route::Queue, Some("clippy")),
            ("trainer train", Route::Queue, Some("run")),
            ("fmt", Route::Passthrough, Some("fmt")),
            (
                "metadata --format-version 1",
                Route::Passthrough,
                Some("metadata"),
            ),
            ("--version", Route::Passthrough, None),
            ("-V", Route::Passthrough, None),
            ("tree", Route::Passthrough, Some("tree")),
            ("build --help", Route::Passthrough, Some("build")),
            ("+nightly build", Route::Queue, Some("build")),
        ] {
            let p = parse(&v(cmd), &a);
            assert_eq!(route(&p, &heavy), expect, "{cmd}");
            assert_eq!(p.sub(), sub, "{cmd}");
        }
    }

    #[test]
    fn alias_expansion_keeps_args() {
        let p = parse(&v("+stable dev -- --windowed"), &aliases());
        assert_eq!(p.toolchain.as_deref(), Some("stable"));
        assert_eq!(p.alias.as_deref(), Some("dev"));
        assert_eq!(p.args, v("run -p app --features dev -- --windowed"));
        assert_eq!(p.opt(&["-p", "--package"]).as_deref(), Some("app"));
        assert_eq!(p.trailing(), v("--windowed"));
        let p = parse(&v("chain"), &aliases());
        assert_eq!(p.args, v("run -p app --features dev --release"));
    }

    #[test]
    fn target_dir_config_is_rerooted() {
        let p = parse(&v("trainer train"), &aliases());
        match target_dir_choice(&p) {
            TargetDirChoice::Config(i, val) => {
                assert_eq!(p.args[i], "build.target-dir=\"target/native\"");
                assert_eq!(val, "target/native");
                let r = reroot_target_dir(&val, Path::new("/pool/m0")).unwrap();
                assert_eq!(r, Path::new("/pool/m0").join("sub").join("native"));
            }
            other => panic!("{other:?}"),
        }
        let p = parse(&v("build --target-dir x"), &aliases());
        assert_eq!(target_dir_choice(&p), TargetDirChoice::Explicit);
    }

    #[test]
    fn aliases_from_config_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".cargo")).unwrap();
        fs::create_dir_all(root.join("sub/.cargo")).unwrap();
        fs::write(
            root.join(".cargo/config.toml"),
            "[alias]\ndev = \"run -p a\"\nx = [\"check\", \"--all\"]\n",
        )
        .unwrap();
        fs::write(
            root.join("sub/.cargo/config.toml"),
            "[alias]\ndev = \"run -p b\"\n",
        )
        .unwrap();
        let a = load_aliases(&root.join("sub"));
        assert_eq!(a["dev"], v("run -p b"));
        assert_eq!(a["x"], v("check --all"));
    }
}
