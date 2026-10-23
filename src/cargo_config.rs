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
}

fn merge(entries: &mut BTreeMap<String, Entry>, table: &toml::Table, root: &Path) {
    let Some(vars) = table.get("env").and_then(toml::Value::as_table) else {
        return;
    };
    for (name, value) in vars {
        let entry = entries.entry(name.clone()).or_default();
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

fn load_file(
    entries: &mut BTreeMap<String, Entry>,
    path: &Path,
    stack: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let path = crate::util::canonical(path);
    if stack.contains(&path) {
        return Err("recursive Cargo config include".into());
    }
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    // Do not include the parser's diagnostic: config values may contain secrets.
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
                    load_file(entries, &file, stack)?;
                }
            }
        }
    }
    let root = path
        .parent()
        .and_then(Path::parent)
        .unwrap_or(Path::new("/"));
    merge(entries, &table, root);
    stack.pop();
    Ok(())
}

fn load_env(cwd: &Path, parsed: &Parsed) -> Result<BTreeMap<String, Entry>, String> {
    let mut entries = BTreeMap::new();
    for file in config_files(cwd).iter().rev() {
        load_file(&mut entries, file, &mut Vec::new())?;
    }
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
                load_file(&mut entries, &file, &mut Vec::new())?;
            } else {
                let table = config
                    .parse::<toml::Table>()
                    .map_err(|_| "invalid Cargo --config".to_string())?;
                merge(&mut entries, &table, cwd);
            }
        }
    }
    Ok(entries)
}

fn values(
    entries: BTreeMap<String, Entry>,
    inherited: impl Fn(&str) -> bool,
) -> BTreeMap<String, OsString> {
    entries
        .into_iter()
        .filter_map(|(name, entry)| {
            if !entry.force.unwrap_or(false) && inherited(&name) {
                return None;
            }
            let (value, root) = entry.value?;
            let value = if entry.relative.unwrap_or(false) {
                root.join(value).into_os_string()
            } else {
                value.into()
            };
            Some((name, value))
        })
        .collect()
}

pub(crate) fn apply_env(cmd: &mut Command, cwd: &Path, parsed: &Parsed) -> Result<(), String> {
    cmd.envs(values(load_env(cwd, parsed)?, |name| {
        env::var_os(name).is_some()
    }));
    Ok(())
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
        let vars = values(entries, |name| name == "B");
        assert_eq!(vars["A"], Path::new("/parent/data").as_os_str());
        assert!(!vars.contains_key("B"));
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
