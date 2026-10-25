//! Shared target-dir pool.
//!
//! One pool per (repository, toolchain). All worktrees of a repository share
//! its pool; each pool has a few *members* (plain cargo target dirs) and a job
//! leases one member exclusively for its duration.
//!
//! ## Why sharing is safe here
//!
//! Cargo names artifacts with a hash that, for workspace members, uses the
//! package path *relative to the workspace root*. Two worktrees therefore map
//! the same crate to the same artifact files in a shared target dir, which is
//! what makes reuse possible, and also what makes naive sharing unsafe:
//! cargo decides freshness by comparing source mtimes against the artifact,
//! so worktree B can silently reuse an artifact built from worktree A's
//! different source if B's file happens to be older.
//!
//! buildq closes that hole with a *source manifest* per member
//! (path -> git blob id of the content the member's artifacts were last
//! known to reflect). Before a job runs in a member, the current worktree's
//! manifest is compared with the member's; for every package that owns a
//! differing file, the package's fingerprints in the member are deleted, so
//! cargo must rebuild that package (and, through it, its dependants) no matter
//! which worktree builds next or how old its files are. The manifest is then
//! updated to the current worktree. When the source root changes (or the
//! previous root is unknown), all local packages are invalidated even if their
//! content matches: compile-time paths such as CARGO_MANIFEST_DIR must refer
//! to the current worktree. Registry and git dependencies remain cached.
//! Invariant: for every package, either its
//! fingerprints are absent or its artifacts were built from manifest content.
//! Differing files outside any package (e.g. files pulled in with
//! `include_str!("../../x")`) are touched in the current worktree instead.
//!
//! ## Picking a member
//!
//! Free members are scored by the number of packages that would have to be
//! rebuilt, including transitive workspace dependents (then by differing
//! files, worktree affinity, and recency). A new member is only created when every member is
//! busy and the pool is below `max_members` (and the disk budget allows it).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::git::Worktree;
use crate::lock::{self, LockFile};
use crate::util::{now_ms, read_json, short_hash, write_atomic, write_json};

/// path -> content id
pub type Manifest = BTreeMap<String, String>;

/// Reverse workspace dependencies from `cargo metadata --no-deps`.
/// Optional, build, dev, and target-specific edges are included conservatively.
#[derive(Debug, Default)]
pub struct RebuildGraph {
    dependents: BTreeMap<String, BTreeSet<String>>,
    /// Every workspace member, whether or not a manifest file maps to it.
    members: BTreeSet<String>,
}

impl RebuildGraph {
    pub fn from_metadata(meta: &serde_json::Value) -> Option<Self> {
        let members = meta["workspace_members"].as_array()?;
        let packages: Vec<_> = meta["packages"]
            .as_array()?
            .iter()
            .filter(|p| members.contains(&p["id"]))
            .collect();
        let mut names_by_root = BTreeMap::new();
        for package in &packages {
            let manifest = Path::new(package["manifest_path"].as_str()?);
            names_by_root.insert(
                crate::util::canonical(manifest.parent()?),
                package["name"].as_str()?,
            );
        }
        let mut graph = Self::default();
        for package in &packages {
            let name = package["name"].as_str()?;
            graph.members.insert(name.into());
            for dep in package["dependencies"].as_array()? {
                let Some(path) = dep["path"].as_str() else {
                    continue;
                };
                // Match paths, not the dependency's (possibly renamed) key or
                // a registry package with the same name.
                let path = crate::util::canonical(Path::new(path));
                if let Some(dependency) = names_by_root.get(&path) {
                    graph
                        .dependents
                        .entry((*dependency).into())
                        .or_default()
                        .insert(name.into());
                }
            }
        }
        Some(graph)
    }

    /// Count the union, so shared dependents and cycles are counted once.
    /// Unknown/deleted packages still cost one; unavailable metadata reduces
    /// to the original raw invalidation count.
    pub fn cost(&self, invalidated: &BTreeSet<String>) -> usize {
        let mut affected = invalidated.clone();
        let mut pending: Vec<_> = invalidated.iter().cloned().collect();
        while let Some(package) = pending.pop() {
            if let Some(dependents) = self.dependents.get(&package) {
                for dependent in dependents {
                    if affected.insert(dependent.clone()) {
                        pending.push(dependent.clone());
                    }
                }
            }
        }
        affected.len()
    }

    pub fn members(&self) -> &BTreeSet<String> {
        &self.members
    }
}

