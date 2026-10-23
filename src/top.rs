//! Live queue, remote-host, and target-pool monitoring.

use std::fmt::Write as _;
use std::io::{self, IsTerminal, Write};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::gc::{self, Usage};
use crate::queue::ClassStatus;
use crate::util::{duration_str, human_bytes, now_ms};
use crate::{remote, status};

type Hosts = Vec<(String, Result<serde_json::Value, String>)>;

/// Redirected output is a single plain-text snapshot. Interactive monitoring
/// never changes input mode or hides the cursor; ordinary Ctrl+C stops it.
pub fn run(cfg: &Config, interval: Duration, once: bool, hosts: bool) -> io::Result<()> {
    let mut out = io::stdout().lock();
    let terminal = out.is_terminal();
    let once = once || !terminal;
    let mut remote: Option<Hosts> = None;
    let mut worker: Option<JoinHandle<Hosts>> = None;
    let mut last_probe: Option<Instant> = None;
    loop {
        if hosts && !cfg.hosts.is_empty() {
            if once {
                remote = Some(remote::host_status(cfg));
            } else {
                if worker.as_ref().is_some_and(|w| w.is_finished()) {
                    remote = Some(worker.take().unwrap().join().unwrap_or_else(|_| {
                        cfg.hosts
                            .keys()
                            .map(|name| (name.clone(), Err("status worker failed".into())))
                            .collect()
                    }));
                }
                if worker.is_none()
                    && last_probe.is_none_or(|t| t.elapsed() >= Duration::from_secs(5))
                {
                    let cfg = cfg.clone();
                    worker = Some(thread::spawn(move || remote::host_status(&cfg)));
                    last_probe = Some(Instant::now());
                }
            }
        }
        let frame = render(
            &status::classes(cfg),
            &gc::scan(cfg, false),
            remote.as_deref(),
            hosts && !cfg.hosts.is_empty(),
        );
        if terminal && !once {
            write!(out, "\x1b[H\x1b[2J")?;
            let frame = format!(
                "buildq top | refresh {}s | Ctrl+C to stop\n{frame}",
                interval.as_secs()
            );
            let (columns, rows) = terminal_size().unwrap_or((80, 24));
            let lines: Vec<_> = frame.lines().collect();
            let available = rows.saturating_sub(1).max(1);
            let shown = if lines.len() > available {
                available.saturating_sub(1)
            } else {
                lines.len()
            };
            for line in &lines[..shown] {
                writeln!(out, "{}", clip(line, columns.saturating_sub(1)))?;
            }
            if lines.len() > shown {
                writeln!(
                    out,
                    "{}",
                    clip(
                        &format!(
                            "... {} more lines; --once shows the full snapshot",
                            lines.len() - shown
                        ),
                        columns.saturating_sub(1)
                    )
                )?;
            }
        } else {
            writeln!(out, "buildq top")?;
            out.write_all(frame.as_bytes())?;
        }
        out.flush()?;
        if once {
            return Ok(());
        }
        thread::sleep(interval);
    }
}

fn clip(line: &str, columns: usize) -> String {
    let mut used = 0;
    line.chars()
        .take_while(|c| {
            // Conservatively allow two cells for non-ASCII text, avoiding wraps
            // without requiring a terminal-width dependency.
            used += if c.is_ascii() { 1 } else { 2 };
            used <= columns
        })
        .collect()
}

fn terminal_size() -> Option<(usize, usize)> {
    #[cfg(unix)]
    {
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: size is a valid writable winsize; ioctl owns no pointer after returning.
        if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0
            && size.ws_col > 0
            && size.ws_row > 0
        {
            return Some((usize::from(size.ws_col), usize::from(size.ws_row)));
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{
            CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo, GetStdHandle, STD_OUTPUT_HANDLE,
        };
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: the API receives a valid output structure and does not retain it.
        if unsafe { GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT_HANDLE), &mut info) } != 0 {
            let width = i32::from(info.srWindow.Right) - i32::from(info.srWindow.Left) + 1;
            let height = i32::from(info.srWindow.Bottom) - i32::from(info.srWindow.Top) + 1;
            if width > 0 && height > 0 {
                return Some((usize::try_from(width).ok()?, usize::try_from(height).ok()?));
            }
        }
    }
    None
}

