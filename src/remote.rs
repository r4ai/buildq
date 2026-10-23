//! Running queued jobs on other machines over ssh.
//!
//! Protocol (all plain ssh + git, nothing to install remotely except git and
//! the toolchain; a remote buildq is optional):
//!
//! 1. Take a local `host-<name>` slot (the host's `slots` bound how many jobs
//!    this machine sends there at once).
//! 2. Snapshot the worktree as a parentless commit (tracked + untracked, minus
//!    ignored) without touching HEAD, the index or files.
//! 3. `git init --bare <root>/<repo>-<hash>/repo.git` on the host (idempotent)
//!    and `git push` the snapshot to `refs/buildq/<worktree-id>`. Only objects
//!    the host lacks travel; a local ref `refs/buildq/remote/<host>/<id>` keeps
//!    the previous snapshot alive so the next push is a delta.
//! 4. Check it out into `<root>/<repo>-<hash>/wt/<worktree-id>` (a `git
//!    worktree` of the bare repo, so objects are shared). git only rewrites
//!    changed files, so mtimes, and with them cargo's incremental state on
//!    the host, survive between jobs.
//! 5. Run the command in the matching subdirectory, streaming output; with
//!    `buildq` configured for the host it runs as `buildq run -- <cmd>` so the
//!    host's own queue and pool apply.
//! 6. Optionally copy `--fetch` paths back; return the remote exit code.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::{Config, HostConfig};
use crate::git::{self, Worktree};
use crate::job::{self, Opts};
use crate::util::{display_cmd, read_json, short_hash, write_json};

pub trait Transport {
    /// Runs `argv` on the host in `cwd` (relative to the remote home unless
    /// absolute). Streams output unless `capture`; returns (exit code, stdout).
    fn exec(
        &self,
        cwd: Option<&str>,
        env: &[(String, String)],
        argv: &[String],
        capture: bool,
    ) -> io::Result<(i32, String)>;
    /// Something `git push` accepts for a remote path.
    fn git_url(&self, path: &str) -> String;
    /// Extra environment for local git talking to the host.
    fn git_env(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    /// Copies a remote file or directory into the local directory `into`.
    fn fetch(&self, remote: &str, into: &Path) -> io::Result<()>;
}

pub struct Ssh {
    pub dest: String,
    pub args: Vec<String>,
}

/// POSIX shell single-quoting.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:@%+,".contains(&b))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

pub fn remote_script(cwd: Option<&str>, env: &[(String, String)], argv: &[String]) -> String {
    let mut s = String::new();
    if let Some(d) = cwd {
        s.push_str(&format!("cd {} && ", sh_quote(d)));
    }
    if !env.is_empty() {
        s.push_str("env ");
        for (k, v) in env {
            s.push_str(&format!("{}={} ", k, sh_quote(v)));
        }
    }
    s.push_str(
        &argv
            .iter()
            .map(|a| sh_quote(a))
            .collect::<Vec<_>>()
            .join(" "),
    );
    s
}

impl Transport for Ssh {
    fn exec(
        &self,
        cwd: Option<&str>,
        env: &[(String, String)],
        argv: &[String],
        capture: bool,
    ) -> io::Result<(i32, String)> {
        let mut c = Command::new("ssh");
        c.args(&self.args);
        // A tty forwards Ctrl+C to the remote job and keeps colours.
        if !capture && io::stdin().is_terminal() && io::stdout().is_terminal() {
            c.arg("-t");
        }
        c.arg(&self.dest).arg(remote_script(cwd, env, argv));
        if capture {
            let out = c.stdin(Stdio::null()).stderr(Stdio::inherit()).output()?;
            Ok((
                crate::exec::exit_code(out.status),
                String::from_utf8_lossy(&out.stdout).to_string(),
            ))
        } else {
            Ok((crate::exec::run(&mut c, true)?, String::new()))
        }
    }

    fn git_url(&self, path: &str) -> String {
        format!("{}:{}", self.dest, path)
    }

    fn git_env(&self) -> Vec<(String, String)> {
        if self.args.is_empty() {
            return Vec::new();
        }
        vec![(
            "GIT_SSH_COMMAND".into(),
            format!("ssh {}", self.args.join(" ")),
        )]
    }