/// Path dependencies of workspace members whose sources the manifest does
/// not cover (outside the worktree, or ignored by Git): pooling cannot
/// invalidate them by content. Relative to `root` where possible.
pub fn uncovered_path_deps(
    meta: &serde_json::Value,
    root: &Path,
    manifest: &Manifest,
) -> Vec<PathBuf> {
    let root = crate::util::canonical(root);
    let members = meta["workspace_members"].as_array();
    let mut out = BTreeSet::new();
    for package in meta["packages"].as_array().into_iter().flatten() {
        if !members.is_some_and(|m| m.contains(&package["id"])) {
            continue;
        }
        for dep in package["dependencies"].as_array().into_iter().flatten() {
            let Some(path) = dep["path"].as_str() else {
                continue;
            };
            let path = crate::util::canonical(Path::new(path));
            let covered = path.strip_prefix(&root).is_ok_and(|rel| {
                let rel = rel.to_string_lossy().replace('\\', "/");
                let key = if rel.is_empty() {
                    "Cargo.toml".to_string()
                } else {
                    format!("{rel}/Cargo.toml")
                };
                manifest.contains_key(&key)
            });
            if !covered {
                out.insert(pathdiff(&path, &root));
            }
        }
    }
    out.into_iter().collect()
}

/// `path` relative to `base` when they share a prefix, else `path` itself.
fn pathdiff(path: &Path, base: &Path) -> PathBuf {
    let common = path
        .components()
        .zip(base.components())
        .take_while(|(a, b)| a == b)
        .count();
    if common <= 1 {
        return path.to_path_buf();
    }
    let ups = base.components().count() - common;
    std::iter::repeat_n(Path::new(".."), ups)
        .map(Path::to_path_buf)
        .chain(
            path.components()
                .skip(common)
                .map(|c| PathBuf::from(c.as_os_str())),
        )
        .collect()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PoolInfo {
    pub repo: String,
    pub repo_key: String,
    pub toolchain: String,
    pub toolchain_id: String,
    pub created_ms: u64,
    pub last_used_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MemberMeta {
    pub id: usize,
    pub created_ms: u64,
    pub last_used_ms: u64,
    pub last_worktree: Option<String>,
    /// Root for which all local fingerprints are valid (or absent).
    /// Separate from last_worktree: older versions could record a new worktree
    /// without rebuilding its compiled paths. Missing means invalidate once.
    pub source_root: Option<String>,
    pub last_head: Option<String>,
    pub last_cmd: String,
    pub jobs: u64,
    /// Last measured size; cheap scans reuse it briefly across builds.
    pub size_bytes: u64,
    /// Measurement time, independent of last_used_ms (a build need not rescan).
    pub size_ms: u64,
    /// Basis of the cached estimate; missing legacy estimates are refreshed.
    pub size_accounting: Option<crate::config::CacheAccounting>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub changed: Vec<String>,
    /// Package names whose fingerprints must go.
    pub packages: BTreeSet<String>,
    /// Changed files that belong to no package.
    pub outside: Vec<String>,
}

pub struct Pool {
    pub dir: PathBuf,
    /// Seed new members from a copy-on-write clone of the closest built one.
    pub seed: bool,
}

pub struct Lease {
    pub id: usize,
    pub target_dir: PathBuf,
    pub meta_path: PathBuf,
    pub meta: MemberMeta,
    pub plan: Plan,
    pub fresh_member: bool,
    /// The member a new one was cloned from, if seeding succeeded.
    pub seeded_from: Option<usize>,
    _lock: LockFile,
}

impl Lease {
    pub fn name(&self) -> String {
        format!("m{}", self.id)
    }

    /// Records the end of the job.
    pub fn finish(&mut self) {
        self.meta.last_used_ms = now_ms();
        let _ = write_json(&self.meta_path, &self.meta);
    }
}

/// `<cache>/pools/<repo, at most 16 chars>-<hash of repo + toolchain>`.
/// Kept short on purpose: cargo nests deep paths below a target dir and
/// Windows tools still trip over long ones. Details live in `pool.json`.
pub fn pool_dir(cache: &Path, wt: &Worktree, toolchain_id: &str) -> PathBuf {
    let repo: String = wt
        .repo_name()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(16)
        .collect();
    let key = format!("{}\n{}", wt.repo_key(), toolchain_id);
    cache
        .join("pools")
        .join(format!("{repo}-{}", short_hash(&key)))
}

pub fn read_manifest(path: &Path) -> Manifest {
    let mut m = Manifest::new();
    let Ok(f) = fs::File::open(path) else {
        return m;
    };
    for line in io::BufReader::new(f).lines().map_while(Result::ok) {
        if let Some((id, p)) = line.split_once(' ') {
            m.insert(p.to_string(), id.to_string());
        }
    }
    m
}

pub fn write_manifest(path: &Path, m: &Manifest) -> io::Result<()> {
    let mut buf = Vec::with_capacity(m.len() * 64);
    for (p, id) in m {
        writeln!(buf, "{id} {p}")?;
    }
    write_atomic(path, &buf)
}

/// Compares two manifests and groups the differences by owning package.
pub fn plan(
    member: &Manifest,
    current: &Manifest,
    owner: &mut dyn FnMut(&str) -> Option<String>,
) -> Plan {
    let mut changed: Vec<String> = Vec::new();
    for (p, id) in current {
        if member.get(p) != Some(id) {
            changed.push(p.clone());
        }
    }
    for p in member.keys() {
        if !current.contains_key(p) {
            changed.push(p.clone());
        }
    }
    changed.sort();
    let mut plan = Plan::default();
    for p in &changed {
        match owner(p) {
            Some(pkg) => {
                plan.packages.insert(pkg);
            }
            // Without a previous manifest Cargo must build everything anyway.
            // Keep package invalidation, but do not wake checkout file watchers.
            None if !member.is_empty() => plan.outside.push(p.clone()),
            None => {}
        }
    }
    plan.changed = changed;
    plan
}

/// Maps a repo-relative file to the package owning it: the nearest ancestor
/// directory whose Cargo.toml has a `[package]`. Results are cached per dir.
pub struct PackageOwners {
    roots: Vec<PathBuf>,
    cache: BTreeMap<String, Option<String>>,
}

impl PackageOwners {
    /// `roots`: worktrees to look in, first match wins (the current one first;
    /// the previous builder's helps with deleted packages).
    pub fn new(roots: Vec<PathBuf>) -> PackageOwners {
        PackageOwners {
            roots,
            cache: BTreeMap::new(),
        }
    }

    pub fn owner(&mut self, rel: &str) -> Option<String> {
        let mut dir = rel.rsplit_once('/').map(|(d, _)| d.to_string());
        // A Cargo.toml itself belongs to its own package.
        if rel == "Cargo.toml" || rel.ends_with("/Cargo.toml") {
            dir = Some(
                rel.trim_end_matches("Cargo.toml")
                    .trim_end_matches('/')
                    .to_string(),
            );
        }
        let mut chain = Vec::new();
        let mut cur = dir.unwrap_or_default();
        let found = loop {
            if let Some(hit) = self.cache.get(&cur) {
                break hit.clone();
            }
            chain.push(cur.clone());
            if let Some(name) = self.package_in(&cur) {
                break Some(name);
            }
            if cur.is_empty() {
                break None;
            }
            cur = cur
                .rsplit_once('/')
                .map(|(d, _)| d.to_string())
                .unwrap_or_default();
        };
        for d in chain {
            self.cache.insert(d, found.clone());
        }
        found
    }

    fn package_in(&self, dir: &str) -> Option<String> {
        for root in &self.roots {
            let manifest = if dir.is_empty() {
                root.join("Cargo.toml")
            } else {
                root.join(dir).join("Cargo.toml")
            };
            let Ok(text) = fs::read_to_string(&manifest) else {
                continue;
            };
            let Ok(value) = text.parse::<toml::Table>() else {
                continue;
            };
            return value
                .get("package")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
                .map(str::to_string);
        }
        None
    }
}

/// Deletes every fingerprint of `packages` anywhere in a target dir
/// (`<profile>/.fingerprint`, `<triple>/<profile>/.fingerprint`, ...).
/// Returns the number of unit fingerprints removed.
pub fn invalidate(target_dir: &Path, packages: &BTreeSet<String>) -> usize {
    if packages.is_empty() {
        return 0;
    }
    let mut removed = 0;
    for fp in fingerprint_dirs(target_dir) {
        let Ok(rd) = fs::read_dir(&fp) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some((pkg, _)) = split_unit(&name)
                && packages.contains(pkg)
                && fs::remove_dir_all(e.path()).is_ok()
            {
                removed += 1;
            }
        }
    }
    removed
}

/// `foo-bar-0123456789abcdef` -> ("foo-bar", "0123456789abcdef")
pub fn split_unit(name: &str) -> Option<(&str, &str)> {
    let (pkg, hash) = name.rsplit_once('-')?;
    (hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then_some((pkg, hash))
}

/// All `.fingerprint` directories up to three levels below a target dir, and
/// below each re-rooted target dir in `<member>/sub/<name>`.
pub fn fingerprint_dirs(target_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn rec(dir: &Path, depth: usize, top: bool, out: &mut Vec<PathBuf>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name == ".fingerprint" {
                out.push(e.path());
            } else if top && name == "sub" {
                for s in fs::read_dir(e.path()).into_iter().flatten().flatten() {
                    if s.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        rec(&s.path(), 2, false, out);
                    }
                }
            } else if depth > 0
                && !matches!(name.as_ref(), "deps" | "build" | "incremental" | "examples")
            {
                rec(&e.path(), depth - 1, false, out);
            }
        }
    }
    rec(target_dir, 2, true, &mut out);
    out
}

