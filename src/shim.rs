//! `buildq shim install cargo --into <dir>`.
//!
//! On Windows the shim is a copy of buildq itself named `cargo.exe`: a real
//! executable resolves the same way from PowerShell, cmd and Git Bash (a
//! `.cmd` or sh script would not). buildq sees its own file stem `cargo` and
//! acts as the shim. On Unix the shim is a two-line sh script.
//!
//! The directory gets a `.buildq-shim` marker; the real-cargo lookup skips
//! every PATH entry carrying it, so a shim never calls itself.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const MARKER: &str = ".buildq-shim";

pub fn install(tool: &str, into: &Path, sh: bool) -> io::Result<Vec<PathBuf>> {
    if tool != "cargo" {
        return Err(io::Error::other(format!(
            "only `cargo` can be shimmed, not `{tool}`"
        )));
    }
    fs::create_dir_all(into)?;
    let me = crate::util::canonical(&std::env::current_exe()?);
    let mut written = Vec::new();
    fs::write(
        into.join(MARKER),
        format!("buildq shim directory\nbuildq = {}\n", me.display()),
    )?;
    written.push(into.join(MARKER));
    if cfg!(windows) {
        let dest = into.join("cargo.exe");
        if crate::util::same_path(&me, &dest) {
            return Err(io::Error::other("refusing to copy the shim onto itself"));
        }
        // A running shim cannot be overwritten, but it can be renamed away.
        if dest.exists() && fs::remove_file(&dest).is_err() {
            let old = into.join(format!("cargo.old-{}.exe", crate::util::now_ms()));
            fs::rename(&dest, old)?;
        }
        fs::copy(&me, &dest)?;
        written.push(dest);
    }
    if sh || !cfg!(windows) {
        let dest = into.join("cargo");
        let script = format!(
            "#!/bin/sh\n# buildq shim: routes heavy cargo commands through the machine-wide queue.\nBUILDQ_SHIM_DIR=\"$(dirname \"$0\")\" exec \"{}\" cargo \"$@\"\n",
            me.display().to_string().replace('\\', "/")
        );
        fs::write(&dest, script)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dest, fs::Permissions::from_mode(0o755))?;
        }
        written.push(dest);
    }
    Ok(written)
}

/// True when this process was started as the shim (file stem `cargo`).
pub fn invoked_as_cargo() -> bool {
    if std::env::var_os("BUILDQ_AS_CLI").is_some() {
        return false;
    }
    std::env::args_os()
        .next()
        .map(|a| crate::util::file_stem_lower(&a.to_string_lossy()) == "cargo")
        .unwrap_or(false)
}

/// The shim's own dir, skipped when looking for the real cargo.
pub fn shim_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("BUILDQ_SHIM_DIR") {
        return Some(PathBuf::from(d));
    }
    if invoked_as_cargo() {
        return std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf));
    }
    None
}