    fn fetch(&self, remote: &str, into: &Path) -> io::Result<()> {
        fs::create_dir_all(into)?;
        let mut c = Command::new("scp");
        c.arg("-r").arg("-q");
        for a in &self.args {
            // ssh -p PORT is scp -P PORT.
            if a == "-p" {
                c.arg("-P");
            } else {
                c.arg(a);
            }
        }
        c.arg(format!("{}:{}", self.dest, remote)).arg(into);
        let st = c.status()?;
        if st.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!("scp {remote} failed")))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub repo_dir: String,
    pub wt_dir: String,
    pub wt_id: String,
    pub remote_ref: String,
}

pub fn layout(root: &str, wt: &Worktree) -> Layout {
    let root = root.trim_end_matches('/');
    let repo = format!("{}-{}", wt.repo_name(), short_hash(&wt.repo_key()));
    let base = wt
        .root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "wt".into());
    let wt_id = format!("{base}-{}", short_hash(&wt.key()));
    Layout {
        repo_dir: format!("{root}/{repo}/repo.git"),
        wt_dir: format!("{root}/{repo}/wt/{wt_id}"),
        remote_ref: format!("refs/buildq/{wt_id}"),
        wt_id,
    }
}

fn ok(r: io::Result<(i32, String)>, what: &str) -> Result<String, String> {
    match r {
        Ok((0, out)) => Ok(out),
        Ok((c, _)) => Err(format!("{what}: exit {c}")),
        Err(e) => Err(format!("{what}: {e}")),
    }
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// Steps 2-4. Returns the snapshot commit.
pub fn sync(
    t: &dyn Transport,
    host: &str,
    wt: &Worktree,
    l: &Layout,
    scratch: &Path,
) -> Result<String, String> {
    let sha = git::snapshot(wt, scratch)?;
    ok(
        t.exec(
            None,
            &[],
            &s(&["git", "init", "--bare", "-q", &l.repo_dir]),
            true,
        ),
        "remote git init",
    )?;
    let mut push = git::git(&wt.root);
    push.args([
        "push",
        "-q",
        "-f",
        &t.git_url(&l.repo_dir),
        &format!("{sha}:{}", l.remote_ref),
    ]);
    for (k, v) in t.git_env() {
        push.env(k, v);
    }
    let st = push.status().map_err(|e| format!("git push: {e}"))?;
    if !st.success() {
        return Err(format!("git push to {host} failed"));
    }
    let _ = git::git(&wt.root)
        .args([
            "update-ref",
            &format!("refs/buildq/remote/{host}/{}", l.wt_id),
            &sha,
        ])
        .status();
    let exists = t.exec(
        None,
        &[],
        &s(&["test", "-e", &format!("{}/.git", l.wt_dir)]),
        true,
    );
    if matches!(exists, Ok((0, _))) {
        ok(
            t.exec(
                Some(&l.wt_dir),
                &[],
                &s(&["git", "checkout", "-q", "-f", "--detach", &sha]),
                true,
            ),
            "remote checkout",
        )?;
        ok(
            t.exec(Some(&l.wt_dir), &[], &s(&["git", "clean", "-fdq"]), true),
            "remote clean",
        )?;
    } else {
        ok(
            t.exec(
                None,
                &[],
                &s(&[
                    "git",
                    "--git-dir",
                    &l.repo_dir,
                    "worktree",
                    "add",
                    "-f",
                    "--detach",
                    &l.wt_dir,
                    &sha,
                ]),
                true,
            ),
            "remote worktree add",
        )?;
    }
    Ok(sha)
}

pub struct RemoteJob<'a> {
    pub host: &'a str,
    pub cfg: &'a HostConfig,
    pub argv: &'a [String],
    pub fetch: &'a [String],
    pub fetch_into: PathBuf,
}