impl Pool {
    pub fn open(dir: PathBuf, info: PoolInfo) -> io::Result<Pool> {
        fs::create_dir_all(&dir)?;
        let info_path = dir.join("pool.json");
        let mut info = read_json::<PoolInfo>(&info_path).unwrap_or(PoolInfo {
            created_ms: now_ms(),
            ..info
        });
        info.last_used_ms = now_ms();
        let _ = write_json(&info_path, &info);
        Ok(Pool { dir, seed: false })
    }

    pub fn with_seeding(mut self, on: bool) -> Pool {
        self.seed = on;
        self
    }

    /// How well member `id` fits `current`: its score and what it must
    /// invalidate. Cargo can reuse workspace artifacts across roots,
    /// including absolute paths baked into code and build-script outputs, so
    /// a member last built for another root invalidates all local packages
    /// (also on upgrade from metadata without a known root). Members are
    /// added from metadata too, so ownership gaps in the manifest cannot keep
    /// another root's artifacts.
    fn assess(
        &self,
        id: usize,
        wt: &Worktree,
        current: &Manifest,
        graph: &RebuildGraph,
        owners: &mut dyn FnMut(&str, Option<&str>) -> Option<String>,
    ) -> (Score, Plan, MemberMeta) {
        let meta = read_json::<MemberMeta>(&self.meta_path(id)).unwrap_or_default();
        let manifest = read_manifest(&self.manifest_path(id));
        let prev = meta.last_worktree.clone();
        let mut p = plan(&manifest, current, &mut |f| owners(f, prev.as_deref()));
        if meta.source_root.as_deref() != Some(&wt.key()) {
            for f in current.keys().chain(manifest.keys()) {
                if let Some(pkg) = owners(f, prev.as_deref()) {
                    p.packages.insert(pkg);
                }
            }
            p.packages.extend(graph.members().iter().cloned());
        }
        let built = self.dir.join(format!("m{id}")).exists() && !manifest.is_empty();
        let score = Score {
            empty: !built,
            packages: graph.cost(&p.packages),
            files: p.changed.len(),
            other_worktree: meta.last_worktree.as_deref() != Some(&wt.key()),
            stale: u64::MAX - meta.last_used_ms,
        };
        (score, p, meta)
    }

