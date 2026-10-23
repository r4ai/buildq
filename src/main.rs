use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

use buildq::config::{self, Config};
use buildq::job::{self, Opts};
use buildq::queue::{ClassQueue, ClassStatus};
use buildq::util::{ago, duration_str, human_bytes, now_ms, parse_size};
use buildq::{gc, history, remote, shim};

#[derive(Parser)]
#[command(
    name = "buildq",
    version,
    about = "Machine-wide queue and shared target-dir pool for cargo jobs"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a command once a slot is free (`cargo` commands also get a pool member).
    Run {
        /// Higher runs earlier; FIFO within a priority.
        #[arg(short, long, default_value_t = 0, allow_negative_numbers = true)]
        priority: i32,
        /// Queue class: build (default), run, or one from [classes].
        #[arg(long, default_value = "build")]
        class: String,
        /// Run on a configured ssh host, `auto`, or a comma list of candidates.
        #[arg(long)]
        host: Option<String>,
        /// Free-form label shown in status/history.
        #[arg(long)]
        label: Option<String>,
        /// Remote only: path (relative to the worktree) to copy back afterwards.
        #[arg(long)]
        fetch: Vec<String>,
        /// Remote only: where fetched paths go (default target/buildq-remote/<host>).
        #[arg(long)]
        fetch_into: Option<PathBuf>,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },
    /// What the cargo shim does: `buildq cargo build -p x`.
    #[command(disable_help_flag = true)]
    Cargo {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Slot holders and waiters.
    Status {
        #[arg(long)]
        json: bool,
        /// Also ask every configured host (needs `buildq` set for the host).
        #[arg(long)]
        hosts: bool,
    },
    /// Recent jobs.
    History {
        #[arg(short = 'n', default_value_t = 20)]
        n: usize,
        #[arg(long)]
        json: bool,
    },
    /// Inspect the target-dir pool.
    Cache {
        #[command(subcommand)]
        cmd: CacheCmd,
    },
    /// Prune caches to the disk budget.
    Gc {
        #[arg(long)]
        dry_run: bool,
        /// Override the budget for this run, e.g. 40G.
        #[arg(long)]
        budget: Option<String>,
        #[arg(long, hide = true)]
        auto: bool,
        #[arg(long)]
        json: bool,
    },
    /// Install a shim so plain `cargo` goes through buildq.
    Shim {
        #[command(subcommand)]
        cmd: ShimCmd,
    },
    /// Print the effective configuration.
    Config,
    #[command(name = "__helper", hide = true)]
    Helper {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum CacheCmd {
    /// Pools, members and sizes.
    Ls {
        #[arg(long)]
        json: bool,
        /// Re-measure sizes instead of using recorded ones.
        #[arg(long)]
        refresh: bool,
    },
}

#[derive(Subcommand)]
enum ShimCmd {
    Install {
        /// Tool to shim (only `cargo`).
        tool: String,
        #[arg(long)]
        into: PathBuf,
        /// Also write a sh script (default on Unix).
        #[arg(long)]
        sh: bool,
    },
}

fn exit(code: i32) -> ExitCode {
    // Exit codes above 255 (Windows NTSTATUS values) need the raw call.
    if !(0..=255).contains(&code) {
        std::process::exit(code);
    }
    ExitCode::from(code as u8)
}

fn main() -> ExitCode {
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("buildq: config: {e}");
            return ExitCode::from(2);
        }
    };
    if shim::invoked_as_cargo() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        return finish(job::run_cargo(&cfg, &Opts::from_env(), &args));
    }
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Run {
            priority,
            class,
            host,
            label,
            fetch,
            fetch_into,
            cmd,
        } => {
            let opts = Opts {
                class,
                priority,
                label,
            };
            let r = match host {
                Some(h) => remote::run(&cfg, &opts, &h, &cmd, &fetch, fetch_into),
                None => job::run_command(&cfg, &opts, &cmd),
            };
            finish(r)
        }
        Cmd::Cargo { args } => finish(job::run_cargo(&cfg, &Opts::from_env(), &args)),
        Cmd::Status { json, hosts } => status(&cfg, json, hosts),
        Cmd::History { n, json } => {
            let entries = history::read(&cfg.queue_dir, n);
            if json {
                println!("{}", serde_json::to_string_pretty(&entries).unwrap());
            } else {
                for e in entries {
                    let wait = e.started_ms.saturating_sub(e.enqueued_ms) / 1000;
                    let run = e.ended_ms.saturating_sub(e.started_ms) / 1000;
                    println!(
                        "{:>5} ago  {:<6} exit {:<4} wait {:>4} run {:>4}  {}  {}{}",
                        ago(e.ended_ms),
                        e.class,
                        e.exit,
                        duration_str(wait),
                        duration_str(run),
                        e.cmd,
                        e.target
                            .or(e.external.map(|x| format!("explicit {x}")))
                            .map(|t| format!("[{t}] "))
                            .unwrap_or_default(),
                        e.cwd
                    );
                }
            }
            ExitCode::SUCCESS
        }
        Cmd::Cache {
            cmd: CacheCmd::Ls { json, refresh },
        } => {
            let u = gc::scan_all(&cfg, refresh);
            if json {
                println!("{}", serde_json::to_string_pretty(&u).unwrap());
            } else {
                print!("{}", gc::render(&u));
            }
            ExitCode::SUCCESS
        }
        Cmd::Gc {
            dry_run,
            budget,
            auto,
            json,
        } => {
            let budget = match budget.as_deref().map(parse_size) {
                Some(None) => {
                    eprintln!("buildq: bad --budget");
                    return ExitCode::from(2);
                }
                Some(b) => b,
                None => None,
            };
            let r = gc::gc(
                &cfg,
                &gc::Options {
                    dry_run,
                    budget,
                    auto,
                },
            );
            if json {
                println!("{}", serde_json::to_string_pretty(&r).unwrap());
            } else if !auto {
                if let Some(s) = &r.skipped {
                    println!("skipped: {s}");
                }
                for a in &r.actions {
                    println!("{}{a}", if dry_run { "would " } else { "" });
                }
                println!(
                    "{} {}; pool {} -> {} (budget {})",
                    if dry_run { "would free" } else { "freed" },
                    human_bytes(r.freed),
                    human_bytes(r.before),
                    human_bytes(r.after),
                    human_bytes(r.effective_budget)
                );
                if r.external > 0 {
                    println!(
                        "explicit target dirs outside the pool: {} (see `buildq cache ls`)",
                        human_bytes(r.external)
                    );
                }
                for w in &r.warnings {
                    println!("warning: {w}");
                }
            }
            ExitCode::SUCCESS
        }
        Cmd::Shim {
            cmd: ShimCmd::Install { tool, into, sh },
        } => match shim::install(&tool, &into, sh) {
            Ok(files) => {
                for f in files {
                    println!("wrote {}", f.display());
                }
                println!(
                    "put {} first on PATH",
                    buildq::util::canonical(&into).display()
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("buildq: {e}");
                ExitCode::from(1)
            }
        },
        Cmd::Config => {
            print!("{}", config::describe(&cfg));
            ExitCode::SUCCESS
        }
        Cmd::Helper { args } => helper(&args),
    }
}

