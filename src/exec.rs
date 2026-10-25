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
//! A SIGTERM, SIGHUP or SIGINT with `kill_tree` freezes and records the
//! child's tree before the child hears of it, and the guard kills what is left
//! of that tree when the child exits, so a build script orphaned by Cargo's
//! exit cannot keep writing to a member that is already free again.

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

/// Spawn and monitor a guarded build. Pressure aborts always stop its entire
/// owned process tree, including when normal kill-on-parent-exit is disabled.
pub fn run_guarded(
    cmd: &mut Command,
    check: &mut dyn FnMut() -> io::Result<()>,
) -> io::Result<i32> {
    check()?;
    let mut child = cmd.spawn()?;
    let guard = ChildGuard::attach(&child, true);
    wait_guarded(&mut child, guard, check)
}

pub fn wait_guarded(
    child: &mut Child,
    guard: ChildGuard,
    check: &mut dyn FnMut() -> io::Result<()>,
) -> io::Result<i32> {
    loop {
        let status = child.try_wait()?;
        if let Err(error) = check() {
            if status.is_none() {
                #[cfg(unix)]
                nix::kill_tree(child.id() as i32);
                #[cfg(windows)]
                if guard.job.is_none() {
                    // Nested sandbox jobs can prevent assignment to our job.
                    let _ = Command::new("taskkill")
                        .args(["/F", "/T", "/PID", &child.id().to_string()])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                }
            }
            drop(guard);
            if status.is_none() {
                let _ = child.kill();
                let _ = child.wait();
            }
            return Err(error);
        }
        if let Some(status) = status {
            return Ok(exit_code(status));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
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
    #[cfg(unix)]
    tracker: Option<nix::Tracker>,
    #[cfg(unix)]
    child: i32,
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
    /// Write end of the running [`Tracker`]'s pipe, -1 without one.
    static TRACKER: AtomicI32 = AtomicI32::new(-1);

    /// Ctrl+C reaches the child from the terminal; only the tracker hears it.
    pub extern "C" fn on_int(sig: libc::c_int) {
        notify(sig);
    }

    pub extern "C" fn forward(sig: libc::c_int) {
        if notify(sig) {
            return;
        }
        let pid = CHILD.load(Ordering::SeqCst);
        if pid > 0 {
            unsafe {
                libc::kill(pid, sig);
            }
        }
    }

    /// Hands the signal to the tracker, which forwards it itself.
    fn notify(sig: libc::c_int) -> bool {
        let fd = TRACKER.load(Ordering::SeqCst);
        if fd < 0 {
            return false;
        }
        let b = sig as u8;
        unsafe { libc::write(fd, (&raw const b).cast(), 1) };
        true
    }

    /// Records the child's process tree once a cancelling signal arrives and
    /// keeps following its forks until the child has exited.
    pub struct Tracker {
        wake: i32,
        thread: std::thread::JoinHandle<Tracked>,
    }

    #[derive(Default)]
    pub struct Tracked {
        pub tree: Vec<i32>,
        /// Ctrl+C (or SIGQUIT) reached the whole job, not just buildq.
        pub interrupted: bool,
        /// Orphans already in our process group when the child started.
        pub foreign: Vec<i32>,
    }

    impl Tracker {
        pub fn start(child: i32) -> Option<Tracker> {
            let mut fds = [0; 2];
            if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
                return None;
            }
            for fd in fds {
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
            }
            let [read, wake] = fds;
            let thread = std::thread::spawn(move || {
                let tree = track(child, read);
                unsafe { libc::close(read) };
                tree
            });
            TRACKER.store(wake, Ordering::SeqCst);
            Some(Tracker { wake, thread })
        }

        /// Stops tracking (the child has exited) and returns the recorded
        /// tree, empty when no cancelling signal arrived.
        pub fn finish(self) -> Tracked {
            TRACKER.store(-1, Ordering::SeqCst);
            let stop = 0u8;
            unsafe { libc::write(self.wake, (&raw const stop).cast(), 1) };
            let tree = self.thread.join().unwrap_or_default();
            unsafe { libc::close(self.wake) };
            tree
        }
    }

    /// Reads one signal number per byte; 0 means the child is gone.
    fn next_signal(fd: i32, timeout_ms: i32) -> Option<Option<i32>> {
        let mut p = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut p, 1, timeout_ms) } <= 0 {
            return Some(None);
        }
        let mut b = 0u8;
        if unsafe { libc::read(fd, (&raw mut b).cast(), 1) } != 1 || b == 0 {
            return None;
        }
        Some(Some(i32::from(b)))
    }

    fn track(child: i32, fd: i32) -> Tracked {
        let mut tracked = Tracked {
            foreign: group_orphans(),
            ..Tracked::default()
        };
        let tree = &mut tracked.tree;
        // A child that survives the signal (a REPL after Ctrl+C) may run on
        // for long: follow its forks, but at a modest rate.
        while let Some(sig) = next_signal(fd, if tree.is_empty() { -1 } else { 250 }) {
            let Some(sig) = sig else {
                extend(tree, false);
                continue;
            };
            if tree.is_empty() {
                // Frozen, nothing can exit (and orphan its children) or fork
                // between the scan and the signal.
                tree.push(child);
                unsafe { libc::kill(child, libc::SIGSTOP) };
                extend(tree, true);
            }
            if sig == libc::SIGINT || sig == libc::SIGQUIT {
                tracked.interrupted = true;
            } else {
                unsafe { libc::kill(child, sig) };
            }
            for &pid in tree.iter() {
                unsafe { libc::kill(pid, libc::SIGCONT) };
            }
        }
        tracked
    }

    /// Orphans in our process group. The terminal's Ctrl+C can end Cargo
    /// before the tracker freezes it, and a helper that ignores the signal is
    /// then adopted by init; it is still in the job buildq runs in.
    pub fn group_orphans() -> Vec<i32> {
        let group = unsafe { libc::getpgrp() };
        let me = std::process::id() as i32;
        processes()
            .into_iter()
            .filter(|&(pid, ppid, pgid)| pgid == group && ppid == 1 && pid != me)
            .map(|(pid, ..)| pid)
            .collect()
    }

    /// Adds the descendants of `tree`, stopping each as it is found if asked.
    fn extend(tree: &mut Vec<i32>, stop: bool) {
        loop {
            let found: Vec<i32> = processes()
                .into_iter()
                .filter(|(pid, ppid, _)| tree.contains(ppid) && !tree.contains(pid))
                .map(|(pid, ..)| pid)
                .collect();
            if found.is_empty() {
                break;
            }
            for pid in found {
                if stop {
                    unsafe { libc::kill(pid, libc::SIGSTOP) };
                }
                tree.push(pid);
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

    /// (pid, ppid, pgid) of every process.
    #[cfg(target_os = "linux")]
    fn processes() -> Vec<(i32, i32, i32)> {
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        dir.flatten()
            .filter_map(|e| {
                let pid = e.file_name().to_str()?.parse().ok()?;
                let stat = std::fs::read_to_string(e.path().join("stat")).ok()?;
                // `pid (comm) state ppid pgrp ...`; comm may contain spaces.
                let mut it = stat.rsplit_once(')')?.1.split_whitespace().skip(1);
                Some((pid, it.next()?.parse().ok()?, it.next()?.parse().ok()?))
            })
            .collect()
    }

    /// (pid, ppid, pgid) of every process.
    #[cfg(not(target_os = "linux"))]
    fn processes() -> Vec<(i32, i32, i32)> {
        let Ok(out) = Command::new("ps")
            .args(["-A", "-o", "pid=", "-o", "ppid=", "-o", "pgid="])
            .output()
        else {
            return Vec::new();
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace().map(str::parse);
                Some((it.next()?.ok()?, it.next()?.ok()?, it.next()?.ok()?))
            })
            .collect()
    }

    /// Kills `root` and all its descendants. Each is stopped as it is found,
    /// so none can fork past the scan or be reparented before the kill.
    pub fn kill_tree(root: i32) {
        let mut tree = vec![root];
        unsafe { libc::kill(root, libc::SIGSTOP) };
        extend(&mut tree, true);
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

/// A process that can still execute. Linux zombies have already terminated,
/// even when container PID 1 has not yet reaped them.
#[cfg(unix)]
pub fn process_alive(pid: i32) -> bool {
    nix::alive(pid)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn a_terminated_unreaped_child_is_not_running() {
        assert!(process_alive(std::process::id() as i32));
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "util::tests::hash_is_stable"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let started = std::time::Instant::now();
        while process_alive(pid) {
            assert!(started.elapsed() < std::time::Duration::from_secs(10));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // No wait has happened: kill(0) still sees the child's zombie entry.
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
        assert!(child.wait().unwrap().success());
        assert!(!process_alive(pid));
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
            let pid = child.id() as i32;
            // The watchdog is forked (pre_exec) and keeps every descriptor
            // open at that moment: start the tracker, whose `ps` pipes are
            // briefly inheritable on macOS, only once the fork is done.
            let watchdog = nix::spawn_watchdog(child.id(), kill_tree);
            ChildGuard {
                tracker: kill_tree.then(|| nix::Tracker::start(pid)).flatten(),
                watchdog,
                child: pid,
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
        // A cancelled child may have left build scripts behind: they still
        // hold the member, so the slot stays ours until they are gone.
        #[cfg(unix)]
        if let Some(t) = self.tracker.take() {
            let mut tracked = t.finish();
            if tracked.interrupted {
                let foreign = tracked.foreign;
                tracked.tree.extend(
                    nix::group_orphans()
                        .into_iter()
                        .filter(|pid| !foreign.contains(pid)),
                );
            }
            let left: Vec<i32> = tracked
                .tree
                .into_iter()
                .filter(|&pid| pid != self.child && nix::alive(pid))
                .collect();
            for &pid in &left {
                nix::kill_tree(pid);
            }
            while left.iter().any(|&pid| nix::alive(pid)) {
                std::thread::sleep(std::time::Duration::from_millis(20));
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
