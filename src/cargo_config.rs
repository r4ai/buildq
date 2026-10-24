//! Cargo's configuration environment for the separately launched run phase.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cargo::{Parsed, config_files};

#[derive(Default)]
struct Entry {
    value: Option<(String, PathBuf)>,
    force: Option<bool>,
    relative: Option<bool>,
    /// Root of the last config that set any field of this variable.
    last_root: PathBuf,
}

fn merge(entries: &mut BTreeMap<String, Entry>, table: &toml::Table, root: &Path) {
    let Some(vars) = table.get("env").and_then(toml::Value::as_table) else {
        return;
    };
    for (name, value) in vars {
        let entry = entries.entry(name.clone()).or_default();
        entry.last_root = root.into();
        if let Some(value) = value.as_str() {
            entry.value = Some((value.into(), root.into()));
        } else if let Some(table) = value.as_table() {
            if let Some(value) = table.get("value").and_then(toml::Value::as_str) {
                entry.value = Some((value.into(), root.into()));
            }
            if let Some(force) = table.get("force").and_then(toml::Value::as_bool) {
                entry.force = Some(force);
            }
            if let Some(relative) = table.get("relative").and_then(toml::Value::as_bool) {
                entry.relative = Some(relative);
            }
        }
    }
}

fn visit_file(
    path: &Path,
    stack: &mut Vec<PathBuf>,
    visit: &mut dyn FnMut(&toml::Table, &Path),
) -> Result<(), String> {
    let path = crate::util::canonical(path);
    if stack.contains(&path) {
        return Err("recursive Cargo config include".into());
    }
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    // Config values can contain secrets; avoid parser diagnostics.
    let table = text
        .parse::<toml::Table>()
        .map_err(|_| "invalid Cargo config".to_string())?;
    stack.push(path.clone());
    if let Some(includes) = table.get("include").and_then(toml::Value::as_array) {
        for include in includes {
            let (name, optional) = if let Some(name) = include.as_str() {
                (Some(name), false)
            } else {
                (
                    include.get("path").and_then(toml::Value::as_str),
                    include
                        .get("optional")
                        .and_then(toml::Value::as_bool)
                        .unwrap_or(false),
                )
            };
            if let Some(name) = name {
                let file = path.parent().unwrap().join(name);
                if !optional || file.exists() {
                    visit_file(&file, stack, visit)?;
                }
            }
        }
    }
    let root = path
        .parent()
        .and_then(Path::parent)
        .unwrap_or(Path::new("/"));
    visit(&table, root);
    stack.pop();
    Ok(())
}

fn visit_cli(
    cwd: &Path,
    parsed: &Parsed,
    visit: &mut dyn FnMut(&toml::Table, &Path),
) -> Result<(), String> {
    let mut args = parsed.args.iter().take_while(|a| a.as_str() != "--");
    while let Some(arg) = args.next() {
        let config = if arg == "--config" {
            args.next().map(String::as_str)
        } else {
            arg.strip_prefix("--config=")
        };
        if let Some(config) = config {
            let file = cwd.join(config);
            if file.is_file() {
                visit_file(&file, &mut Vec::new(), visit)?;
            } else {
                let table = config
                    .parse::<toml::Table>()
                    .map_err(|_| "invalid Cargo --config".to_string())?;
                visit(&table, cwd);
            }
        }
    }
    Ok(())
}

fn load_env(cwd: &Path, parsed: &Parsed) -> Result<BTreeMap<String, Entry>, String> {
    let mut entries = BTreeMap::new();
    let mut visit = |table: &toml::Table, root: &Path| merge(&mut entries, table, root);
    for file in config_files(cwd).iter().rev() {
        visit_file(file, &mut Vec::new(), &mut visit)?;
    }
    visit_cli(cwd, parsed, &mut visit)?;
    Ok(entries)
}

/// Which config a `relative = true` value is resolved against when the
/// fields of one variable come from several configs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelativeTo {
    /// Cargo 1.97+ (rust-lang/cargo#16957): the config that set `value`.
    ValueOrigin,
    /// Earlier Cargo: the highest-precedence config defining the variable.
    LastDefinition,
}

