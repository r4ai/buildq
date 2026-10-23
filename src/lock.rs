//! OS file locks (LockFileEx on Windows, flock on Unix) via `std::fs::File::lock`.
//!
//! A lock lives exactly as long as its open handle, so the OS releases it when
//! the holder exits or is killed: no stale-lock cleanup is ever needed. Lock
//! files carry no data (Windows locks are mandatory, so a locked file cannot be
//! read by others); metadata sits in a sibling `.json` file.
//!
//! On Unix the descriptors of held locks are tracked so the child watchdog
//! (`exec`) can inherit them and keep the locks for as long as the build lives.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct LockFile {
    _file: File,
    pub path: PathBuf,
}

#[cfg(unix)]
static HELD: std::sync::Mutex<Vec<std::os::fd::RawFd>> = std::sync::Mutex::new(Vec::new());

impl LockFile {
    fn new(file: File, path: &Path) -> LockFile {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            HELD.lock().unwrap().push(file.as_raw_fd());
        }
        LockFile {
            _file: file,
            path: path.to_path_buf(),
        }
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let fd = self._file.as_raw_fd();
            HELD.lock().unwrap().retain(|&f| f != fd);
        }
    }
}

/// Descriptors of the locks this process holds right now.
#[cfg(unix)]
pub fn held_fds() -> Vec<std::os::fd::RawFd> {
    HELD.lock().unwrap().clone()
}

fn open(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
    {
        Ok(f) => Ok(f),
        // Created by another sandbox and not writable for us: a read handle
        // is enough to take the lock.
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            OpenOptions::new().read(true).open(path)
        }
        Err(e) => Err(e),
    }
}

/// Takes the lock if it is free right now.
pub fn try_lock(path: &Path) -> io::Result<Option<LockFile>> {
    let file = open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(LockFile::new(file, path))),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

/// Blocks until the lock is ours.
pub fn lock(path: &Path) -> io::Result<LockFile> {
    let file = open(path)?;
    file.lock()?;
    Ok(LockFile::new(file, path))
}

/// True when some live process holds the lock. Missing files are free.
pub fn is_held(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    matches!(try_lock(path), Ok(None))
}
