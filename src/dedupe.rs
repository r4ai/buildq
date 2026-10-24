//! Shares identical files between pool members as reflinks.
//!
//! Members of one pool build the same registry dependencies, so their
//! `deps/` directories often contain byte-identical artifacts under the same
//! relative path. Deduplication makes those paths share data blocks without
//! changing any path's content, length, mode, or mtime: Cargo fingerprints,
//! member manifests, and the pool safety invariant stay valid.
//!
//! - Linux: `FIDEDUPERANGE`; the kernel compares bytes and keeps the inode
//! - macOS: `clonefile` into a temporary file, then `rename` over the path
//! - elsewhere, or on a volume without reflinks: nothing changes
//!
//! Allocated block counts report every clone in full, so the cache keeps a
//! ledger (`dedupe.json`) of the paths a pass made share data. Usage
//! subtracts a group's copies beyond the first while each recorded file
//! identity still matches; a rewrite by Cargo drops it from the group.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{CacheAccounting, Config};
use crate::lock::{self, LockFile};
use crate::pool;
use crate::util::{self, human_bytes, read_json, write_json};

/// Release member locks at least this often so leases are not starved.
const BATCH_BYTES: u64 = 1 << 30;
const BATCH_MS: u128 = 1_000;
/// Temporary clones live in the member root until renamed into place.
const TMP_PREFIX: &str = ".buildq-dedupe-";
/// Linux shares data in place; macOS replaces the path with a new inode,
/// which would split a hard-link pair.
const KEEPS_INODE: bool = cfg!(target_os = "linux");
const SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "macos"));

#[derive(Debug, Clone)]
pub struct Options {
    pub dry_run: bool,
    /// Files below this length are not worth comparing.
    pub min_size: u64,
    /// Stop after reading this many bytes for comparisons.
    pub max_read: u64,
}

impl Options {
    pub fn new(cfg: &Config, dry_run: bool) -> Options {
        Options {
            dry_run,
            min_size: cfg.dedupe_min_size,
            max_read: cfg.dedupe_max_read,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub dry_run: bool,
    pub accounting: CacheAccounting,
    pub pools: usize,
    /// Files compared with the copy another member holds.
    pub compared: usize,
    pub identical: usize,
    /// Accounted bytes of identical copies beyond the first.
    pub identical_bytes: u64,
    /// Copies that now share data with the first copy (0 in a dry run).
    pub shared: usize,
    pub shared_bytes: u64,
    /// Copies an earlier pass already shared and that are unchanged since.
    pub already_shared: usize,
    pub already_shared_bytes: u64,
    pub differing: usize,
    /// Members (or member pairs) skipped because a job held them.
    pub skipped_busy: usize,
    /// Files whose size or identity changed between indexing and comparison.
    pub skipped_changed: usize,
    /// Hard-linked copies that replacing would split (macOS).
    pub skipped_hardlinked: usize,
    pub read_bytes: u64,
    /// The read limit stopped the pass before every candidate was compared.
    pub read_limited: bool,
    /// The platform or volume cannot share data; the pass stopped.
    pub unsupported: Option<String>,
    pub skipped: Option<String>,
    pub errors: Vec<String>,
}

impl Report {
    fn stopped(&self) -> bool {
        self.read_limited || self.unsupported.is_some()
    }
}

/// One recorded path and the file identity it had when it was shared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub pool: String,
    pub member: usize,
    pub rel: String,
    /// Other names of the inode in the member, such as Cargo's uplifted
    /// hard links: the blocks stay held while any name survives.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    pub dev: u64,
    pub ino: u64,
    pub len: u64,
    pub mtime_ns: u64,
}

/// Paths that share data blocks with each other.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Group {
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ledger {
    pub groups: Vec<Group>,
}

type Key = (String, usize, String);

impl Entry {
    fn of(c: &Candidate, md: &fs::Metadata) -> Option<Entry> {
        let (dev, ino) = file_id(md)?;
        Some(Entry {
            pool: c.pool.clone(),
            member: c.member,
            rel: c.rel.to_string_lossy().into_owned(),
            aliases: c
                .aliases
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
            dev,
            ino,
            len: md.len(),
            mtime_ns: mtime_ns(md),
        })
    }

