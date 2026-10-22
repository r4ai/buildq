//! Running a child with exit-code passthrough and sane Ctrl+C behaviour.
//!
//! While a child runs, buildq ignores Ctrl+C itself (the console / terminal
//! delivers it to the child too) so it can report the child's exit code and
//! release its slot after the child is gone. On Windows the child is put in a
//! kill-on-close job object: if buildq is killed, the child tree dies with it,
//! so a freed slot never hides a still-running build.

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
            let _ = kill_tree;
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
            ChildGuard {}
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
        unsafe {
            nix::CHILD.store(0, std::sync::atomic::Ordering::SeqCst);
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGQUIT, libc::SIG_DFL);
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            libc::signal(libc::SIGHUP, libc::SIG_DFL);
        }
    }
}