fn finish(r: Result<i32, String>) -> ExitCode {
    match r {
        Ok(c) => exit(c),
        Err(e) => {
            eprintln!("buildq: {e}");
            ExitCode::from(125)
        }
    }
}

fn status(cfg: &Config, json: bool, hosts: bool) -> ExitCode {
    let classes: Vec<ClassStatus> = cfg
        .known_classes()
        .iter()
        .map(|c| {
            ClassQueue::new(
                &cfg.queue_dir,
                c,
                cfg.class_slots(c),
                Duration::from_millis(cfg.poll_ms),
            )
            .status()
        })
        .collect();
    let remote = if hosts {
        remote::host_status(cfg)
    } else {
        Vec::new()
    };
    if json {
        let hosts: serde_json::Map<String, serde_json::Value> = remote
            .into_iter()
            .map(|(h, r)| (h, r.unwrap_or_else(|e| serde_json::json!({ "error": e }))))
            .collect();
        let v = serde_json::json!({ "classes": classes, "hosts": hosts });
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
        return ExitCode::SUCCESS;
    }
    for st in &classes {
        print_class(st, "");
    }
    for (h, r) in remote {
        println!("== host {h}");
        match r {
            Ok(v) => {
                let parsed: Vec<ClassStatus> =
                    serde_json::from_value(v["classes"].clone()).unwrap_or_default();
                for st in &parsed {
                    print_class(st, "  ");
                }
            }
            Err(e) => println!("  {e}"),
        }
    }
    ExitCode::SUCCESS
}

fn print_class(st: &ClassStatus, indent: &str) {
    if st.busy() == 0 && st.waiters.is_empty() && st.class.starts_with("host-") {
        println!("{indent}{}: {} slots, idle", st.class, st.slots);
        return;
    }
    println!(
        "{indent}{}: {}/{} busy, {} waiting",
        st.class,
        st.busy(),
        st.slots,
        st.waiters.len()
    );
    for h in &st.holders {
        match (&h.busy, &h.job) {
            (true, Some(j)) => println!(
                "{indent}  [{}] {:>5}  {}  (pid {}{}, {})",
                h.slot,
                ago(j.started_ms),
                j.cmd,
                j.pid,
                j.target
                    .as_ref()
                    .map(|t| format!(", {t}"))
                    .unwrap_or_default(),
                j.cwd
            ),
            (true, None) => println!("{indent}  [{}] busy", h.slot),
            _ => println!("{indent}  [{}] free", h.slot),
        }
    }
    for (i, w) in st.waiters.iter().enumerate() {
        println!(
            "{indent}  #{} prio {} waiting {:>5}  {}  (pid {}, {})",
            i + 1,
            w.priority,
            duration_str(now_ms().saturating_sub(w.enqueued_ms) / 1000),
            w.cmd,
            w.pid,
            w.cwd
        );
    }
}

/// Test helpers that behave the same on every platform.
fn helper(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("sleep") => {
            let ms: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1000);
            std::thread::sleep(Duration::from_millis(ms));
            ExitCode::SUCCESS
        }
        Some("exit") => exit(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0)),
        Some("span") => {
            // span <dir> <ms>: record [start, end] of this process for overlap checks.
            let dir = PathBuf::from(&args[1]);
            let ms: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100);
            let start = now_ms();
            std::thread::sleep(Duration::from_millis(ms));
            let end = now_ms();
            let _ = std::fs::write(
                dir.join(format!("span-{}", std::process::id())),
                format!("{start} {end}"),
            );
            ExitCode::SUCCESS
        }
        Some("env") => {
            for k in &args[1..] {
                println!("{k}={}", std::env::var(k).unwrap_or_default());
            }
            ExitCode::SUCCESS
        }
        _ => ExitCode::from(2),
    }
}