    pub fn member_ids(&self) -> Vec<usize> {
        member_ids(&self.dir)
    }

    fn lock_path(&self, id: usize) -> PathBuf {
        self.dir.join(format!("m{id}.lock"))
    }

    /// Leases the best free member, creating one if all are busy and
    /// `may_grow(member_count)` agrees; otherwise waits.
    #[allow(clippy::too_many_arguments)]
    pub fn lease(
        &self,
        wt: &Worktree,
        current: &Manifest,
        graph: &RebuildGraph,
        owners: &mut dyn FnMut(&str, Option<&str>) -> Option<String>,
        max_members: usize,
        may_grow: &mut dyn FnMut(usize) -> bool,
        poll: Duration,
        on_wait: &mut dyn FnMut(),
    ) -> io::Result<Lease> {
        let mut waited = false;
        loop {
            let guard = lock::lock(&self.dir.join("pool.lock"))?;
            let ids = self.member_ids();
            let mut free: Vec<(usize, LockFile)> = Vec::new();
            for id in &ids {
                if let Some(l) = lock::try_lock(&self.lock_path(*id))? {
                    free.push((*id, l));
                }
            }
            let mut best: Option<(Score, usize, Plan)> = None;
            for (id, _) in &free {
                let (score, p, _) = self.assess(*id, wt, current, graph, owners);
                if best.as_ref().map(|(s, _, _)| score < *s).unwrap_or(true) {
                    best = Some((score, *id, p));
                }
            }
            let new_id = (0..).find(|i| !ids.contains(i)).unwrap();
            // Held until a seed from an idle member is cloned.
            let mut seed_lock = None;
            let chosen = match best {
                // Reusing another worktree's member would throw away its
                // warm local packages; a seeded copy rebuilds the same ones
                // and leaves the original warm for that worktree.
                Some((score, id, p))
                    if self.seed
                        && score.other_worktree
                        && !p.packages.is_empty()
                        && ids.len() < max_members
                        && can_clone(&self.dir)
                        && may_grow(ids.len()) =>
                {
                    let pos = free.iter().position(|(i, _)| *i == id).unwrap();
                    seed_lock = Some(free.swap_remove(pos).1);
                    let l = lock::try_lock(&self.lock_path(new_id))?
                        .ok_or_else(|| io::Error::other("new member lock busy"))?;
                    let fresh = plan(&Manifest::new(), current, &mut |f| owners(f, None));
                    // Idle and locked by us: nothing in it is half written.
                    Some((new_id, l, fresh, true, Some((id, p, now_ms()))))
                }
                Some((_, id, p)) => {
                    let pos = free.iter().position(|(i, _)| *i == id).unwrap();
                    Some((id, free.swap_remove(pos).1, p, false, None))
                }
                None if ids.len() < max_members && may_grow(ids.len()) => {
                    let id = new_id;
                    let l = lock::try_lock(&self.lock_path(id))?
                        .ok_or_else(|| io::Error::other("new member lock busy"))?;
                    let p = plan(&Manifest::new(), current, &mut |f| owners(f, None));
                    // Every member is busy here: the closest built one,
                    // still being built in, is the seed.
                    let seed = self
                        .seed
                        .then(|| {
                            ids.iter()
                                .map(|&i| (self.assess(i, wt, current, graph, owners), i))
                                .filter(|((s, _, _), _)| !s.empty)
                                .min_by(|a, b| a.0.0.cmp(&b.0.0))
                                .map(|((_, p, meta), i)| (i, p, meta.last_used_ms))
                        })
                        .flatten();
                    Some((id, l, p, true, seed))
                }
                None => None,
            };
            drop(free);
            if let Some((id, l, p, fresh, seed)) = chosen {
                let target_dir = self.dir.join(format!("m{id}"));
                fs::create_dir_all(&target_dir)?;
                let meta_path = self.meta_path(id);
                let mut meta = read_json::<MemberMeta>(&meta_path).unwrap_or(MemberMeta {
                    id,
                    created_ms: now_ms(),
                    ..Default::default()
                });
                // Invalidate before recording the new manifest: a crash in
                // between only costs a rebuild, never a stale artifact. A
                // member being seeded records no files until it is cloned.
                invalidate(&target_dir, &p.packages);
                let empty = Manifest::new();
                write_manifest(
                    &self.manifest_path(id),
                    if seed.is_some() { &empty } else { current },
                )?;
                meta.id = id;
                meta.last_used_ms = now_ms();
                meta.last_worktree = Some(wt.key());
                meta.source_root = Some(wt.key());
                meta.last_head = wt.head.clone();
                meta.jobs += 1;
                write_json(&meta_path, &meta)?;
                drop(guard);
                // Cloned outside pool.lock: our member lock keeps others out.
                let mut p = p;
                let mut seeded_from = None;
                if let Some((src, sp, since)) = seed {
                    if seed_member(&self.dir.join(format!("m{src}")), &target_dir, since) {
                        invalidate(&target_dir, &sp.packages);
                        p = sp;
                        seeded_from = Some(src);
                    }
                    write_manifest(&self.manifest_path(id), current)?;
                }
                drop(seed_lock);
                return Ok(Lease {
                    id,
                    target_dir,
                    meta_path,
                    meta,
                    plan: p,
                    fresh_member: fresh,
                    seeded_from,
                    _lock: l,
                });
            }
            drop(guard);
            if !waited {
                on_wait();
                waited = true;
            }
            thread::sleep(poll);
        }
    }

