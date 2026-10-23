//! Running a child with exit-code passthrough and sane Ctrl+C behaviour.
//!
//! While a child runs, buildq ignores Ctrl+C itself (the console / terminal
//! delivers it to the child too) so it can report the child's exit code and
//! release its slot after the child is gone. On Windows the child is put in a
//! kill-on-close job object: if buildq is killed, the child tree dies with it,
//! so a freed slot never hides a still-running build. On Unix a watchdog
//! process (`buildq __watch`) inherits the locks buildq holds: if buildq dies
//! first, it kills the child tree (or, without `kill_tree`, waits for the
//! child) before the locks, and with them the slot and pool member, go free.

use std::io;
use std::process::{Child, Command, ExitStatus};

/// Exit code as the shell would see it (128+signal for signal deaths on Unix).
pub fn exit_code(st: ExitStatus) -> i32 {
    if let Some(c) = st.code() {
        return c;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = st.signal() {
            return 128 + sig;
        }
    }
    1
}

/// Spawns, waits, returns the exit code.
pub fn run(cmd: &mut Command, kill_tree: bool) -> io::Result<i32> {
    let mut child = cmd.spawn()?;
    let _guard = ChildGuard::attach(&child, kill_tree);
    let st = child.wait()?;
    Ok(exit_code(st))
}

/// Spawns a process that outlives us (background gc).
pub fn spawn_detached(cmd: &mut Command) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // Break away from a sandbox job if allowed, otherwise stay inside it.
        if cmd
            .creation_flags(base | CREATE_BREAKAWAY_FROM_JOB)
            .spawn()
            .is_ok()
        {
            return Ok(());
        }
        cmd.creation_flags(base).spawn().map(|_| ())
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .map(|_| ())
    }
}

pub struct ChildGuard {
    #[cfg(windows)]
    job: Option<windows_sys::Win32::Foundation::HANDLE>,
    #[cfg(unix)]
    watchdog: Option<Child>,
}

#[cfg(windows)]
mod win {
    use windows_sys::Win32::Foundation::{FALSE, TRUE};
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT};
    use windows_sys::core::BOOL;

    pub unsafe extern "system" fn ignore_ctrl_c(kind: u32) -> BOOL {
        if kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT {
            TRUE
        } else {
            FALSE
        }
    }
}