/// Picks the rule from `cargo -V` output, e.g.
/// `cargo 1.97.0-nightly (0123abcd 2026-05-06)`. Nightlies of 1.97 got the
/// change on 2026-05-05; unknown versions get the current rule.
pub(crate) fn relative_rule(cargo_v: &str) -> RelativeTo {
    let mut words = cargo_v.split_whitespace();
    let Some(release) = words.nth(1) else {
        return RelativeTo::ValueOrigin;
    };
    let date = words.last().map(|d| d.trim_end_matches(')'));
    let (version, pre) = release.split_once('-').unwrap_or((release, ""));
    let mut parts = version.split('.').map(|p| p.parse::<u32>().ok());
    let (Some(Some(major)), Some(Some(minor))) = (parts.next(), parts.next()) else {
        return RelativeTo::ValueOrigin;
    };
    let old = (major, minor) < (1, 97)
        || ((major, minor) == (1, 97) && !pre.is_empty() && date.is_some_and(|d| d < "2026-05-05"));
    if old {
        RelativeTo::LastDefinition
    } else {
        RelativeTo::ValueOrigin
    }
}

/// The rule of the Cargo that `real` (with an optional `+toolchain`) runs.
pub(crate) fn cargo_relative_rule(real: &Path, toolchain: Option<&str>) -> RelativeTo {
    let mut c = Command::new(real);
    if let Some(t) = toolchain {
        c.arg(format!("+{t}"));
    }
    let out = c
        .arg("-V")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => relative_rule(&String::from_utf8_lossy(&o.stdout)),
        _ => RelativeTo::ValueOrigin,
    }
}

fn values(
    entries: BTreeMap<String, Entry>,
    inherited: impl Fn(&str) -> bool,
    rule: RelativeTo,
) -> BTreeMap<String, OsString> {
    entries
        .into_iter()
        .filter_map(|(name, entry)| {
            if !entry.force.unwrap_or(false) && inherited(&name) {
                return None;
            }
            let (value, value_root) = entry.value?;
            let root = match rule {
                RelativeTo::ValueOrigin => value_root,
                RelativeTo::LastDefinition => entry.last_root,
            };
            let value = if entry.relative.unwrap_or(false) {
                root.join(value).into_os_string()
            } else {
                value.into()
            };
            Some((name, value))
        })
        .collect()
}

pub(crate) fn apply_env(
    cmd: &mut Command,
    cwd: &Path,
    parsed: &Parsed,
    rule: RelativeTo,
) -> Result<(), String> {
    cmd.envs(values(
        load_env(cwd, parsed)?,
        |name| env::var_os(name).is_some(),
        rule,
    ));
    Ok(())
}

/// Configured target directory, with Cargo's explicit/environment/config precedence.
/// A missing setting leaves workspace-default selection to Cargo.
pub(crate) fn target_directory(cwd: &Path, parsed: &Parsed) -> Result<Option<PathBuf>, String> {
    let mut target = None;
    let mut collect = |table: &toml::Table, root: &Path| {
        if let Some(value) = table
            .get("build")
            .and_then(|v| v.get("target-dir"))
            .and_then(toml::Value::as_str)
        {
            target = Some(crate::util::resolve(root, Path::new(value)));
        }
    };
    for file in config_files(cwd).iter().rev() {
        visit_file(file, &mut Vec::new(), &mut collect)?;
    }
    if let Some(value) = env::var_os("CARGO_BUILD_TARGET_DIR").filter(|v| !v.is_empty()) {
        target = Some(crate::util::resolve(cwd, Path::new(&value)));
    }
    visit_cli(cwd, parsed, &mut |table, root| {
        if let Some(value) = table
            .get("build")
            .and_then(|v| v.get("target-dir"))
            .and_then(toml::Value::as_str)
        {
            target = Some(crate::util::resolve(root, Path::new(value)));
        }
    })?;
    if let Some(value) = env::var_os("CARGO_TARGET_DIR").filter(|v| !v.is_empty()) {
        target = Some(crate::util::resolve(cwd, Path::new(&value)));
    }
    if let Some(value) = parsed.opt(&["--target-dir"]) {
        target = Some(crate::util::resolve(cwd, Path::new(&value)));
    }
    Ok(target)
}

#[derive(Default)]
struct Runners {
    target: Option<toml::Value>,
    entries: BTreeMap<String, (toml::Value, PathBuf)>,
}