    fn key(&self) -> Key {
        (self.pool.clone(), self.member, self.rel.clone())
    }

    /// The recorded path first, then its aliases.
    fn paths(&self, cache_dir: &Path) -> Vec<PathBuf> {
        let member = cache_dir
            .join("pools")
            .join(&self.pool)
            .join(format!("m{}", self.member));
        std::iter::once(&self.rel)
            .chain(&self.aliases)
            .map(|rel| member.join(rel))
            .collect()
    }

    fn matches(&self, md: &fs::Metadata) -> bool {
        md.is_file()
            && file_id(md) == Some((self.dev, self.ino))
            && md.len() == self.len
            && mtime_ns(md) == self.mtime_ns
    }

    /// The current metadata when any recorded name still holds the file.
    fn current(&self, cache_dir: &Path) -> Option<fs::Metadata> {
        self.current_excluding(cache_dir, &|_| false)
    }

    fn current_excluding(
        &self,
        cache_dir: &Path,
        excluded: &dyn Fn(&Path) -> bool,
    ) -> Option<fs::Metadata> {
        self.paths(cache_dir)
            .iter()
            .filter(|p| !excluded(p))
            .find_map(|p| fs::symlink_metadata(p).ok().filter(|md| self.matches(md)))
    }
}

impl Ledger {
    fn path(cache_dir: &Path) -> PathBuf {
        cache_dir.join("dedupe.json")
    }

    pub fn load(cache_dir: &Path) -> Ledger {
        read_json(&Self::path(cache_dir)).unwrap_or_default()
    }

    fn save(&self, cache_dir: &Path) -> io::Result<()> {
        write_json(&Self::path(cache_dir), self)
    }

    /// Group index of every recorded path.
    fn index(&self) -> BTreeMap<Key, (usize, &Entry)> {
        let mut map = BTreeMap::new();
        for (g, group) in self.groups.iter().enumerate() {
            for e in &group.entries {
                map.insert(e.key(), (g, e));
            }
        }
        map
    }

    /// Records that `other` now shares data with `canonical`.
    fn record(&mut self, canonical: Entry, other: Entry) {
        let (ck, ok) = (canonical.key(), other.key());
        for group in &mut self.groups {
            group.entries.retain(|e| e.key() != ok);
        }
        match self
            .groups
            .iter_mut()
            .find(|g| g.entries.iter().any(|e| e.key() == ck))
        {
            Some(group) => {
                for e in &mut group.entries {
                    if e.key() == ck {
                        *e = canonical.clone();
                    }
                }
                group.entries.push(other);
            }
            None => self.groups.push(Group {
                entries: vec![canonical, other],
            }),
        }
        self.groups.retain(|g| g.entries.len() > 1);
    }

    /// Drops entries whose path no longer holds the recorded file.
    fn retain_current(&mut self, cache_dir: &Path) {
        for group in &mut self.groups {
            group.entries.retain(|e| e.current(cache_dir).is_some());
        }
        self.groups.retain(|g| g.entries.len() > 1);
    }
}

/// Recorded sharing that still holds, for budget accounting.
#[derive(Debug, Clone, Default)]
pub struct Sharing {
    groups: Vec<Shared>,
}

#[derive(Debug, Clone)]
struct Shared {
    /// Accounted bytes of one copy.
    bytes: u64,
    holders: Vec<Holder>,
}

#[derive(Debug, Clone)]
struct Holder {
    pool: String,
    member: usize,
    /// Every recorded name of the holder's inode; the first is its key.
    paths: Vec<PathBuf>,
}

impl Holder {
    fn is(&self, pool: &str, id: usize) -> bool {
        self.pool == pool && self.member == id
    }
}

impl Sharing {
    /// Validates the ledger against the filesystem.
    pub fn load(cfg: &Config) -> Sharing {
        Self::load_excluding(cfg, &|_| false)
    }

