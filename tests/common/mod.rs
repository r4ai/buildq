//! Isolated process environment for integration fixtures.
use std::ffi::OsStr;
use std::process::Command;

pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("BUILDQ_") || name == "CARGO_TARGET_DIR" {
            command.env_remove(name);
        }
    }
    command
}
