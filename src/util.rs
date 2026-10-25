//! Small helpers: stable hashing, time, sizes, paths, directory walking.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// FNV-1a 64-bit: stable across processes, platforms and Rust versions (unlike
/// `DefaultHasher`), which matters because hashes name shared directories.
pub fn fnv64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Eight hex digits of [`fnv64`], enough to tell directories apart.
pub fn short_hash(data: &str) -> String {
    format!("{:016x}", fnv64(data.as_bytes()))[..8].to_string()
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn system_time_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// "12s", "5m", "3h", "2d" relative to now.
pub fn ago(ms: u64) -> String {
    let now = now_ms();
    if ms == 0 || ms > now {
        return "now".into();
    }
    duration_str((now - ms) / 1000)
}

pub fn duration_str(secs: u64) -> String {
    match secs {
        s if s < 120 => format!("{s}s"),
        s if s < 7200 => format!("{}m", s / 60),
        s if s < 172_800 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Parses "50G", "512M", "1.5T", "1000" (bytes). Units are binary (G = GiB).
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let lower = s.to_ascii_lowercase();
    let lower = lower.trim_end_matches("ib").trim_end_matches('b');
    let (num, mult) = match lower.chars().last()? {
        'k' => (&lower[..lower.len() - 1], 1u64 << 10),
        'm' => (&lower[..lower.len() - 1], 1 << 20),
        'g' => (&lower[..lower.len() - 1], 1 << 30),
        't' => (&lower[..lower.len() - 1], 1 << 40),
        _ => (lower, 1),
    };
    let v: f64 = num.trim().parse().ok()?;
    if v < 0.0 {
        return None;
    }
    Some((v * mult as f64) as u64)
}

/// Logical lengths, deduplicated by file identity within the tree.
pub fn dir_size(path: &Path) -> u64 {
    dir_size_for(path, crate::config::CacheAccounting::Logical)
}

pub fn dir_size_for(path: &Path, mode: crate::config::CacheAccounting) -> u64 {
    measure(
        path,
        mode,
        &Default::default(),
        false,
        &mut Default::default(),
    )
    .unwrap_or(0)
}

/// Strict logical measurement; symlinks are not followed.
pub fn path_size(path: &Path) -> io::Result<u64> {
    size_for(path, crate::config::CacheAccounting::Logical)
}

pub fn size_for(path: &Path, mode: crate::config::CacheAccounting) -> io::Result<u64> {
    size_excluding(path, mode, &Default::default())
}

pub fn size_excluding(
    path: &Path,
    mode: crate::config::CacheAccounting,
    excluded: &std::collections::BTreeSet<PathBuf>,
) -> io::Result<u64> {
    measure(path, mode, excluded, true, &mut Default::default())
}

fn measure(
    path: &Path,
    mode: crate::config::CacheAccounting,
    excluded: &std::collections::BTreeSet<PathBuf>,
    strict: bool,
    seen: &mut std::collections::BTreeSet<(u64, u64)>,
) -> io::Result<u64> {
    if excluded.contains(path) {
        return Ok(0);
    }
    let md = match fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(e) if !strict || e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    if md.is_file() {
        return match file_footprint(path, &md, mode) {
            Ok((id, bytes)) => Ok(if seen.insert(id) { bytes } else { 0 }),
            Err(_) if !strict => Ok(md.len()),
            Err(e) => Err(e),
        };
    }
    if !md.is_dir() {
        return Ok(0);
    }
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) if !strict => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut total = 0u64;
    for entry in entries {
        match entry {
            Ok(entry) => {
                total = total.saturating_add(measure(&entry.path(), mode, excluded, strict, seen)?)
            }
            Err(e) if strict => return Err(e),
            Err(_) => {}
        }
    }
    Ok(total)
}

#[cfg(unix)]
pub(crate) fn file_footprint(
    _path: &Path,
    md: &fs::Metadata,
    mode: crate::config::CacheAccounting,
) -> io::Result<((u64, u64), u64)> {
    use std::os::unix::fs::MetadataExt;
    let bytes = match mode {
        crate::config::CacheAccounting::Logical => md.len(),
        crate::config::CacheAccounting::Allocated => md.blocks().saturating_mul(512),
    };
    Ok(((md.dev(), md.ino()), bytes))
}

#[cfg(windows)]
pub(crate) fn file_footprint(
    path: &Path,
    md: &fs::Metadata,
    mode: crate::config::CacheAccounting,
) -> io::Result<((u64, u64), u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_STANDARD_INFO, FileStandardInfo,
        GetFileInformationByHandle, GetFileInformationByHandleEx,
    };
    let file = fs::File::open(path)?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let id = (
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    );
    let bytes = match mode {
        crate::config::CacheAccounting::Logical => md.len(),
        crate::config::CacheAccounting::Allocated => {
            let mut standard: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
            if unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileStandardInfo,
                    (&mut standard as *mut FILE_STANDARD_INFO).cast(),
                    u32::try_from(std::mem::size_of::<FILE_STANDARD_INFO>()).unwrap(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            u64::try_from(standard.AllocationSize)
                .map_err(|_| io::Error::other("negative file allocation size"))?
        }
    };
    Ok((id, bytes))
}

/// Calls `f` for every regular file below `path`, without following symlinks.
pub fn walk(path: &Path, f: &mut dyn FnMut(&Path, &fs::Metadata)) {
    let Ok(rd) = fs::read_dir(path) else { return };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let p = entry.path();
        if ft.is_dir() {
            walk(&p, f);
        } else if ft.is_file()
            && let Ok(md) = entry.metadata()
        {
            f(&p, &md);
        }
    }
}