/// Steps 2-6 against any transport (the queue slot is taken by the caller).
pub fn run_on(
    t: &dyn Transport,
    job: &RemoteJob,
    wt: &Worktree,
    here: &Path,
    scratch: &Path,
) -> Result<i32, String> {
    let root = job.cfg.root.clone().unwrap_or_else(|| ".buildq".into());
    let l = layout(&root, wt);
    let sha = sync(t, job.host, wt, &l, scratch)?;
    job::note(&format!(
        "{}: synced {} to {}",
        job.host,
        &sha[..12.min(sha.len())],
        l.wt_dir
    ));
    // Same subdirectory as locally.
    let sub = here
        .strip_prefix(crate::util::canonical(&wt.root))
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .filter(|p| !p.is_empty());
    let cwd = match sub {
        Some(sub) => format!("{}/{sub}", l.wt_dir),
        None => l.wt_dir.clone(),
    };
    let mut env: Vec<(String, String)> = job
        .cfg
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let argv: Vec<String> = match &job.cfg.buildq {
        Some(bq) => {
            if !job.fetch.is_empty() {
                // Fetched paths are relative to the worktree, so keep target/ there.
                env.push(("BUILDQ_POOL".into(), "0".into()));
            }
            [bq.clone(), "run".into(), "--".into()]
                .into_iter()
                .chain(job.argv.iter().cloned())
                .collect()
        }
        None => job.argv.to_vec(),
    };
    let (code, _) = t
        .exec(Some(&cwd), &env, &argv, false)
        .map_err(|e| format!("{}: {e}", job.host))?;
    for f in job.fetch {
        let remote = format!("{}/{}", l.wt_dir, f.trim_start_matches("./"));
        if let Err(e) = t.fetch(&remote, &job.fetch_into) {
            eprintln!("buildq: fetch {f} from {}: {e}", job.host);
        }
    }
    Ok(code)
}

pub fn transport(name: &str, h: &HostConfig) -> Ssh {
    Ssh {
        dest: h.ssh.clone().unwrap_or_else(|| name.to_string()),
        args: h.ssh_args.clone(),
    }
}

/// `buildq run --host <name|auto|a,b> -- cmd`
pub fn run(
    cfg: &Config,
    opts: &Opts,
    host_arg: &str,
    argv: &[String],
    fetch: &[String],
    fetch_into: Option<PathBuf>,
) -> Result<i32, String> {
    let here = crate::util::canonical(&std::env::current_dir().map_err(|e| e.to_string())?);
    let wt = git::discover(&here).ok_or("remote runs need a git worktree")?;
    let host = if host_arg == "auto" || host_arg.contains(',') {
        let cands: Vec<String> = if host_arg == "auto" {
            cfg.auto_hosts.clone().unwrap_or_else(|| {
                std::iter::once("local".to_string())
                    .chain(cfg.hosts.keys().cloned())
                    .collect()
            })
        } else {
            host_arg.split(',').map(|s| s.trim().to_string()).collect()
        };
        let h = pick_host(cfg, &cands, &wt.key());
        job::note(&format!("picked host {h}"));
        h
    } else {
        host_arg.to_string()
    };
    if host == "local" {
        return job::run_command(cfg, opts, argv);
    }
    let hc = cfg.hosts.get(&host).cloned().ok_or_else(|| {
        format!(
            "unknown host `{host}`: add [hosts.{host}] to {}",
            cfg.config_path.display()
        )
    })?;
    let host_opts = Opts {
        class: format!("host-{host}"),
        ..opts.clone()
    };
    let mut g = job::acquire(cfg, &host_opts, &display_cmd(argv))?;
    g.set_target(&host);
    remember_affinity(cfg, &wt.key(), &host);
    let t = transport(&host, &hc);
    let rj = RemoteJob {
        host: &host,
        cfg: &hc,
        argv,
        fetch,
        fetch_into: fetch_into
            .unwrap_or_else(|| wt.root.join("target").join("buildq-remote").join(&host)),
    };
    let code = run_on(&t, &rj, &wt, &here, &cfg.queue_dir.join("scratch"))?;
    crate::history::append(
        &cfg.queue_dir,
        &crate::history::Entry {
            pid: g.info.pid,
            class: g.info.class.clone(),
            cmd: g.info.cmd.clone(),
            cwd: g.info.cwd.clone(),
            enqueued_ms: g.info.enqueued_ms,
            started_ms: g.info.started_ms,
            ended_ms: crate::util::now_ms(),
            exit: code,
            target: Some(host.clone()),
            ..Default::default()
        },
    );
    Ok(code)
}

#[derive(Debug, Clone, PartialEq)]
pub struct HostLoad {
    pub name: String,
    pub slots: usize,
    pub busy: usize,
    pub waiters: usize,
}