    pub fn meta_path(&self, id: usize) -> PathBuf {
        self.dir.join(format!("m{id}.json"))
    }

    pub fn manifest_path(&self, id: usize) -> PathBuf {
        self.dir.join(format!("m{id}.manifest"))
    }
}

pub fn member_ids(pool_dir: &Path) -> Vec<usize> {
    let mut ids = BTreeSet::new();
    if let Ok(rd) = fs::read_dir(pool_dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            let stem = n.split('.').next().unwrap_or("");
            if let Some(num) = stem.strip_prefix('m')
                && let Ok(id) = num.parse::<usize>()
            {
                ids.insert(id);
            }
        }
    }
    ids.into_iter().collect()
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Score {
    empty: bool,
    packages: usize,
    files: usize,
    other_worktree: bool,
    stale: u64,
}

/// Seeds the empty member `dst` with a copy-on-write clone of `src`, which
/// another job may be building in since `since_ms`. Units it touched since
/// then may be half written in the clone, so their fingerprints go and Cargo
/// rebuilds them; so do incremental sessions. What remains matches `src`'s
/// manifest. False, leaving `dst` empty, where the volume cannot clone.
pub fn seed_member(src: &Path, dst: &Path, since_ms: u64) -> bool {
    let Some(name) = dst.file_name() else {
        return false;
    };
    // Not `m<id>`-shaped: member listings must not pick it up.
    let tmp = dst.with_file_name(format!(".seed-{}", name.to_string_lossy()));
    let _ = fs::remove_dir_all(&tmp);
    if clone_tree(src, &tmp).is_err() {
        let _ = fs::remove_dir_all(&tmp);
        return false;
    }
    let since = std::time::UNIX_EPOCH + Duration::from_millis(since_ms.saturating_sub(2_000));
    for fp in fingerprint_dirs(&tmp) {
        if let Some(profile) = fp.parent() {
            let _ = fs::remove_dir_all(profile.join("incremental"));
        }
        for unit in fs::read_dir(&fp).into_iter().flatten().flatten() {
            if touched_since(&unit.path(), since) {
                let _ = fs::remove_dir_all(unit.path());
            }
        }
    }
    if fs::remove_dir(dst).is_err() || fs::rename(&tmp, dst).is_err() {
        let _ = fs::remove_dir_all(&tmp);
        let _ = fs::create_dir_all(dst);
        return false;
    }
    true
}