fn clean(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn render(
    classes: &[ClassStatus],
    usage: &Usage,
    hosts: Option<&[(String, Result<serde_json::Value, String>)]>,
    show_hosts: bool,
) -> String {
    let mut out = String::new();
    let members: Vec<_> = usage.pools.iter().flat_map(|p| &p.members).collect();
    let _ = writeln!(
        out,
        "Cache: {} / {} effective budget ({} bytes) | {} members ({} busy) | {} run copies | disk free {}",
        human_bytes(usage.total),
        human_bytes(usage.effective_budget),
        usage.accounting.name(),
        members.len(),
        members.iter().filter(|m| m.busy).count(),
        usage.runs.len(),
        usage
            .free_disk
            .map(human_bytes)
            .unwrap_or_else(|| "?".into())
    );
    if let Some(warning) = usage.budget_warning() {
        let _ = writeln!(out, "Warning: {warning}");
    }
    if show_hosts {
        let _ = writeln!(out, "\nRemote hosts:");
        match hosts {
            None => {
                let _ = writeln!(out, "  probing configured hosts...");
            }
            Some(hosts) => {
                for (name, result) in hosts {
                    match result {
                        Ok(value) => match serde_json::from_value::<Vec<ClassStatus>>(
                            value["classes"].clone(),
                        ) {
                            Ok(classes) => {
                                for class in classes {
                                    let _ = writeln!(
                                        out,
                                        "  {} / {}: {}/{} busy, {} waiting",
                                        clean(name),
                                        clean(&class.class),
                                        class.busy(),
                                        class.slots,
                                        class.waiters.len()
                                    );
                                }
                            }
                            Err(_) => {
                                let _ = writeln!(out, "  {}: invalid status", clean(name));
                            }
                        },
                        Err(error) => {
                            let _ =
                                writeln!(out, "  {}: unavailable ({})", clean(name), clean(error));
                        }
                    }
                }
            }
        }
    }
    let now = now_ms();
    for class in classes {
        let _ = writeln!(
            out,
            "\n{}: {}/{} busy, {} waiting",
            clean(&class.class),
            class.busy(),
            class.slots,
            class.waiters.len()
        );
        for slot in &class.holders {
            if !slot.busy {
                continue;
            }
            if let Some(job) = &slot.job {
                let _ = writeln!(
                    out,
                    "  RUN  [{}] {} | pid {} | {}{}{}",
                    slot.slot,
                    duration_str(now.saturating_sub(job.started_ms) / 1000),
                    job.pid,
                    clean(&job.cmd),
                    job.target
                        .as_ref()
                        .map(|t| format!(" | {}", clean(t)))
                        .unwrap_or_default(),
                    job.label
                        .as_ref()
                        .map(|t| format!(" | {}", clean(t)))
                        .unwrap_or_default()
                );
            } else {
                let _ = writeln!(out, "  RUN  [{}] starting", slot.slot);
            }
        }
        for (i, job) in class.waiters.iter().enumerate() {
            let _ = writeln!(
                out,
                "  WAIT #{} {} | prio {} | pid {} | {}{}",
                i + 1,
                duration_str(now.saturating_sub(job.enqueued_ms) / 1000),
                job.priority,
                job.pid,
                clean(&job.cmd),
                job.wait_reason
                    .as_ref()
                    .map(|r| format!(" | {}", clean(r)))
                    .unwrap_or_default()
            );
        }
    }
    if !usage.pools.is_empty() {
        let _ = writeln!(out, "\nTarget pools (size estimates):");
        for pool in &usage.pools {
            for member in &pool.members {
                let _ = writeln!(
                    out,
                    "  {}/m{}: {} | {} | {} jobs",
                    clean(&pool.name),
                    member.id,
                    human_bytes(member.size),
                    if member.busy { "busy" } else { "idle" },
                    member.meta.jobs
                );
            }
        }
    }
    out
}