/// Free slot first (the host this worktree used last wins ties, since its
/// remote checkout and caches are warm), then the shortest queue per slot.
pub fn choose(loads: &[HostLoad], affinity: Option<&str>) -> Option<String> {
    let free: Vec<&HostLoad> = loads
        .iter()
        .filter(|l| l.busy < l.slots && l.waiters == 0)
        .collect();
    if let Some(a) = affinity
        && let Some(l) = free.iter().find(|l| l.name == a)
    {
        return Some(l.name.clone());
    }
    if let Some(l) = free.iter().max_by(|a, b| {
        let fa = (a.slots - a.busy) as f64 / a.slots as f64;
        let fb = (b.slots - b.busy) as f64 / b.slots as f64;
        fa.total_cmp(&fb)
    }) {
        return Some(l.name.clone());
    }
    loads
        .iter()
        .min_by(|a, b| {
            let la = (a.busy + a.waiters) as f64 / a.slots as f64;
            let lb = (b.busy + b.waiters) as f64 / b.slots as f64;
            la.total_cmp(&lb)
        })
        .map(|l| l.name.clone())
}

pub fn pick_host(cfg: &Config, cands: &[String], wt_key: &str) -> String {
    let loads: Vec<HostLoad> = cands
        .iter()
        .map(|name| {
            let class = if name == "local" {
                "build".to_string()
            } else {
                format!("host-{name}")
            };
            let q = crate::queue::ClassQueue::new(
                &cfg.queue_dir,
                &class,
                cfg.class_slots(&class),
                std::time::Duration::from_millis(cfg.poll_ms),
            )
            .with_max_per_worktree(cfg.max_per_worktree);
            let st = q.status();
            HostLoad {
                name: name.clone(),
                slots: st.slots,
                busy: st.busy(),
                waiters: st.waiters.len(),
            }
        })
        .collect();
    let aff: BTreeMap<String, String> =
        read_json(&cfg.queue_dir.join("affinity.json")).unwrap_or_default();
    choose(&loads, aff.get(wt_key).map(String::as_str)).unwrap_or_else(|| "local".into())
}

fn remember_affinity(cfg: &Config, wt_key: &str, host: &str) {
    let p = cfg.queue_dir.join("affinity.json");
    let mut aff: BTreeMap<String, String> = read_json(&p).unwrap_or_default();
    aff.insert(wt_key.to_string(), host.to_string());
    let _ = write_json(&p, &aff);
}

/// `status --hosts`: each host's own `buildq status --json`, when configured.
pub fn host_status(cfg: &Config) -> Vec<(String, Result<serde_json::Value, String>)> {
    cfg.hosts
        .iter()
        .map(|(name, h)| {
            let r = match &h.buildq {
                None => Err("no remote buildq configured (local accounting only)".to_string()),
                Some(bq) => {
                    let t = transport(name, h);
                    match t.exec(
                        None,
                        &[],
                        &[bq.clone(), "status".into(), "--json".into()],
                        true,
                    ) {
                        Ok((0, out)) => serde_json::from_str(&out).map_err(|e| e.to_string()),
                        Ok((c, _)) => Err(format!("exit {c}")),
                        Err(e) => Err(e.to_string()),
                    }
                }
            };
            (name.clone(), r)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("cargo"), "cargo");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(
            remote_script(
                Some(".buildq/x"),
                &[("A".into(), "1 2".into())],
                &s(&["cargo", "test", "-p", "x y"])
            ),
            "cd .buildq/x && env A='1 2' cargo test -p 'x y'"
        );
    }

    #[test]
    fn host_choice() {
        let l = |n: &str, slots, busy, waiters| HostLoad {
            name: n.into(),
            slots,
            busy,
            waiters,
        };
        let loads = [l("local", 2, 2, 1), l("a", 4, 1, 0), l("b", 4, 3, 0)];
        assert_eq!(choose(&loads, None).as_deref(), Some("a"));
        assert_eq!(
            choose(&loads, Some("b")).as_deref(),
            Some("b"),
            "affinity wins while it has a free slot"
        );
        let full = [l("local", 2, 2, 3), l("a", 4, 4, 1), l("b", 8, 8, 1)];
        assert_eq!(choose(&full, Some("a")).as_deref(), Some("b"));
    }
}