    /// Like `load`, treating paths for which `excluded` holds as gone, as a
    /// dry run does for the paths it plans to remove.
    ///
    /// A file identity counts in one group at most: Cargo hard-links uplifted
    /// outputs, so two recorded paths can name one inode, and member sizes
    /// already count such an inode once. Dropping a repeat only overstates
    /// usage, never understates it.
    pub fn load_excluding(cfg: &Config, excluded: &dyn Fn(&Path) -> bool) -> Sharing {
        let ledger = Ledger::load(&cfg.cache_dir);
        let mut seen = BTreeSet::new();
        let mut groups = Vec::new();
        for group in &ledger.groups {
            let mut bytes = 0;
            let mut holders: Vec<Holder> = Vec::new();
            for e in &group.entries {
                let Some(md) = e.current_excluding(&cfg.cache_dir, excluded) else {
                    continue;
                };
                if !seen.insert((e.dev, e.ino)) || holders.iter().any(|h| h.is(&e.pool, e.member)) {
                    continue;
                }
                bytes = footprint(&md, cfg.accounting);
                holders.push(Holder {
                    pool: e.pool.clone(),
                    member: e.member,
                    paths: e.paths(&cfg.cache_dir),
                });
            }
            if holders.len() > 1 {
                groups.push(Shared { bytes, holders });
            }
        }
        Sharing { groups }
    }

    /// Bytes to subtract from a pool's member sizes: every copy beyond a
    /// group's first is charged to the group's first holder instead.
    pub fn pool_discount(&self, pool: &str) -> u64 {
        self.groups
            .iter()
            .map(|g| g.bytes * g.holders[1..].iter().filter(|h| h.pool == pool).count() as u64)
            .sum()
    }

    /// Bytes of a member that at least one other member also holds.
    pub fn member_shared(&self, pool: &str, id: usize) -> u64 {
        self.exclusive_discount(pool, id, &BTreeSet::new())
    }

    /// Bytes that removing the member would not free while the members in
    /// `removed` are already gone.
    pub fn exclusive_discount(
        &self,
        pool: &str,
        id: usize,
        removed: &BTreeSet<(String, usize)>,
    ) -> u64 {
        self.groups
            .iter()
            .filter(|g| {
                g.holders.iter().any(|h| h.is(pool, id))
                    && g.holders
                        .iter()
                        .any(|h| !h.is(pool, id) && !removed.contains(&(h.pool.clone(), h.member)))
            })
            .map(|g| g.bytes)
            .sum()
    }

    /// Bytes that pruning `removed` paths of one member did not free, because
    /// another holder keeps the blocks. A holder is pruned only when all its
    /// names are; a surviving alias keeps its blocks, which the member's own
    /// measurement already excludes. `gone` collects pruned holders across
    /// members, so the last holder's removal is credited in full.
    pub fn pruned_discount(
        &self,
        pool: &str,
        id: usize,
        removed: &dyn Fn(&Path) -> bool,
        gone: &mut BTreeSet<PathBuf>,
    ) -> u64 {
        let mut kept = 0;
        for g in &self.groups {
            let Some(mine) = g.holders.iter().find(|h| h.is(pool, id)) else {
                continue;
            };
            if !mine.paths.iter().all(|p| removed(p)) {
                continue;
            }
            gone.insert(mine.paths[0].clone());
            if g.holders.iter().any(|h| !gone.contains(&h.paths[0])) {
                kept += g.bytes;
            }
        }
        kept
    }
}

/// One regular file of an idle member, as indexed without its lock.
#[derive(Debug, Clone)]
struct Candidate {
    pool: String,
    member: usize,
    member_dir: PathBuf,
    rel: PathBuf,
    /// Other names of the same inode in the member.
    aliases: Vec<PathBuf>,
    path: PathBuf,
    md: fs::Metadata,
}

impl Candidate {
    fn member_key(&self) -> (&str, usize) {
        (&self.pool, self.member)
    }