#[cfg(unix)]
mod nix {
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicI32, Ordering};
    pub static CHILD: AtomicI32 = AtomicI32::new(0);

    pub extern "C" fn on_int(_: libc::c_int) {}

    pub extern "C" fn forward(sig: libc::c_int) {
        let pid = CHILD.load(Ordering::SeqCst);
        if pid > 0 {
            unsafe {
                libc::kill(pid, sig);
            }
        }
    }

    /// Starts `buildq __watch` holding copies of our lock handles. Its stdin
    /// is a pipe only we write to, so it reads EOF exactly when we are gone.
    pub fn spawn_watchdog(child: u32, kill_tree: bool) -> Option<Child> {
        let fds = crate::lock::held_fds();
        let mut cmd = Command::new(std::env::current_exe().ok()?);
        cmd.arg0("buildq")
            .env("BUILDQ_AS_CLI", "1")
            .args(["__watch", &child.to_string()])
            .args(kill_tree.then_some("--kill-tree"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Out of the terminal's process group: Ctrl+C is not for it.
            .process_group(0);
        unsafe {
            cmd.pre_exec(move || {
                for &fd in &fds {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags >= 0 {
                        libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
                    }
                }
                Ok(())
            });
        }
        cmd.spawn().ok()
    }

    pub fn alive(pid: i32) -> bool {
        if unsafe { libc::kill(pid, 0) } != 0 {
            return false;
        }
        // An unreaped zombie (no reaping init in a container) is not running.
        #[cfg(target_os = "linux")]
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            && let Some((_, rest)) = stat.rsplit_once(')')
        {
            return !rest.trim_start().starts_with('Z');
        }
        true
    }

    /// (pid, ppid) of every process.
    #[cfg(target_os = "linux")]
    fn processes() -> Vec<(i32, i32)> {
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        dir.flatten()
            .filter_map(|e| {
                let pid = e.file_name().to_str()?.parse().ok()?;
                let stat = std::fs::read_to_string(e.path().join("stat")).ok()?;
                // `pid (comm) state ppid ...`; comm may contain spaces.
                let ppid = stat.rsplit_once(')')?.1.split_whitespace().nth(1)?;
                Some((pid, ppid.parse().ok()?))
            })
            .collect()
    }

    /// (pid, ppid) of every process.
    #[cfg(not(target_os = "linux"))]
    fn processes() -> Vec<(i32, i32)> {
        let Ok(out) = Command::new("ps")
            .args(["-A", "-o", "pid=", "-o", "ppid="])
            .output()
        else {
            return Vec::new();
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace().map(str::parse);
                Some((it.next()?.ok()?, it.next()?.ok()?))
            })
            .collect()
    }

    /// Kills `root` and all its descendants. Each is stopped as it is found,
    /// so none can fork past the scan or be reparented before the kill.
    pub fn kill_tree(root: i32) {
        let mut tree = vec![root];
        unsafe { libc::kill(root, libc::SIGSTOP) };
        loop {
            let found: Vec<i32> = processes()
                .into_iter()
                .filter(|(pid, ppid)| tree.contains(ppid) && !tree.contains(pid))
                .map(|(pid, _)| pid)
                .collect();
            if found.is_empty() {
                break;
            }
            for pid in found {
                unsafe { libc::kill(pid, libc::SIGSTOP) };
                tree.push(pid);
            }
        }
        for &pid in &tree {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// `buildq __watch <pid>`: holds the inherited locks until buildq is gone and
/// then until `pid` is, killing its tree first when `kill_tree` is set.
#[cfg(unix)]
pub fn watch(pid: i32, kill_tree: bool) {
    use std::io::Read;
    // buildq never writes: this returns when it exits or is killed.
    let _ = io::stdin().read(&mut [0u8; 1]);
    if kill_tree {
        nix::kill_tree(pid);
    }
    while nix::alive(pid) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

impl ChildGuard {
    pub fn attach(child: &Child, kill_tree: bool) -> ChildGuard {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Foundation::TRUE;
            use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
            use windows_sys::Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JobObjectExtendedLimitInformation, SetInformationJobObject,
            };
            unsafe {
                // A handler (unlike SetConsoleCtrlHandler(NULL, TRUE)) is not
                // inherited, so the child still reacts to Ctrl+C.
                SetConsoleCtrlHandler(Some(win::ignore_ctrl_c), TRUE);
            }
            let mut job = None;
            if kill_tree {
                unsafe {
                    let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                    if !h.is_null() {
                        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                        limits.BasicLimitInformation.LimitFlags =
                            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
                        SetInformationJobObject(
                            h,
                            JobObjectExtendedLimitInformation,
                            &limits as *const _ as *const _,
                            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                        );
                        if AssignProcessToJobObject(h, child.as_raw_handle() as _) != 0 {
                            job = Some(h);
                        } else {
                            windows_sys::Win32::Foundation::CloseHandle(h);
                        }
                    }
                }
            }
            ChildGuard { job }
        }
        #[cfg(unix)]
        {
            nix::CHILD.store(child.id() as i32, std::sync::atomic::Ordering::SeqCst);
            unsafe {
                libc::signal(libc::SIGINT, nix::on_int as *const () as libc::sighandler_t);
                libc::signal(
                    libc::SIGQUIT,
                    nix::on_int as *const () as libc::sighandler_t,
                );
                libc::signal(
                    libc::SIGTERM,
                    nix::forward as *const () as libc::sighandler_t,
                );
                libc::signal(
                    libc::SIGHUP,
                    nix::forward as *const () as libc::sighandler_t,
                );
            }
            ChildGuard {
                watchdog: nix::spawn_watchdog(child.id(), kill_tree),
            }
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::Foundation::FALSE;
            use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
            SetConsoleCtrlHandler(Some(win::ignore_ctrl_c), FALSE);
            // The child has exited; closing the job kills stragglers it left.
            if let Some(h) = self.job.take() {
                windows_sys::Win32::Foundation::CloseHandle(h);
            }
        }
        #[cfg(unix)]
        if let Some(mut w) = self.watchdog.take() {
            // Its copies of our lock handles must close before ours do.
            let _ = w.kill();
            let _ = w.wait();
        }
        #[cfg(unix)]
        unsafe {
            nix::CHILD.store(0, std::sync::atomic::Ordering::SeqCst);
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGQUIT, libc::SIG_DFL);
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            libc::signal(libc::SIGHUP, libc::SIG_DFL);
        }
    }
}