impl Runners {
    fn merge(&mut self, table: &toml::Table, root: &Path) {
        if let Some(target) = table.get("build").and_then(|v| v.get("target")) {
            self.target = Some(target.clone());
        }
        if let Some(targets) = table.get("target").and_then(toml::Value::as_table) {
            for (key, target) in targets {
                if let Some(runner) = target.get("runner") {
                    if let Some((toml::Value::Array(old), _)) = self.entries.get_mut(key)
                        && let toml::Value::Array(new) = runner
                    {
                        old.extend(new.iter().cloned());
                    } else {
                        self.entries
                            .insert(key.clone(), (runner.clone(), root.into()));
                    }
                }
            }
        }
    }
}

/// Resolve Cargo's runner for the copied executable, keeping its argument list.
pub(crate) fn runner(
    real: &Path,
    cwd: &Path,
    parsed: &Parsed,
    rule: RelativeTo,
) -> Result<Option<Vec<OsString>>, String> {
    let mut runners = Runners::default();
    for file in config_files(cwd).iter().rev() {
        visit_file(file, &mut Vec::new(), &mut |table, root| {
            runners.merge(table, root)
        })?;
    }
    let configured_env = values(
        load_env(cwd, parsed)?,
        |name| env::var_os(name).is_some(),
        rule,
    );
    let get_env = |name: &str| {
        configured_env
            .get(name)
            .cloned()
            .or_else(|| env::var_os(name))
    };
    if let Some(target) = get_env("CARGO_BUILD_TARGET") {
        runners.target = Some(toml::Value::String(target.to_string_lossy().into_owned()));
    }
    // CLI overrides environment and file values, except that an exact triple
    // always has precedence over a matching cfg expression.
    let mut cli = Vec::new();
    visit_cli(cwd, parsed, &mut |table, root| {
        if let Some(target) = table.get("build").and_then(|v| v.get("target")) {
            runners.target = Some(target.clone());
        }
        cli.push((table.clone(), root.to_path_buf()));
    })?;
    if runners
        .target
        .as_ref()
        .and_then(toml::Value::as_array)
        .is_some_and(|a| a.len() != 1)
        || parsed
            .sub_args()
            .iter()
            .filter(|a| *a == "--target" || a.starts_with("--target="))
            .count()
            > 1
    {
        return Err("cargo run requires exactly one build target".into());
    }
    let target = parsed
        .opt(&["--target"])
        .or_else(|| {
            runners.target.as_ref().and_then(|v| {
                v.as_str().map(str::to_owned).or_else(|| {
                    v.as_array()
                        .filter(|a| a.len() == 1)?
                        .first()?
                        .as_str()
                        .map(str::to_owned)
                })
            })
        })
        .or_else(|| crate::job::probe_toolchain(real, parsed.toolchain.as_deref()).map(|t| t.host))
        .ok_or("cannot determine Cargo runner target")?;
    let key = if target.ends_with(".json") {
        Path::new(&target)
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    } else {
        target.clone()
    };
    let env_key = format!(
        "CARGO_TARGET_{}_RUNNER",
        key.to_uppercase().replace('-', "_")
    );
    if let Some(value) = get_env(&env_key) {
        runners.entries.insert(
            key.clone(),
            (
                toml::Value::String(value.to_string_lossy().into_owned()),
                cwd.into(),
            ),
        );
    }
    for (table, root) in cli {
        runners.merge(&table, &root);
    }
    let selected = if let Some(runner) = runners.entries.get(&key) {
        Some(runner)
    } else {
        let mut matches = Vec::new();
        if runners.entries.keys().any(|k| k.starts_with("cfg(")) {
            let output = crate::job::rustc_cmd(real, parsed.toolchain.as_deref())
                .args(["--print", "cfg", "--target", &target])
                .output()
                .map_err(|_| "cannot inspect target cfg for Cargo runner")?;
            if !output.status.success() {
                return Err("cannot inspect target cfg for Cargo runner".into());
            }
            let cfgs: Vec<_> = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| cfg_expr::Expression::parse(line).ok())
                .collect();
            for (key, runner) in &runners.entries {
                if key.starts_with("cfg(") {
                    let expression = cfg_expr::Expression::parse(key)
                        .map_err(|_| "invalid Cargo runner cfg expression")?;
                    if expression.eval(|p| cfgs.iter().any(|e| e.predicates().any(|v| &v == p))) {
                        matches.push(runner);
                    }
                }
            }
        }
        if matches.len() > 1 {
            return Err(
                "multiple matching Cargo target runners; configure an exact target triple".into(),
            );
        }
        matches.first().copied()
    };
    let Some((value, root)) = selected else {
        return Ok(None);
    };
    let mut args: Vec<OsString> = match value {
        toml::Value::String(s) => s.split_whitespace().map(OsString::from).collect(),
        toml::Value::Array(a) => a
            .iter()
            .map(|v| {
                v.as_str()
                    .map(OsString::from)
                    .ok_or("invalid Cargo runner argument")
            })
            .collect::<Result<_, _>>()?,
        _ => return Err("invalid Cargo runner configuration".into()),
    };
    let Some(program) = args.first_mut() else {
        return Err("empty Cargo runner command".into());
    };
    if Path::new(program).components().count() > 1 {
        *program = root.join(&*program).into_os_string();
    }
    Ok(Some(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_fields_and_retains_value_origin() {
        let mut entries = BTreeMap::new();
        merge(
            &mut entries,
            &"[env]\nA={value='data',relative=true,force=true}\nB='default'"
                .parse()
                .unwrap(),
            Path::new("/parent"),
        );
        merge(
            &mut entries,
            &"[env]\nA={force=false}".parse().unwrap(),
            Path::new("/child"),
        );
        let vars = values(entries, |name| name == "B", RelativeTo::ValueOrigin);
        assert_eq!(vars["A"], Path::new("/parent").join("data").as_os_str());
        assert!(!vars.contains_key("B"));
    }

    #[test]
    fn relative_values_follow_the_running_cargos_rule() {
        let entries = || {
            let mut entries = BTreeMap::new();
            merge(
                &mut entries,
                &"[env]\nA={value='data',relative=true}".parse().unwrap(),
                Path::new("/home"),
            );
            merge(
                &mut entries,
                &"[env]\nA={force=true}".parse().unwrap(),
                Path::new("/ws"),
            );
            entries
        };
        let a = |rule| values(entries(), |_| false, rule)["A"].clone();
        assert_eq!(
            a(RelativeTo::ValueOrigin),
            Path::new("/home/data").as_os_str()
        );
        assert_eq!(
            a(RelativeTo::LastDefinition),
            Path::new("/ws/data").as_os_str()
        );

        let vv = |release: &str, date: &str| {
            relative_rule(&format!("cargo {release} (0123abcd {date})\n"))
        };
        assert_eq!(vv("1.89.0", "2025-07-30"), RelativeTo::LastDefinition);
        assert_eq!(vv("1.96.0", "2026-05-25"), RelativeTo::LastDefinition);
        assert_eq!(
            vv("1.97.0-nightly", "2026-05-01"),
            RelativeTo::LastDefinition
        );
        assert_eq!(vv("1.97.0-nightly", "2026-05-06"), RelativeTo::ValueOrigin);
        assert_eq!(vv("1.97.0", "2026-07-06"), RelativeTo::ValueOrigin);
        assert_eq!(vv("1.99.1", "2026-10-01"), RelativeTo::ValueOrigin);
        assert_eq!(relative_rule(""), RelativeTo::ValueOrigin);
    }

    #[test]
    fn discovery_legacy_precedence_and_cli_overrides() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::util::canonical(temp.path());
        let child = root.join("child");
        fs::create_dir_all(child.join(".cargo")).unwrap();
        fs::create_dir_all(root.join(".cargo")).unwrap();
        fs::write(root.join(".cargo/config.toml"), "[env]\nBQ_TEST='parent'").unwrap();
        fs::write(child.join(".cargo/config.toml"), "[env]\nBQ_TEST='ignored'").unwrap();
        fs::write(child.join(".cargo/config"), "[env]\nBQ_TEST='legacy'").unwrap();
        let parsed = crate::cargo::parse(&["run".into()], &BTreeMap::new());
        assert_eq!(
            load_env(&child, &parsed).unwrap()["BQ_TEST"]
                .value
                .as_ref()
                .unwrap()
                .0,
            "legacy"
        );
        let parsed = crate::cargo::parse(
            &[
                "run".into(),
                "--config=env.BQ_TEST='cli'".into(),
                "--".into(),
                "--config=env.BQ_TEST='program'".into(),
            ],
            &BTreeMap::new(),
        );
        assert_eq!(
            load_env(&child, &parsed).unwrap()["BQ_TEST"]
                .value
                .as_ref()
                .unwrap()
                .0,
            "cli"
        );
    }
}