    fn key(&self) -> Key {
        (
            self.pool.clone(),
            self.member,
            self.rel.to_string_lossy().into_owned(),
        )
    }
}

/// A copy to share with the group's first (canonical) copy.
struct Work<'a> {
    canonical: &'a Candidate,
    other: &'a Candidate,
}

impl Work<'_> {
    fn pair(&self) -> ((&str, usize), (&str, usize)) {
        (self.canonical.member_key(), self.other.member_key())
    }
}

/// Copies that may share data: one toolchain, one volume, one relative
/// path and length. Pools of different repositories on the same toolchain
/// build registry dependencies to the same unit paths, so they group too.
type GroupKey = (String, u64, PathBuf, u64);

pub fn run(cfg: &Config, opts: &Options) -> Report {
    let mut report = Report {
        dry_run: opts.dry_run,
        accounting: cfg.accounting,
        ..Default::default()
    };
    let _ = fs::create_dir_all(&cfg.cache_dir);
    let Ok(Some(_lock)) = lock::try_lock(&cfg.cache_dir.join("dedupe.lock")) else {
        report.skipped = Some("another deduplication pass is running".into());
        return report;
    };
    if !opts.dry_run && !SUPPORTED {
        report.unsupported = Some("buildq shares files only on Linux and macOS".into());
        return report;
    }
    let mut ledger = Ledger::load(&cfg.cache_dir);
    let groups = index(cfg, opts, &mut report);
    let work = plan(&groups, &ledger, &mut report);
    let mut start = 0;
    while start < work.len() && !report.stopped() {
        let end = start
            + work[start..]
                .iter()
                .take_while(|w| w.pair() == work[start].pair())
                .count();
        share_pair(&work[start..end], opts, &mut ledger, &mut report);
        start = end;
    }
    if !opts.dry_run {
        ledger.retain_current(&cfg.cache_dir);
        if let Err(e) = ledger.save(&cfg.cache_dir) {
            report.errors.push(format!("write ledger: {e}"));
        }
    }
    report
}

/// Picks each group's canonical copy and the copies still to share with it,
/// ordered by member pair so one pair's locks cover a run of work.
fn plan<'a>(
    groups: &'a BTreeMap<GroupKey, Vec<Candidate>>,
    ledger: &Ledger,
    report: &mut Report,
) -> Vec<Work<'a>> {
    let recorded = ledger.index();
    // The ledger group a candidate still belongs to.
    let group_of = |c: &Candidate| {
        recorded
            .get(&c.key())
            .filter(|(_, e)| e.matches(&c.md))
            .map(|(g, _)| *g)
    };
    let mut work: Vec<Work> = Vec::new();
    for copies in groups.values().filter(|c| c.len() > 1) {
        // Keep sharing with the copy an earlier pass chose, if still intact.
        let canonical = copies
            .iter()
            .find(|c| group_of(c).is_some())
            .unwrap_or(&copies[0]);
        let group = group_of(canonical);
        for other in copies
            .iter()
            .filter(|c| c.member_key() != canonical.member_key())
        {
            if group.is_some() && group_of(other) == group {
                report.already_shared += 1;
                report.already_shared_bytes += footprint(&other.md, report.accounting);
            } else if !KEEPS_INODE && nlink(&other.md) > 1 {
                report.skipped_hardlinked += 1;
            } else {
                work.push(Work { canonical, other });
            }
        }
    }
    work.sort_by(|a, b| a.pair().cmp(&b.pair()));
    work
}