/// Whether `dir`'s volume clones files (a tiny probe, removed again).
fn can_clone(dir: &Path) -> bool {
    let probe = dir.join(format!(".seed-probe-{}", std::process::id()));
    let copy = dir.join(format!(".seed-probe-{}-copy", std::process::id()));
    let ok = fs::create_dir_all(&probe).is_ok()
        && fs::write(probe.join("f"), b"probe").is_ok()
        && clone_tree(&probe, &copy).is_ok();
    let _ = fs::remove_dir_all(&probe);
    let _ = fs::remove_dir_all(&copy);
    ok
}

fn touched_since(dir: &Path, since: std::time::SystemTime) -> bool {
    fs::read_dir(dir).into_iter().flatten().flatten().any(|e| {
        e.metadata()
            .and_then(|m| m.modified())
            .map_or(true, |t| t >= since)
    })
}

/// APFS clones a directory tree in one call, timestamps included.
#[cfg(target_os = "macos")]
fn clone_tree(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(io::Error::other);
    let (s, d) = (c(src)?, c(dst)?);
    // <sys/clonefile.h>; libc does not export them.
    const CLONE_NOFOLLOW: u32 = 0x0001;
    const CLONE_NOOWNERCOPY: u32 = 0x0002;
    let flags = CLONE_NOFOLLOW | CLONE_NOOWNERCOPY;
    if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Btrfs and XFS clone file by file; mtimes are kept for Cargo's checks.
/// Files the running build removes meanwhile are skipped.
#[cfg(target_os = "linux")]
fn clone_tree(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    fs::create_dir(dst)?;
    for e in fs::read_dir(src)?.flatten() {
        let (from, to) = (e.path(), dst.join(e.file_name()));
        let Ok(ft) = e.file_type() else { continue };
        let gone = |r: io::Result<()>| match r {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        };
        if ft.is_dir() {
            gone(clone_tree(&from, &to))?;
        } else if ft.is_symlink() {
            gone(fs::read_link(&from).and_then(|t| std::os::unix::fs::symlink(t, &to)))?;
        } else if ft.is_file() {
            gone((|| {
                let md = e.metadata()?;
                let s = fs::File::open(&from)?;
                let d = fs::File::create(&to)?;
                if unsafe { libc::ioctl(d.as_raw_fd(), libc::FICLONE, s.as_raw_fd()) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                d.set_permissions(md.permissions())?;
                d.set_times(
                    fs::FileTimes::new()
                        .set_modified(md.modified()?)
                        .set_accessed(md.accessed()?),
                )
            })())?;
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn clone_tree(_src: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Touches files in the worktree (sets mtime to now) so cargo's mtime check
/// sees them as newer than any artifact.
pub fn touch(root: &Path, files: &[String]) -> usize {
    let now = std::time::SystemTime::now();
    let mut n = 0;
    for f in files {
        let p = root.join(f);
        if let Ok(file) = fs::OpenOptions::new().write(true).open(&p)
            && file.set_modified(now).is_ok()
        {
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clone keeps what the source built before `since` (mtimes intact),
    /// drops units touched after it and incremental sessions, and leaves the
    /// destination empty where the volume cannot clone.
    #[test]
    fn seeding_drops_units_the_source_may_still_be_writing() {
        let t = tempfile::tempdir().unwrap();
        let (src, dst) = (t.path().join("m0"), t.path().join("m1"));
        let old = std::time::SystemTime::now() - Duration::from_secs(86_400);
        let put = |rel: &str, aged: bool| {
            let p = src.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, rel).unwrap();
            if aged {
                let f = fs::File::options().write(true).open(&p).unwrap();
                f.set_modified(old).unwrap();
            }
        };
        put("debug/deps/libdone-1.rlib", true);
        put("debug/.fingerprint/done-1/lib-done", true);
        put("debug/.fingerprint/done-1/invoked.timestamp", true);
        put("debug/deps/libbusy-2.rlib", false);
        put("debug/.fingerprint/busy-2/invoked.timestamp", false);
        put("debug/incremental/busy-2/s-1/query-cache.bin", true);
        fs::create_dir_all(&dst).unwrap();
        let since = now_ms() - 3_600_000;
        if !seed_member(&src, &dst, since) {
            assert_eq!(fs::read_dir(&dst).unwrap().count(), 0);
            return;
        }
        let kept = dst.join("debug/deps/libdone-1.rlib");
        assert_eq!(
            fs::read_to_string(&kept).unwrap(),
            "debug/deps/libdone-1.rlib"
        );
        assert_eq!(fs::metadata(&kept).unwrap().modified().unwrap(), old);
        assert!(dst.join("debug/.fingerprint/done-1/lib-done").exists());
        assert!(!dst.join("debug/.fingerprint/busy-2").exists());
        assert!(!dst.join("debug/incremental").exists());
        // The source is untouched, and no seeding leftovers remain.
        assert!(src.join("debug/.fingerprint/busy-2").exists());
        assert!(!t.path().join(".seed-m1").exists());
    }

    fn m(pairs: &[(&str, &str)]) -> Manifest {
        pairs
            .iter()
            .map(|(p, i)| (p.to_string(), i.to_string()))
            .collect()
    }

    fn owner(p: &str) -> Option<String> {
        p.strip_prefix("crates/")
            .and_then(|r| r.split('/').next())
            .map(str::to_string)
    }

    #[test]
    fn plan_groups_by_package() {
        let member = m(&[
            ("crates/a/src/lib.rs", "1"),
            ("crates/b/src/lib.rs", "2"),
            ("crates/b/src/gone.rs", "3"),
            ("README.md", "4"),
        ]);
        let cur = m(&[
            ("crates/a/src/lib.rs", "1"),
            ("crates/b/src/lib.rs", "9"),
            ("crates/c/src/new.rs", "5"),
            ("README.md", "8"),
        ]);
        let p = plan(&member, &cur, &mut |f| owner(f));
        assert_eq!(p.packages.into_iter().collect::<Vec<_>>(), ["b", "c"]);
        assert_eq!(p.outside, ["README.md"]);
        assert_eq!(p.changed.len(), 4);
    }

    #[test]
    fn empty_manifest_needs_no_outside_touches() {
        let current = m(&[("crates/a/src/lib.rs", "1"), ("content/data", "2")]);
        let p = plan(&Manifest::new(), &current, &mut owner);
        assert!(p.outside.is_empty());
        assert!(p.packages.contains("a"));
    }

    #[test]
    fn outside_touches_only_include_manifest_differences() {
        let previous = m(&[("same", "1"), ("changed", "2"), ("removed", "3")]);
        let current = m(&[("same", "1"), ("changed", "4"), ("added", "5")]);
        let p = plan(&previous, &current, &mut |_| None);
        assert_eq!(p.outside, ["added", "changed", "removed"]);
        assert!(plan(&current, &current, &mut |_| None).outside.is_empty());
    }

    #[test]
    fn unit_names() {
        assert_eq!(
            split_unit("open-racing-app-0123456789abcdef"),
            Some(("open-racing-app", "0123456789abcdef"))
        );
        assert_eq!(split_unit("foo-123"), None);
    }

    #[test]
    fn fingerprints_include_rerooted_sub_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path();
        let want = [
            t.join("debug/.fingerprint"),
            t.join("x86_64-unknown-linux-gnu/debug/.fingerprint"),
            t.join("sub/native/release/.fingerprint"),
            t.join("sub/base/aarch64-apple-darwin/debug/.fingerprint"),
        ];
        for d in &want {
            fs::create_dir_all(d).unwrap();
        }
        let mut got = fingerprint_dirs(t);
        got.sort();
        let mut want = want.to_vec();
        want.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn owners_from_manifests() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        fs::create_dir_all(r.join("crates/x/src/deep")).unwrap();
        fs::write(
            r.join("Cargo.toml"),
            "[workspace]\nmembers=[\"crates/x\"]\n",
        )
        .unwrap();
        fs::write(r.join("crates/x/Cargo.toml"), "[package]\nname=\"xx\"\n").unwrap();
        let mut o = PackageOwners::new(vec![r.to_path_buf()]);
        assert_eq!(o.owner("crates/x/src/deep/m.rs").as_deref(), Some("xx"));
        assert_eq!(o.owner("crates/x/Cargo.toml").as_deref(), Some("xx"));
        assert_eq!(o.owner("Cargo.toml"), None);
        assert_eq!(o.owner("docs/a.md"), None);
    }

    #[test]
    fn path_deps_outside_the_manifest_are_reported() {
        use serde_json::json;
        let t = tempfile::tempdir().unwrap();
        let base = crate::util::canonical(t.path());
        let root = base.join("app");
        for d in ["app/sub", "app/ignored", "outside"] {
            fs::create_dir_all(base.join(d)).unwrap();
        }
        let dep = |d: &str| json!({"name": d, "path": base.join(d).to_str().unwrap()});
        let meta = json!({
            "workspace_members": ["app"],
            "packages": [
                {"id": "app", "dependencies": [
                    dep("app/sub"), dep("app/ignored"), dep("outside"),
                    {"name": "serde", "path": null},
                ]},
                {"id": "nonmember", "dependencies": [dep("outside/other")]},
            ],
        });
        let manifest = m(&[("Cargo.toml", "1"), ("sub/Cargo.toml", "2")]);
        assert_eq!(
            uncovered_path_deps(&meta, &root, &manifest),
            [PathBuf::from("../outside"), PathBuf::from("ignored")]
        );
    }

    #[test]
    fn invalidate_removes_only_named_packages() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path();
        for unit in [
            "debug/.fingerprint/core-0123456789abcdef",
            "debug/.fingerprint/core-fedcba9876543210",
            "debug/.fingerprint/serde-0123456789abcdef",
            "x86_64-pc-windows-msvc/release/.fingerprint/core-1111111111111111",
        ] {
            fs::create_dir_all(t.join(unit)).unwrap();
        }
        let n = invalidate(t, &BTreeSet::from(["core".to_string()]));
        assert_eq!(n, 3);
        assert!(t.join("debug/.fingerprint/serde-0123456789abcdef").exists());
    }

    fn wt(name: &str) -> Worktree {
        Worktree {
            root: PathBuf::from(format!("/w/{name}")),
            common_dir: PathBuf::from("/w/main/.git"),
            head: None,
        }
    }

    #[test]
    fn rebuild_graph_counts_transitive_union_and_handles_cycles_and_unknowns() {
        use serde_json::json;
        let package = |name: &str, deps: Vec<serde_json::Value>| {
            json!({
                "id": name, "name": name,
                "manifest_path": format!("/workspace/{name}/Cargo.toml"),
                "dependencies": deps,
            })
        };
        let path_dep = |name: &str| {
            json!({
                "name": name, "path": format!("/workspace/{name}"),
            })
        };
        let meta = json!({
            "workspace_members": ["core", "a", "b", "app", "unrelated"],
            "packages": [
                package("core", vec![]),
                package("a", vec![json!({"name": "core", "rename": "base", "path": "/workspace/core"})]),
                package("b", vec![path_dep("core")]),
                package("app", vec![path_dep("a"), path_dep("b")]),
                package("unrelated", vec![json!({"name": "core", "path": null})]),
                package("nonmember", vec![path_dep("core")]),
            ],
        });
        let mut graph = RebuildGraph::from_metadata(&meta).unwrap();
        assert_eq!(
            graph
                .members()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["a", "app", "b", "core", "unrelated"]
        );
        let cost = |names: &[&str]| graph.cost(&names.iter().map(|s| s.to_string()).collect());
        assert_eq!(cost(&["core"]), 4);
        assert_eq!(cost(&["a", "b"]), 3);
        assert_eq!(cost(&["core", "deleted"]), 5);
        assert_eq!(cost(&["unrelated"]), 1);
        assert_eq!(cost(&[]), 0);
        graph
            .dependents
            .entry("app".into())
            .or_default()
            .insert("core".into());
        assert_eq!(graph.cost(&BTreeSet::from(["core".into()])), 4);
        assert!(RebuildGraph::from_metadata(&json!({})).is_none());
        assert_eq!(
            RebuildGraph::default().cost(&BTreeSet::from(["a".into(), "b".into()])),
            2
        );
    }

    #[test]
    fn lease_prefers_two_leaves_over_a_shared_foundation() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::open(dir.path().join("p"), PoolInfo::default()).unwrap();
        let current = m(&[
            ("crates/core/src/lib.rs", "new"),
            ("crates/a/src/lib.rs", "new"),
            ("crates/b/src/lib.rs", "new"),
            ("crates/app/src/main.rs", "new"),
        ]);
        for id in 0..2 {
            let mut previous = current.clone();
            for name in if id == 0 {
                vec!["core"]
            } else {
                vec!["a", "b"]
            } {
                previous.insert(format!("crates/{name}/src/lib.rs"), "old".into());
            }
            fs::create_dir_all(pool.dir.join(format!("m{id}"))).unwrap();
            write_manifest(&pool.manifest_path(id), &previous).unwrap();
            write_json(
                &pool.meta_path(id),
                &MemberMeta {
                    id,
                    last_worktree: Some(wt("a").key()),
                    source_root: Some(wt("a").key()),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let graph = RebuildGraph {
            dependents: BTreeMap::from([(
                "core".into(),
                BTreeSet::from(["a".into(), "b".into(), "app".into()]),
            )]),
            ..Default::default()
        };
        let lease = pool
            .lease(
                &wt("a"),
                &current,
                &graph,
                &mut |f, _| owner(f),
                2,
                &mut |_| false,
                Duration::from_millis(5),
                &mut || {},
            )
            .unwrap();
        assert_eq!(
            lease.id, 1,
            "rebuild two leaves instead of core and all its dependents"
        );
    }
}