/// Newest modification time (ms) of the files below `path` (or of `path`
/// itself when it is a file or holds no files).
pub fn newest_mtime(path: &Path) -> u64 {
    let mut newest = 0;
    walk(path, &mut |_, md| {
        if let Ok(t) = md.modified() {
            newest = newest.max(system_time_ms(t));
        }
    });
    if newest == 0 {
        newest = fs::metadata(path)
            .and_then(|m| m.modified())
            .map(system_time_ms)
            .unwrap_or(0);
    }
    newest
}

/// Absolute, `.`/`..`-free spelling of `path` (relative to `base`) whose
/// longest existing prefix is canonicalized, so it compares with `canonical`
/// paths even when the tail does not exist yet.
pub fn resolve(base: &Path, path: &Path) -> PathBuf {
    use std::path::Component;
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let mut norm = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                norm.pop();
            }
            c => norm.push(c),
        }
    }
    let mut tail = Vec::new();
    let mut head = norm.as_path();
    while !head.exists() {
        match (head.parent(), head.file_name()) {
            (Some(p), Some(n)) => {
                tail.push(n.to_os_string());
                head = p;
            }
            _ => return norm,
        }
    }
    let mut out = canonical(head);
    for n in tail.iter().rev() {
        out.push(n);
    }
    out
}

/// `fs::canonicalize` without the `\\?\` prefix on Windows, so the result is
/// usable by tools that do not understand verbatim paths.
pub fn canonical(path: &Path) -> PathBuf {
    match fs::canonicalize(path) {
        Ok(p) => strip_verbatim(p),
        Err(_) => path.to_path_buf(),
    }
}

pub fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\")
        && rest.as_bytes().get(1) == Some(&b':')
    {
        return PathBuf::from(rest.to_string());
    }
    p
}

/// Same path for comparison purposes (case-insensitive and slash-agnostic on Windows).
pub fn same_path(a: &Path, b: &Path) -> bool {
    norm_key(&canonical(a)) == norm_key(&canonical(b))
}

pub fn norm_key(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/').to_string();
    if cfg!(windows) { s.to_lowercase() } else { s }
}

/// Removes a directory tree, first renaming it aside so a half-finished delete
/// (killed process) never leaves a half-valid tree under the original name.
pub fn remove_tree(path: &Path, trash_dir: &Path) -> io::Result<()> {
    remove_tree_tracked(path, trash_dir).map_err(|e| io::Error::new(e.error.kind(), e))
}

#[derive(Debug)]
pub struct RemovalError {
    pub path: PathBuf,
    pub error: io::Error,
}

impl std::fmt::Display for RemovalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.error)
    }
}
impl std::error::Error for RemovalError {}

/// Preserve the actual remaining path when a moved-aside tree cannot be deleted.
pub fn remove_tree_tracked(path: &Path, trash_dir: &Path) -> Result<(), RemovalError> {
    if !path.exists() {
        return Ok(());
    }
    let _ = fs::create_dir_all(trash_dir);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let aside = trash_dir.join(format!("{name}-{}-{}", std::process::id(), now_ms()));
    let target = if fs::rename(path, &aside).is_ok() {
        aside
    } else {
        path.to_path_buf()
    };
    fs::remove_dir_all(&target).map_err(|error| RemovalError {
        path: target,
        error,
    })
}

/// Writes a file atomically enough for our metadata: write a sibling, then rename.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp, data)?;
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            // Fall back to an in-place write (e.g. rename refused across sandboxes).
            fs::write(path, data).map_err(|_| e)
        }
    }
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let data = fs::read(path).ok()?;
    serde_json::from_slice(&data).ok()
}

pub fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let data = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    write_atomic(path, &data)
}

/// Quotes one argument for display (not for execution).
pub fn display_cmd(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.is_empty() || a.contains([' ', '"', '\'']) {
                format!("{a:?}")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn file_stem_lower(p: &str) -> String {
    Path::new(p)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// Free bytes on the volume holding `path`.
pub fn free_disk_bytes(path: &Path) -> Option<u64> {
    let mut probe = path.to_path_buf();
    while !probe.exists() {
        probe = probe.parent()?.to_path_buf();
    }
    platform_free(&probe)
}

#[cfg(windows)]
fn platform_free(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free)
}

#[cfg(unix)]
fn platform_free(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    (r == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardlinks_are_counted_once_per_tree() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        fs::create_dir(dir.join("deps")).unwrap();
        let original = dir.join("deps/artifact");
        fs::write(&original, vec![0u8; 4096]).unwrap();
        fs::hard_link(&original, dir.join("app")).unwrap();
        assert_eq!(path_size(dir).unwrap(), 4096);
        assert_eq!(dir_size(dir), 4096);
    }

    #[test]
    #[cfg(unix)]
    fn sparse_file_allocation_is_distinct_from_logical_length() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sparse");
        fs::File::create(&path)
            .unwrap()
            .set_len(64 * 1024 * 1024)
            .unwrap();
        assert_eq!(path_size(&path).unwrap(), 64 * 1024 * 1024);
        assert!(
            size_for(&path, crate::config::CacheAccounting::Allocated).unwrap()
                < path_size(&path).unwrap()
        );
    }

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("2K"), Some(2048));
        assert_eq!(parse_size("50G"), Some(50 << 30));
        assert_eq!(parse_size("50GiB"), Some(50 << 30));
        assert_eq!(parse_size("1.5g"), Some(3 << 29));
        assert_eq!(parse_size("x"), None);
    }

    #[test]
    fn hash_is_stable() {
        assert_eq!(fnv64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(short_hash("abc"), short_hash("abc"));
        assert_ne!(short_hash("abc"), short_hash("abd"));
    }
}