/// Indexes eligible files of idle members in every pool. Pools and members
/// are visited in order, so a group's first copy is the lowest (pool, id).
fn index(cfg: &Config, opts: &Options, report: &mut Report) -> BTreeMap<GroupKey, Vec<Candidate>> {
    let mut groups: BTreeMap<GroupKey, Vec<Candidate>> = BTreeMap::new();
    let mut pools: Vec<PathBuf> = fs::read_dir(cfg.cache_dir.join("pools"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    pools.sort();
    for pool_dir in pools {
        report.pools += 1;
        let name = pool_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Without a recorded toolchain, a pool only shares within itself.
        let scope = read_json::<pool::PoolInfo>(&pool_dir.join("pool.json"))
            .map(|i| i.toolchain_id)
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| format!("pool:{name}"));
        for id in pool::member_ids(&pool_dir) {
            let dir = pool_dir.join(format!("m{id}"));
            if !dir.is_dir() {
                continue;
            }
            // Indexing does not hold the lock; sharing rechecks each file.
            let Ok(Some(lock)) = lock::try_lock(&dir.with_extension("lock")) else {
                report.skipped_busy += 1;
                continue;
            };
            if !opts.dry_run {
                remove_leftovers(&dir);
            }
            drop(lock);
            util::walk(&dir, &mut |path, md| {
                if md.len() < opts.min_size {
                    return;
                }
                let Ok(rel) = path.strip_prefix(&dir) else {
                    return;
                };
                if !eligible(rel) {
                    return;
                }
                let dev = file_id(md).map_or(0, |(dev, _)| dev);
                groups
                    .entry((scope.clone(), dev, rel.to_path_buf(), md.len()))
                    .or_default()
                    .push(Candidate {
                        pool: name.clone(),
                        member: id,
                        member_dir: dir.clone(),
                        rel: rel.to_path_buf(),
                        aliases: Vec::new(),
                        path: path.to_path_buf(),
                        md: md.clone(),
                    });
            });
        }
    }
    link_aliases(&mut groups);
    groups
}

/// Fills in the other names each hard-linked candidate has in its member.
fn link_aliases(groups: &mut BTreeMap<GroupKey, Vec<Candidate>>) {
    let mut names: BTreeMap<(String, usize, (u64, u64)), Vec<PathBuf>> = BTreeMap::new();
    for c in groups.values().flatten() {
        if let Some(id) = file_id(&c.md).filter(|_| nlink(&c.md) > 1) {
            names
                .entry((c.pool.clone(), c.member, id))
                .or_default()
                .push(c.rel.clone());
        }
    }
    for c in groups.values_mut().flatten() {
        if let Some(id) = file_id(&c.md).filter(|_| nlink(&c.md) > 1)
            && let Some(all) = names.get(&(c.pool.clone(), c.member, id))
        {
            c.aliases = all.iter().filter(|r| **r != c.rel).cloned().collect();
        }
    }
}

/// Temporary clones of an interrupted pass. Called under the member lock.
fn remove_leftovers(member_dir: &Path) {
    for e in fs::read_dir(member_dir).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().starts_with(TMP_PREFIX) {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// Build outputs only: rustc owns incremental sessions and Cargo owns
/// fingerprints, both of which are small or private to one member.
fn eligible(rel: &Path) -> bool {
    !rel.components().any(|c| match c {
        Component::Normal(name) => {
            name == "incremental"
                || name == ".fingerprint"
                || name.to_string_lossy().starts_with(TMP_PREFIX)
        }
        _ => true,
    })
}

/// Both member locks for one batch of comparisons.
struct PairLock {
    _locks: (LockFile, LockFile),
    started: std::time::Instant,
    bytes: u64,
}

impl PairLock {
    fn take(w: &Work) -> Option<PairLock> {
        let first = lock::try_lock(&w.canonical.member_dir.with_extension("lock")).ok()??;
        let second = lock::try_lock(&w.other.member_dir.with_extension("lock")).ok()??;
        Some(PairLock {
            _locks: (first, second),
            started: std::time::Instant::now(),
            bytes: 0,
        })
    }

    fn expired(&self) -> bool {
        self.bytes >= BATCH_BYTES || self.started.elapsed().as_millis() >= BATCH_MS
    }
}

/// Shares a run of work items that all name the same two members.
fn share_pair(work: &[Work], opts: &Options, ledger: &mut Ledger, report: &mut Report) {
    let mut held: Option<PairLock> = None;
    for (n, w) in work.iter().enumerate() {
        let len = w.other.md.len();
        if report.read_bytes.saturating_add(2 * len) > opts.max_read {
            report.read_limited = true;
            return;
        }
        if held.as_ref().is_none_or(PairLock::expired) {
            // Release first: a second handle cannot take a lock this process holds.
            drop(held.take());
            held = PairLock::take(w);
            if held.is_none() {
                report.skipped_busy += 1;
                return;
            }
        }
        let pair_lock = held.as_mut().unwrap();
        if !unchanged(w.canonical) || !unchanged(w.other) {
            report.skipped_changed += 1;
            continue;
        }
        report.compared += 1;
        report.read_bytes += 2 * len;
        pair_lock.bytes += 2 * len;
        let bytes = footprint(&w.other.md, report.accounting);
        // The Linux kernel compares while it shares; elsewhere compare first.
        if opts.dry_run || !KEEPS_INODE {
            match same_bytes(&w.canonical.path, &w.other.path, len) {
                Ok(true) => {}
                Ok(false) => {
                    report.differing += 1;
                    continue;
                }
                Err(e) => {
                    report
                        .errors
                        .push(format!("compare {}: {e}", w.other.path.display()));
                    continue;
                }
            }
            if opts.dry_run {
                report.identical += 1;
                report.identical_bytes += bytes;
                continue;
            }
        }
        let tmp = w
            .other
            .member_dir
            .join(format!("{TMP_PREFIX}{}-{n}", std::process::id()));
        match platform::share(&w.canonical.path, &w.other.path, &tmp) {
            Ok(Outcome::Shared) => {
                report.identical += 1;
                report.identical_bytes += bytes;
                report.shared += 1;
                report.shared_bytes += bytes;
                let entry = |c: &Candidate| {
                    fs::symlink_metadata(&c.path)
                        .ok()
                        .and_then(|md| Entry::of(c, &md))
                };
                if let (Some(canonical), Some(other)) = (entry(w.canonical), entry(w.other)) {
                    ledger.record(canonical, other);
                }
            }
            Ok(Outcome::Differs) => report.differing += 1,
            Err(Failure::Unsupported(why)) => {
                report.unsupported = Some(why);
                return;
            }
            Err(Failure::Io(e)) => report
                .errors
                .push(format!("share {}: {e}", w.other.path.display())),
        }
    }
}

/// True when the path still names the file that was indexed.
fn unchanged(c: &Candidate) -> bool {
    fs::symlink_metadata(&c.path).is_ok_and(|md| {
        md.is_file()
            && md.len() == c.md.len()
            && mtime_ns(&md) == mtime_ns(&c.md)
            && file_id(&md) == file_id(&c.md)
    })
}

fn mtime_ns(md: &fs::Metadata) -> u64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn file_id(md: &fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((md.dev(), md.ino()))
}

#[cfg(not(unix))]
fn file_id(_: &fs::Metadata) -> Option<(u64, u64)> {
    None
}

#[cfg(unix)]
fn nlink(md: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.nlink()
}

#[cfg(not(unix))]
fn nlink(_: &fs::Metadata) -> u64 {
    1
}

fn footprint(md: &fs::Metadata, mode: CacheAccounting) -> u64 {
    match mode {
        CacheAccounting::Logical => md.len(),
        CacheAccounting::Allocated => allocated(md),
    }
}

#[cfg(unix)]
fn allocated(md: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn allocated(md: &fs::Metadata) -> u64 {
    md.len()
}

fn same_bytes(a: &Path, b: &Path, len: u64) -> io::Result<bool> {
    let (mut fa, mut fb) = (File::open(a)?, File::open(b)?);
    let mut ba = vec![0u8; 1 << 20];
    let mut bb = vec![0u8; 1 << 20];
    let mut left = len;
    while left > 0 {
        let n = left.min(ba.len() as u64) as usize;
        fa.read_exact(&mut ba[..n])?;
        fb.read_exact(&mut bb[..n])?;
        if ba[..n] != bb[..n] {
            return Ok(false);
        }
        left -= n as u64;
    }
    Ok(true)
}

/// Only the Linux kernel reports `Differs`; elsewhere the caller compares,
/// and unsupported platforms only ever fail.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum Outcome {
    Shared,
    Differs,
}

#[derive(Debug)]
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
enum Failure {
    /// The platform or volume cannot share data blocks.
    Unsupported(String),
    Io(io::Error),
}

#[cfg(target_os = "linux")]
mod platform {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::path::Path;

    use super::{Failure, Outcome};

    #[repr(C)]
    struct Range {
        src_offset: u64,
        src_length: u64,
        dest_count: u16,
        reserved1: u16,
        reserved2: u32,
        info: Info,
    }

    #[repr(C)]
    struct Info {
        dest_fd: i64,
        dest_offset: u64,
        bytes_deduped: u64,
        status: i32,
        reserved: u32,
    }

    /// `_IOWR(0x94, 54, struct file_dedupe_range)`; the size excludes `info`.
    const FIDEDUPERANGE: libc::Ioctl = 0xC018_9436u32 as libc::Ioctl;
    const FILE_DEDUPE_RANGE_DIFFERS: i32 = 1;
    /// btrfs caps one request at 16 MiB.
    const CHUNK: u64 = 16 << 20;

    fn classify(e: io::Error) -> Failure {
        match e.raw_os_error() {
            Some(libc::EOPNOTSUPP | libc::ENOTTY | libc::EXDEV | libc::EINVAL) => {
                Failure::Unsupported(format!("FIDEDUPERANGE: {e}"))
            }
            _ => Failure::Io(e),
        }
    }

    pub(super) fn share(src: &Path, dst: &Path, _tmp: &Path) -> Result<Outcome, Failure> {
        let s = File::open(src).map_err(Failure::Io)?;
        // Owners may dedupe into a read-only descriptor; avoid write opens,
        // which a still-running test executable would refuse.
        match dedupe(&s, &File::open(dst).map_err(Failure::Io)?) {
            Err(Failure::Io(e)) if e.raw_os_error() == Some(libc::EPERM) => {
                let d = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(dst)
                    .map_err(Failure::Io)?;
                dedupe(&s, &d)
            }
            r => r,
        }
    }

    fn dedupe(s: &File, d: &File) -> Result<Outcome, Failure> {
        let before = d.metadata().map_err(Failure::Io)?;
        let len = before.len();
        let mut offset = 0;
        while offset < len {
            let mut range = Range {
                src_offset: offset,
                src_length: (len - offset).min(CHUNK),
                dest_count: 1,
                reserved1: 0,
                reserved2: 0,
                info: Info {
                    dest_fd: i64::from(d.as_raw_fd()),
                    dest_offset: offset,
                    bytes_deduped: 0,
                    status: 0,
                    reserved: 0,
                },
            };
            if unsafe { libc::ioctl(s.as_raw_fd(), FIDEDUPERANGE, &mut range) } < 0 {
                return Err(classify(io::Error::last_os_error()));
            }
            match range.info.status {
                FILE_DEDUPE_RANGE_DIFFERS => return Ok(Outcome::Differs),
                status if status < 0 => {
                    return Err(classify(io::Error::from_raw_os_error(-status)));
                }
                _ => {}
            }
            if range.info.bytes_deduped == 0 {
                // Some filesystems leave an unaligned tail block unshared.
                if offset == 0 {
                    return Err(Failure::Io(io::Error::other("kernel shared no bytes")));
                }
                break;
            }
            offset += range.info.bytes_deduped;
        }
        // Deduplication is not a write, but never let it look like one to Cargo.
        let modified = before.modified().map_err(Failure::Io)?;
        if d.metadata().and_then(|m| m.modified()).ok() != Some(modified) {
            d.set_modified(modified).map_err(Failure::Io)?;
        }
        Ok(Outcome::Shared)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::CString;
    use std::fs::{self, File, FileTimes};
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use super::{Failure, Outcome};

    const CLONE_NOFOLLOW: u32 = 0x0001;
    const CLONE_NOOWNERCOPY: u32 = 0x0002;

    fn c_path(p: &Path) -> Result<CString, Failure> {
        CString::new(p.as_os_str().as_bytes()).map_err(|e| Failure::Io(io::Error::other(e)))
    }

    /// The caller compared both files and holds both member locks.
    pub(super) fn share(src: &Path, dst: &Path, tmp: &Path) -> Result<Outcome, Failure> {
        let md = fs::symlink_metadata(dst).map_err(Failure::Io)?;
        let (src_c, tmp_c) = (c_path(src)?, c_path(tmp)?);
        let flags = CLONE_NOFOLLOW | CLONE_NOOWNERCOPY;
        if unsafe { libc::clonefile(src_c.as_ptr(), tmp_c.as_ptr(), flags) } != 0 {
            let e = io::Error::last_os_error();
            return Err(match e.raw_os_error() {
                Some(libc::ENOTSUP | libc::EOPNOTSUPP | libc::EXDEV) => {
                    Failure::Unsupported(format!("clonefile: {e}"))
                }
                _ => Failure::Io(e),
            });
        }
        // The clone carries the source's metadata; give it the destination's.
        let place = || -> io::Result<()> {
            fs::set_permissions(tmp, md.permissions())?;
            let times = FileTimes::new()
                .set_accessed(md.accessed()?)
                .set_modified(md.modified()?);
            File::open(tmp)?.set_times(times)?;
            fs::rename(tmp, dst)
        };
        place().map_err(|e| {
            let _ = fs::remove_file(tmp);
            Failure::Io(e)
        })?;
        Ok(Outcome::Shared)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use std::path::Path;

    use super::{Failure, Outcome};

    pub(super) fn share(_: &Path, _: &Path, _: &Path) -> Result<Outcome, Failure> {
        Err(Failure::Unsupported(
            "buildq shares files only on Linux and macOS".into(),
        ))
    }
}

pub fn render(r: &Report) -> String {
    let mut s = String::new();
    if let Some(skipped) = &r.skipped {
        s.push_str(&format!("skipped: {skipped}\n"));
        return s;
    }
    s.push_str(&format!(
        "compared {} files in {} pools, read {} [{} accounting]\n",
        r.compared,
        r.pools,
        human_bytes(r.read_bytes),
        r.accounting.name()
    ));
    if r.dry_run {
        s.push_str(&format!(
            "identical: {} files, would share {}\n",
            r.identical,
            human_bytes(r.identical_bytes)
        ));
    } else {
        s.push_str(&format!(
            "shared: {} files, {}\n",
            r.shared,
            human_bytes(r.shared_bytes)
        ));
    }
    s.push_str(&format!(
        "already shared: {} files, {}\n",
        r.already_shared,
        human_bytes(r.already_shared_bytes)
    ));
    s.push_str(&format!(
        "differing: {} files; skipped: {} busy, {} changed, {} hard-linked\n",
        r.differing, r.skipped_busy, r.skipped_changed, r.skipped_hardlinked
    ));
    if r.read_limited {
        s.push_str("stopped at the read limit; shared files are skipped on the next pass\n");
    }
    if let Some(why) = &r.unsupported {
        s.push_str(&format!("unsupported: {why}\n"));
    }
    for e in &r.errors {
        s.push_str(&format!("error: {e}\n"));
    }
    s
}
