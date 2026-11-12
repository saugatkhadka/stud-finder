//! Change detection inside the daemon: the answer of
//! [`crate::git::changed_files`], cached and kept up to date incrementally.
//!
//! The one-shot path runs `merge-base`, two `git diff`s and
//! `git ls-files -o` on every call (~150-600 ms on large repos, mostly
//! `ls-files -o` walking the whole tree). The daemon instead keeps two
//! results and patches them with the paths its file watcher reports:
//!
//! * **untracked files** (`ls-files -o --exclude-standard`): depends only on
//!   the index's path set, the work tree and ignore rules. Re-listed with a
//!   pathspec of just the paths that changed since the last query.
//! * **the diff** per (target tree, HEAD): `git diff -z --raw --numstat -p
//!   -U0 -M TARGET` in one process. Re-run with a pathspec of the changed
//!   paths plus every added / deleted / renamed path of the previous result.
//!   Renames pair added with deleted paths only (`-M`, no copies), so a
//!   pathspec that contains all of them yields exactly the full diff's
//!   entries for those paths, renames included.
//!
//! "Changed paths" are: watcher events since the result was computed, and
//! paths whose index entry changed (found by parsing `.git/index`, re-read
//! only when its stat changes). Before using the watcher's events, the
//! request does a cookie round trip through the watcher (see
//! `server::cookie_sync`), so every edit made before the request is known.
//!
//! Anything this cannot account for precisely forces a full recomputation:
//! a new HEAD or target, changed git config or exclude files, a
//! `.gitignore` / `.gitattributes` / nested `.git` among the changed paths,
//! lost watcher events, an index format the parser does not handle (split or
//! sparse index), or more than [`MAX_PARTIAL`] changed paths. Without a
//! working watcher every request recomputes in full.
//!
//! Small differences from the one-shot path, by design: the daemon runs git
//! with `GIT_OPTIONAL_LOCKS=0` (it never rewrites the index to refresh stat
//! data) and runs the binary from `git --exec-path` (skipping wrapper
//! scripts such as macOS's `/usr/bin/git` shim, ~5 ms per call).

use crate::git::{self, DiffSpec, EMPTY_TREE};
use ripple_core::RelPath;
use ripple_core::store::StrTable;
use ripple_policy::{ChangedFile, FileStatus};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Instant;

/// Above this many changed paths a full recomputation is used (git matches
/// pathspecs linearly, and the command line must stay short).
pub const MAX_PARTIAL: usize = 1000;
/// Watcher paths retained for results that have not caught up yet.
const LOG_CAP: usize = 100_000;
/// Diff results kept (one per target tree and HEAD).
const MAX_DIFFS: usize = 8;
/// Cookie files the daemon writes into the work tree when `.git` is not a directory.
const COOKIE_PREFIX: &str = "ripple-cookie-";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// ---------------------------------------------------------------------------
// Watcher log

/// Paths reported by the file watcher, in order, so each cached result can
/// ask for "everything since I was computed".
#[derive(Default)]
pub struct DirtyLog {
    inner: Mutex<LogInner>,
}

#[derive(Default)]
struct LogInner {
    /// Sequence number of the next path.
    next: u64,
    /// Sequence number of `paths[0]`; anything older was dropped.
    floor: u64,
    paths: VecDeque<RelPath>,
}

impl DirtyLog {
    pub fn push<'a>(&self, paths: impl IntoIterator<Item = &'a RelPath>) {
        let mut l = lock(&self.inner);
        for p in paths {
            l.paths.push_back(p.clone());
            l.next += 1;
        }
        while l.paths.len() > LOG_CAP {
            l.paths.pop_front();
            l.floor += 1;
        }
    }

    /// Events were lost (watcher overflow / rescan): every result recomputes.
    pub fn overflow(&self) {
        let mut l = lock(&self.inner);
        l.paths.clear();
        // A phantom lost event, so results that were fully caught up are stale too.
        l.next += 1;
        l.floor = l.next;
    }

    fn position(&self) -> u64 {
        lock(&self.inner).next
    }

    /// Paths delivered at or after `seq` and the position after them;
    /// `None` when some of them were dropped.
    fn since(&self, seq: u64) -> Option<(BTreeSet<RelPath>, u64)> {
        let l = lock(&self.inner);
        if seq < l.floor {
            return None;
        }
        let skip = ((seq - l.floor) as usize).min(l.paths.len());
        Some((l.paths.range(skip..).cloned().collect(), l.next))
    }
}

// ---------------------------------------------------------------------------
// Reading git state without running git

/// Where a repository's files are (`.git` may be a file for linked work trees).
#[derive(Debug, Clone)]
pub struct GitLayout {
    pub gitdir: PathBuf,
    pub common: PathBuf,
    /// The root's path inside the work tree (`""`, or `"sub/"` when ripple's
    /// root is a subdirectory): index paths are relative to the top, git
    /// output (`--relative`) and watcher paths to the root.
    pub prefix: String,
}

impl GitLayout {
    /// `None` when `.git` cannot be read, refs are stored in a reftable, or
    /// git's location is overridden by the environment; callers then run git.
    pub fn discover(root: &Path) -> Option<Self> {
        if ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR", "GIT_OBJECT_DIRECTORY"].iter().any(|k| std::env::var_os(k).is_some()) {
            return None;
        }
        let top = root.ancestors().find(|d| d.join(".git").exists())?;
        let dotgit = top.join(".git");
        let gitdir = if dotgit.is_dir() {
            dotgit
        } else {
            let s = std::fs::read_to_string(&dotgit).ok()?;
            let g = s.lines().next()?.strip_prefix("gitdir:")?.trim();
            let g = Path::new(g);
            if g.is_absolute() { g.to_path_buf() } else { top.join(g) }
        };
        let common = match std::fs::read_to_string(gitdir.join("commondir")) {
            Ok(s) => {
                let c = Path::new(s.trim());
                if c.is_absolute() { c.to_path_buf() } else { gitdir.join(c) }
            }
            Err(_) => gitdir.clone(),
        };
        if common.join("reftable").exists() || !gitdir.join("HEAD").is_file() {
            return None;
        }
        let prefix = root.strip_prefix(top).ok()?.to_str()?.to_string();
        let prefix = if prefix.is_empty() { prefix } else { format!("{prefix}/") };
        Some(Self { gitdir, common, prefix })
    }

    fn ref_file(&self, name: &str) -> PathBuf {
        let per_worktree = !name.starts_with("refs/") || ["refs/bisect/", "refs/worktree/", "refs/rewritten/"].iter().any(|p| name.starts_with(p));
        if per_worktree { self.gitdir.join(name) } else { self.common.join(name) }
    }
}

/// Identity of a file's current version (`None`: absent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    mtime: i64,
    mtime_ns: i64,
    ctime: i64,
    ctime_ns: i64,
    size: u64,
    ino: u64,
}

pub fn stamp(p: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(p).ok()?;
    Some(Stamp { mtime: m.mtime(), mtime_ns: m.mtime_nsec(), ctime: m.ctime(), ctime_ns: m.ctime_nsec(), size: m.size(), ino: m.ino() })
}

type PackedRefs = HashMap<String, String>;

fn parse_packed_refs(text: &str) -> PackedRefs {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.starts_with('^'))
        .filter_map(|l| l.split_once(' '))
        .map(|(oid, name)| (name.trim().to_string(), oid.to_string()))
        .collect()
}

fn is_hex_oid(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

// ---------------------------------------------------------------------------
// The index, as far as change detection needs it

/// Path -> fingerprint of its index entries (stage, mode, object id, and the
/// assume-unchanged / skip-worktree / intent-to-add flags), as two sorted
/// columns (a large repository's index has ~100k entries; a map of
/// `String`s costs twice as much).
#[derive(Default)]
struct IndexMap {
    paths: StrTable,
    fps: Vec<u64>,
    /// Submodule paths: changes inside them are reported by the watcher as
    /// `sub/file` but show up in a diff as `sub`.
    gitlinks: Vec<RelPath>,
}

fn fnv(h: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *h ^= *b as u64;
        *h = h.wrapping_mul(0x100000001b3);
    }
}

/// Parses index versions 2-4. `None` for anything else, including split
/// indexes (`link`) and sparse indexes (`sdir`), whose entries are not all
/// in this file.
fn parse_index(data: &[u8], oid_len: usize, prefix: &str) -> Option<IndexMap> {
    let be32 = |at: usize| -> Option<u32> { data.get(at..at + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]])) };
    if data.get(0..4)? != b"DIRC" {
        return None;
    }
    let version = be32(4)?;
    if !(2..=4).contains(&version) {
        return None;
    }
    let count = be32(8)? as usize;
    let mut gitlinks = Vec::new();
    let mut cols = Columns::default();
    let mut pos = 12;
    let mut prev: Vec<u8> = Vec::new();
    for _ in 0..count {
        let start = pos;
        let mode = be32(start + 24)?;
        let oid = data.get(start + 40..start + 40 + oid_len)?;
        let flags_at = start + 40 + oid_len;
        let flags = u16::from_be_bytes(data.get(flags_at..flags_at + 2)?.try_into().ok()?);
        let mut name_at = flags_at + 2;
        let mut ext = 0u16;
        if flags & 0x4000 != 0 {
            if version < 3 {
                return None;
            }
            ext = u16::from_be_bytes(data.get(name_at..name_at + 2)?.try_into().ok()?);
            name_at += 2;
        }
        let name: Vec<u8> = if version == 4 {
            // Varint: bytes to drop from the previous name, then the NUL-terminated suffix.
            let mut i = name_at;
            let mut c = *data.get(i)?;
            i += 1;
            let mut strip = (c & 127) as usize;
            while c & 128 != 0 {
                strip += 1;
                c = *data.get(i)?;
                i += 1;
                strip = (strip << 7) + (c & 127) as usize;
            }
            let end = i + data.get(i..)?.iter().position(|b| *b == 0)?;
            let keep = prev.len().checked_sub(strip)?;
            let mut n = prev[..keep].to_vec();
            n.extend_from_slice(&data[i..end]);
            pos = end + 1;
            n
        } else {
            let end = name_at + data.get(name_at..)?.iter().position(|b| *b == 0)?;
            // Entries are padded with 1-8 NULs to a multiple of 8 bytes.
            let len = end - start;
            pos = start + ((len + 8) & !7);
            data[name_at..end].to_vec()
        };
        if mode & 0o170000 == 0o040000 {
            return None; // sparse directory entry
        }
        let full = String::from_utf8_lossy(&name);
        let in_root = full.strip_prefix(prefix).map(str::to_string);
        prev = name;
        let Some(path) = in_root else { continue };
        let mut h: u64 = 0xcbf29ce484222325;
        fnv(&mut h, &mode.to_be_bytes());
        fnv(&mut h, oid);
        fnv(&mut h, &(flags & 0xb000).to_be_bytes()); // assume-valid, stage
        fnv(&mut h, &(ext & 0x6000).to_be_bytes()); // skip-worktree, intent-to-add
        if mode & 0o170000 == 0o160000 {
            gitlinks.push(path.clone());
        }
        cols.push(&path, h);
    }
    // Extensions, up to the trailing checksum.
    let end = data.len().checked_sub(oid_len)?;
    while pos + 8 <= end {
        let sig = &data[pos..pos + 4];
        if sig == b"link" || sig == b"sdir" {
            return None;
        }
        pos += 8 + be32(pos + 4)? as usize;
    }
    let (paths, fps) = cols.finish();
    Some(IndexMap { paths, fps, gitlinks })
}

/// Builds the columns of an [`IndexMap`] from entries in index order.
#[derive(Default)]
struct Columns {
    bytes: Vec<u8>,
    ends: Vec<u32>,
    fps: Vec<u64>,
    /// Entries so far were in strictly increasing path order.
    unsorted: bool,
}

impl Columns {
    fn last(&self) -> Option<&[u8]> {
        let n = self.ends.len();
        (n > 0).then(|| &self.bytes[if n == 1 { 0 } else { self.ends[n - 2] as usize }..self.ends[n - 1] as usize])
    }

    fn push(&mut self, path: &str, h: u64) {
        match self.last().map(|l| l.cmp(path.as_bytes())) {
            // Several stages of one path combine into one fingerprint.
            Some(std::cmp::Ordering::Equal) => {
                let f = self.fps.last_mut().expect("an entry");
                *f = f.wrapping_mul(31).wrapping_add(h);
                return;
            }
            Some(std::cmp::Ordering::Greater) => self.unsorted = true,
            _ => {}
        }
        self.bytes.extend_from_slice(path.as_bytes());
        self.ends.push(self.bytes.len() as u32);
        self.fps.push(h);
    }

    fn finish(self) -> (StrTable, Vec<u64>) {
        let table = StrTable::new_checked(self.ends.into(), self.bytes.into()).expect("built from strings");
        if !self.unsorted {
            return (table, self.fps);
        }
        // Not in path order (git always writes it sorted): sort and combine.
        let mut v: Vec<(&str, u64)> = table.iter().zip(self.fps.iter().copied()).collect();
        v.sort_by(|a, b| a.0.cmp(b.0));
        let mut again = Columns::default();
        for (p, h) in v {
            again.push(p, h);
        }
        again.finish()
    }
}

/// Paths whose entries differ between two index maps (a merge of the sorted columns).
fn index_delta(old: &IndexMap, new: &IndexMap) -> BTreeSet<RelPath> {
    let mut out = BTreeSet::new();
    let (mut i, mut j) = (0, 0);
    while i < old.fps.len() || j < new.fps.len() {
        let ord = match (i < old.fps.len(), j < new.fps.len()) {
            (true, true) => old.paths.get(i).cmp(new.paths.get(j)),
            (true, false) => std::cmp::Ordering::Less,
            _ => std::cmp::Ordering::Greater,
        };
        match ord {
            std::cmp::Ordering::Less => {
                out.insert(old.paths.get(i).to_string());
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.insert(new.paths.get(j).to_string());
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                if old.fps[i] != new.fps[j] {
                    out.insert(old.paths.get(i).to_string());
                }
                i += 1;
                j += 1;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Cached results

/// What a cached result still has to account for.
#[derive(Default)]
struct Pending {
    /// Position in the [`DirtyLog`] the result reflects.
    seq: u64,
    /// Changed index entries since then.
    index: BTreeSet<RelPath>,
    /// Must be recomputed from scratch.
    full: bool,
}

struct Untracked {
    pending: Pending,
    /// Untracked path -> its report entry.
    files: BTreeMap<RelPath, ChangedFile>,
}

struct DiffResult {
    target: String,
    head: Option<String>,
    pending: Pending,
    files: BTreeMap<RelPath, ChangedFile>,
    used: u64,
}

struct StagedResult {
    head: Option<String>,
    index: Option<Stamp>,
    seq: u64,
    files: Vec<ChangedFile>,
}

#[derive(Default)]
struct Inner {
    /// Stats of git config / exclude / attribute files: any change drops everything.
    config: Option<Vec<Option<Stamp>>>,
    index_stamp: Option<Option<Stamp>>,
    /// `None` when the index could not be parsed.
    index: Option<IndexMap>,
    packed: Option<(Option<Stamp>, PackedRefs)>,
    merge_bases: HashMap<String, String>,
    untracked: Option<Untracked>,
    diffs: Vec<DiffResult>,
    staged: Option<StagedResult>,
    clock: u64,
}

impl Inner {
    fn clear(&mut self) {
        let clock = self.clock;
        *self = Inner { clock, ..Inner::default() };
    }
    fn index_changed(&mut self, delta: Option<&BTreeSet<RelPath>>) {
        let all = self.untracked.iter_mut().map(|u| &mut u.pending).chain(self.diffs.iter_mut().map(|d| &mut d.pending));
        for p in all {
            match delta {
                Some(d) => p.index.extend(d.iter().cloned()),
                None => p.full = true,
            }
        }
    }
}

/// Counters for `stats`.
#[derive(Default)]
pub struct GitCounters {
    pub hits: AtomicU64,
    pub partial: AtomicU64,
    pub full: AtomicU64,
    pub last_us: AtomicU64,
}

/// What git has to run for one cached result.
enum Plan {
    Keep,
    Full,
    Partial(BTreeSet<RelPath>),
}

pub struct GitTracker {
    root: PathBuf,
    layout: Option<GitLayout>,
    exe: OnceLock<PathBuf>,
    pub log: DirtyLog,
    /// Set once the file watcher runs; without it nothing is cached.
    pub watching: AtomicBool,
    pub counters: GitCounters,
    inner: Mutex<Inner>,
    /// Called when a query finds HEAD at a new commit (see [`GitTracker::on_head_move`]).
    head_hook: OnceLock<Box<dyn Fn() + Send + Sync>>,
    /// FNV of the last HEAD seen (0: none yet).
    last_head: AtomicU64,
    /// Specs asked for recently, newest last (a daemon resuming from sleep prewarms them).
    recent: Mutex<Vec<DiffSpec>>,
}

/// Resolves the git binary once: `git --exec-path`/git skips wrappers such
/// as the macOS `/usr/bin/git` shim.
fn find_git() -> PathBuf {
    let direct = Command::new("git")
        .arg("--exec-path")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()).join("git"))
        .filter(|p| p.is_file());
    direct.unwrap_or_else(|| PathBuf::from("git"))
}

fn has_component(p: &str, name: &str) -> bool {
    p.split('/').any(|c| c == name)
}

fn file_name(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// Removes `path` and everything under `path/` from `map`.
fn remove_covered<V>(map: &mut BTreeMap<RelPath, V>, path: &str) {
    map.remove(path);
    let lo = format!("{path}/");
    let hi = format!("{path}0"); // '0' follows '/'
    let keys: Vec<RelPath> = map.range(lo..hi).map(|(k, _)| k.clone()).collect();
    for k in keys {
        map.remove(&k);
    }
}

impl GitTracker {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            layout: GitLayout::discover(root),
            exe: OnceLock::new(),
            log: DirtyLog::default(),
            watching: AtomicBool::new(false),
            counters: GitCounters::default(),
            inner: Mutex::new(Inner::default()),
            head_hook: OnceLock::new(),
            last_head: AtomicU64::new(0),
            recent: Mutex::new(Vec::new()),
        }
    }

    /// The specs asked for recently, newest last.
    pub fn recent_specs(&self) -> Vec<DiffSpec> {
        lock(&self.recent).clone()
    }

    /// Computes the results for `specs` now so later requests find them cached.
    pub fn prewarm_specs(&self, specs: &[DiffSpec]) {
        for s in specs {
            let _ = self.changed(s);
        }
    }

    /// Registers `f` to be told (cheaply, on the request thread) whenever a
    /// diff query finds HEAD at a different commit than the previous one.
    pub fn on_head_move(&self, f: impl Fn() + Send + Sync + 'static) {
        let _ = self.head_hook.set(Box::new(f));
    }

    fn note_head(&self, head: &Option<String>) {
        let mut h: u64 = 0xcbf29ce484222325;
        fnv(&mut h, head.as_deref().unwrap_or("-").as_bytes());
        let prev = self.last_head.swap(h, Ordering::Relaxed);
        // The first observation is not a move (the history thread reads HEAD itself).
        if prev != 0
            && prev != h
            && let Some(f) = self.head_hook.get()
        {
            f();
        }
    }

    fn exe(&self) -> &Path {
        self.exe.get_or_init(find_git)
    }

    fn git(&self, args: &[&str]) -> Result<Vec<u8>, String> {
        let out = Command::new(self.exe())
            .arg("-C")
            .arg(&self.root)
            .args(["-c", "core.quotepath=off"])
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("running git (is it installed?): {e}"))?;
        if !out.status.success() {
            let shown: Vec<&str> = args.iter().take(12).copied().collect();
            let more = if args.len() > 12 { " ..." } else { "" };
            return Err(format!("git {}{more} failed: {}", shown.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(out.stdout)
    }

    /// Fills the caches for the working tree in the background, so the
    /// first `impact` does not pay for `git ls-files -o`.
    pub fn prewarm(&self) {
        let _ = self.changed(&DiffSpec::WorkingTree);
    }

    /// Files whose change invalidates everything: config (diff options,
    /// excludesFile, attributes), exclude and attribute files, shallow info.
    fn config_stamps(&self) -> Vec<Option<Stamp>> {
        let mut files: Vec<PathBuf> = Vec::new();
        if let Some(l) = &self.layout {
            for f in ["config", "info/exclude", "info/attributes", "shallow"] {
                files.push(l.common.join(f));
            }
            files.push(l.gitdir.join("config.worktree"));
        }
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            files.push(home.join(".gitconfig"));
            let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
            for f in ["git/config", "git/ignore", "git/attributes"] {
                files.push(xdg.join(f));
            }
        }
        files.iter().map(|f| stamp(f)).collect()
    }

    fn packed<'a>(&self, inner: &'a mut Inner, l: &GitLayout) -> &'a PackedRefs {
        let path = l.common.join("packed-refs");
        let st = stamp(&path);
        if inner.packed.as_ref().is_none_or(|(s, _)| *s != st) {
            let refs = std::fs::read_to_string(&path).map(|t| parse_packed_refs(&t)).unwrap_or_default();
            inner.packed = Some((st, refs));
        }
        &inner.packed.as_ref().expect("just set").1
    }

    /// Contents of ref `name` (symrefs followed); `Ok(None)` if it does not
    /// exist, `Err` if it cannot be read reliably.
    fn read_ref(&self, inner: &mut Inner, l: &GitLayout, name: &str, depth: u32) -> Result<Option<String>, ()> {
        if depth > 5 || name.is_empty() || name.contains("..") || name.starts_with('/') || name.contains('\\') {
            return Err(());
        }
        let path = l.ref_file(name);
        match std::fs::read_to_string(&path) {
            Ok(s) => {
                let s = s.trim();
                match s.strip_prefix("ref:") {
                    Some(target) => self.read_ref(inner, l, target.trim(), depth + 1),
                    None => Ok(Some(s.to_string())),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound || path.is_dir() => Ok(if name.starts_with("refs/") { self.packed(inner, l).get(name).cloned() } else { None }),
            Err(_) => Err(()),
        }
    }

    /// HEAD's commit, `None` on an unborn branch.
    fn head(&self, inner: &mut Inner) -> Result<Option<String>, String> {
        if let Some(l) = self.layout.clone()
            && let Ok(v) = self.read_ref(inner, &l, "HEAD", 0)
            && v.as_deref().is_none_or(is_hex_oid)
        {
            return Ok(v);
        }
        match self.git(&["rev-parse", "--verify", "-q", "HEAD"]) {
            Ok(out) if !out.is_empty() => Ok(Some(String::from_utf8_lossy(&out).trim().to_string())),
            _ => Ok(None),
        }
    }

    /// A cache key that changes whenever `merge-base(spec, HEAD)` may: the
    /// spec, HEAD, and the value of every ref the spec's name could resolve
    /// to. `None` for specs this does not understand (reflog, `:/text`, ...).
    fn base_key(&self, inner: &mut Inner, spec: &str, head: &Option<String>) -> Option<String> {
        let l = self.layout.clone()?;
        if spec.is_empty() || spec.contains(['@', ':', '{', ' ', '\t', '*', '?', '[']) || spec.contains("..") || spec.starts_with('-') {
            return None;
        }
        // Strip `~N` / `^N` suffixes to find the name.
        let mut anchor = spec;
        while let Some(i) = anchor.rfind(['~', '^']) {
            if anchor[i + 1..].bytes().all(|b| b.is_ascii_digit()) {
                anchor = &anchor[..i];
            } else {
                break;
            }
        }
        if anchor.is_empty() {
            return None;
        }
        let mut key = format!("{spec}\0{}", head.as_deref().unwrap_or("-"));
        let candidates = [anchor.to_string(), format!("refs/{anchor}"), format!("refs/tags/{anchor}"), format!("refs/heads/{anchor}"), format!("refs/remotes/{anchor}"), format!("refs/remotes/{anchor}/HEAD")];
        for c in candidates {
            let v = self.read_ref(inner, &l, &c, 0).ok()?;
            key.push('\0');
            key.push_str(v.as_deref().unwrap_or("-"));
        }
        Some(key)
    }

    fn merge_base(&self, inner: &mut Inner, spec: &str, head: &Option<String>) -> Result<String, String> {
        let key = self.base_key(inner, spec, head);
        if let Some(k) = &key
            && let Some(t) = inner.merge_bases.get(k)
        {
            return Ok(t.clone());
        }
        let out = self
            .git(&["merge-base", spec, "HEAD"])
            .map_err(|e| format!("finding the merge base of `{spec}` and HEAD (in CI, check out with fetch-depth: 0): {e}"))?;
        let t = String::from_utf8_lossy(&out).trim().to_string();
        if let Some(k) = key {
            if inner.merge_bases.len() > 64 {
                inner.merge_bases.clear();
            }
            inner.merge_bases.insert(k, t.clone());
        }
        Ok(t)
    }

    /// Re-reads the index if its stat changed and tells every cached result
    /// which entries changed.
    fn refresh_index(&self, inner: &mut Inner) {
        let Some(l) = &self.layout else {
            inner.index_changed(None);
            return;
        };
        let path = l.gitdir.join("index");
        let st = stamp(&path);
        if inner.index_stamp == Some(st) {
            return;
        }
        let oid_len = if self.sha256() { 32 } else { 20 };
        let new = match st {
            None => Some(IndexMap::default()),
            Some(_) => std::fs::read(&path).ok().and_then(|d| parse_index(&d, oid_len, &l.prefix)),
        };
        // Read the index again if it changed while we read it.
        let st2 = stamp(&path);
        let (st, new) = if st2 != st { (None, None) } else { (st, new) };
        let delta = match (&inner.index, &new) {
            (Some(old), Some(new)) => Some(index_delta(old, new)),
            _ => None,
        };
        inner.index_changed(delta.as_ref());
        inner.index = new;
        // An unreadable state is never cached as "seen".
        inner.index_stamp = if inner.index.is_some() { Some(st) } else { None };
    }

    fn sha256(&self) -> bool {
        self.layout.as_ref().is_some_and(|l| {
            std::fs::read_to_string(l.common.join("config")).is_ok_and(|c| c.lines().any(|line| {
                let t = line.trim().to_ascii_lowercase();
                t.starts_with("objectformat") && t.contains("sha256")
            }))
        })
    }

    /// The paths a result must re-check, or `None` if it needs a full recomputation.
    fn take_pending(&self, p: &Pending) -> Option<(BTreeSet<RelPath>, u64)> {
        if p.full {
            return None;
        }
        let (mut paths, upto) = self.log.since(p.seq)?;
        paths.extend(p.index.iter().cloned());
        Some((paths, upto))
    }

    /// The changed files for `spec`; identical to [`git::changed_files`].
    pub fn changed(&self, spec: &DiffSpec) -> Result<Vec<ChangedFile>, String> {
        {
            let mut r = lock(&self.recent);
            r.retain(|s| s != spec);
            r.push(spec.clone());
            let n = r.len().saturating_sub(MAX_DIFFS / 2);
            r.drain(..n);
        }
        let t = Instant::now();
        let mut inner = lock(&self.inner);
        let r = self.changed_locked(&mut inner, spec);
        if r.is_err() {
            // Never keep a half-updated state.
            inner.clear();
        }
        self.counters.last_us.store(t.elapsed().as_micros() as u64, Ordering::Relaxed);
        r
    }

    fn changed_locked(&self, inner: &mut Inner, spec: &DiffSpec) -> Result<Vec<ChangedFile>, String> {
        let cfg = self.config_stamps();
        if !self.watching.load(Ordering::SeqCst) || inner.config.as_ref() != Some(&cfg) {
            inner.clear();
            inner.config = Some(cfg);
        }
        let head = self.head(inner)?;
        self.note_head(&head);
        self.refresh_index(inner);
        match spec {
            DiffSpec::Staged => self.staged(inner, &head),
            DiffSpec::WorkingTree => {
                let target = head.clone().unwrap_or_else(|| EMPTY_TREE.to_string());
                self.worktree(inner, target, &head)
            }
            DiffSpec::Base(r) => {
                let target = self.merge_base(inner, r, &head)?;
                self.worktree(inner, target, &head)
            }
        }
    }

    fn count(&self, plan: &Plan) {
        let c = match plan {
            Plan::Keep => &self.counters.hits,
            Plan::Full => &self.counters.full,
            Plan::Partial(_) => &self.counters.partial,
        };
        c.fetch_add(1, Ordering::Relaxed);
    }

    fn worktree(&self, inner: &mut Inner, target: String, head: &Option<String>) -> Result<Vec<ChangedFile>, String> {
        inner.clock += 1;
        let clock = inner.clock;
        let gitlinks: Vec<RelPath> = inner.index.as_ref().map(|i| i.gitlinks.clone()).unwrap_or_default();

        // Untracked files.
        let (u_plan, u_upto) = match &inner.untracked {
            None => (Plan::Full, self.log.position()),
            Some(u) => match self.take_pending(&u.pending) {
                None => (Plan::Full, self.log.position()),
                Some((paths, upto)) if paths.is_empty() => (Plan::Keep, upto),
                Some((paths, upto)) => {
                    let full = paths.len() > MAX_PARTIAL || paths.iter().any(|p| file_name(p) == ".gitignore" || has_component(p, ".git"));
                    (if full { Plan::Full } else { Plan::Partial(paths) }, upto)
                }
            },
        };

        // The diff.
        let slot = inner.diffs.iter().position(|d| d.target == target && d.head == *head);
        let (d_plan, d_upto) = match slot.map(|i| &inner.diffs[i]) {
            None => (Plan::Full, self.log.position()),
            Some(d) => match self.take_pending(&d.pending) {
                None => (Plan::Full, self.log.position()),
                Some((paths, upto)) if paths.is_empty() => (Plan::Keep, upto),
                Some((mut paths, upto)) => {
                    if paths.len() > MAX_PARTIAL || paths.iter().any(|p| file_name(p) == ".gitattributes") {
                        (Plan::Full, upto)
                    } else {
                        // Changes inside a submodule show up as the submodule itself.
                        let subs: Vec<RelPath> = gitlinks.iter().filter(|g| paths.iter().any(|p| p.starts_with(&format!("{g}/")))).cloned().collect();
                        paths.extend(subs);
                        // Every rename candidate, so pairing is decided as in a full diff.
                        for f in d.files.values() {
                            if f.status != FileStatus::Modified {
                                paths.insert(f.path.clone());
                                paths.extend(f.old_path.clone());
                            }
                        }
                        if paths.len() > MAX_PARTIAL { (Plan::Full, upto) } else { (Plan::Partial(paths), upto) }
                    }
                }
            },
        };
        self.count(&d_plan);

        let u_args: Option<Vec<String>> = match &u_plan {
            Plan::Keep => None,
            Plan::Full => Some(vec!["ls-files".into(), "-z".into(), "-o".into(), "--exclude-standard".into()]),
            Plan::Partial(paths) => {
                let mut a: Vec<String> = ["--literal-pathspecs", "ls-files", "-z", "-o", "--exclude-standard", "--"].map(String::from).to_vec();
                a.extend(paths.iter().cloned());
                Some(a)
            }
        };
        let diff_base = ["diff", "-z", "--raw", "--numstat", "-p", "-U0", "--no-color", "-M", "--relative", "--no-ext-diff", "--src-prefix=a/", "--dst-prefix=b/"];
        let d_args: Option<Vec<String>> = match &d_plan {
            Plan::Keep => None,
            Plan::Full => {
                let mut a: Vec<String> = diff_base.map(String::from).to_vec();
                a.push(target.clone());
                a.push("--".into());
                Some(a)
            }
            Plan::Partial(paths) => {
                let mut a: Vec<String> = vec!["--literal-pathspecs".into()];
                a.extend(diff_base.map(String::from));
                a.push(target.clone());
                a.push("--".into());
                a.extend(paths.iter().cloned());
                Some(a)
            }
        };

        // Both in parallel.
        let (u_out, d_out) = std::thread::scope(|s| {
            let u = u_args.as_ref().map(|a| s.spawn(move || self.git(&a.iter().map(String::as_str).collect::<Vec<_>>())));
            let d = d_args.as_ref().map(|a| self.git(&a.iter().map(String::as_str).collect::<Vec<_>>()));
            let u = u.map(|h| h.join().unwrap_or_else(|_| Err("git thread panicked".into())));
            (u, d)
        });

        // Apply the untracked listing.
        if let Some(out) = u_out {
            let out = out?;
            let listed = git::split_z(&out).filter(|p| !file_name(p).starts_with(COOKIE_PREFIX));
            match u_plan {
                Plan::Full => {
                    let files = listed.map(|p| (p.clone(), git::untracked_file(&self.root, p))).collect();
                    inner.untracked = Some(Untracked { pending: Pending { seq: u_upto, ..Pending::default() }, files });
                }
                Plan::Partial(paths) => {
                    let u = inner.untracked.as_mut().expect("partial plan has a result");
                    for p in &paths {
                        remove_covered(&mut u.files, p);
                    }
                    for p in listed {
                        let f = git::untracked_file(&self.root, p.clone());
                        u.files.insert(p, f);
                    }
                    u.pending = Pending { seq: u_upto, ..Pending::default() };
                }
                Plan::Keep => {}
            }
        } else if let Some(u) = &mut inner.untracked {
            u.pending.seq = u_upto;
        }

        // Apply the diff.
        let idx = match d_out {
            Some(out) => {
                let out = out?;
                let (raw, patch) = git::split_combined(&out);
                let mut files = git::parse_raw_numstat(raw);
                git::apply_patch(&mut files, patch, &HashSet::new());
                match (d_plan, slot) {
                    (Plan::Partial(paths), Some(i)) => {
                        let d = &mut inner.diffs[i];
                        for p in &paths {
                            remove_covered(&mut d.files, p);
                        }
                        d.files.extend(files);
                        d.pending = Pending { seq: d_upto, ..Pending::default() };
                        d.used = clock;
                        i
                    }
                    (_, slot) => {
                        let r = DiffResult { target, head: head.clone(), pending: Pending { seq: d_upto, ..Pending::default() }, files, used: clock };
                        match slot {
                            Some(i) => {
                                inner.diffs[i] = r;
                                i
                            }
                            None => {
                                if inner.diffs.len() >= MAX_DIFFS
                                    && let Some(old) = inner.diffs.iter().enumerate().min_by_key(|(_, d)| d.used).map(|(i, _)| i)
                                {
                                    inner.diffs.remove(old);
                                }
                                inner.diffs.push(r);
                                inner.diffs.len() - 1
                            }
                        }
                    }
                }
            }
            None => {
                let i = slot.expect("keep plan has a result");
                inner.diffs[i].pending.seq = d_upto;
                inner.diffs[i].used = clock;
                i
            }
        };

        let mut files = inner.diffs[idx].files.clone();
        if let Some(u) = &inner.untracked {
            for (p, f) in &u.files {
                files.entry(p.clone()).or_insert_with(|| f.clone());
            }
        }
        Ok(files.into_values().collect())
    }

    /// `--staged`: cached while HEAD, the index and the work tree are
    /// unchanged; otherwise recomputed in full (3 git calls in parallel).
    fn staged(&self, inner: &mut Inner, head: &Option<String>) -> Result<Vec<ChangedFile>, String> {
        let index = inner.index_stamp.flatten();
        if inner.index_stamp.is_some()
            && let Some(s) = &inner.staged
            && s.head == *head
            && s.index == index
            && self.log.since(s.seq).is_some_and(|(p, _)| p.is_empty())
        {
            self.counters.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(s.files.clone());
        }
        self.counters.full.fetch_add(1, Ordering::Relaxed);
        let seq = self.log.position();
        let target = head.clone().unwrap_or_else(|| EMPTY_TREE.to_string());
        let diff = ["diff", "-z", "--raw", "--numstat", "-p", "-U0", "--no-color", "-M", "--relative", "--no-ext-diff", "--src-prefix=a/", "--dst-prefix=b/", "--cached", &target, "--"];
        let (out, unstaged) = std::thread::scope(|s| {
            let u = s.spawn(|| self.git(&["diff", "--name-only", "-z", "--relative", "--no-ext-diff", "--"]));
            (self.git(&diff), u.join().unwrap_or_else(|_| Err("git thread panicked".into())))
        });
        let out = out?;
        let (raw, patch) = git::split_combined(&out);
        let mut files = git::parse_raw_numstat(raw);
        let unstaged: HashSet<String> = unstaged.map(|o| git::split_z(&o).collect()).unwrap_or_default();
        git::apply_patch(&mut files, patch, &unstaged);
        let files: Vec<ChangedFile> = files.into_values().collect();
        inner.staged = Some(StagedResult { head: head.clone(), index, seq, files: files.clone() });
        Ok(files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_since_and_overflow() {
        let log = DirtyLog::default();
        let a = "a".to_string();
        let b = "b".to_string();
        log.push([&a]);
        let pos = log.position();
        assert_eq!(log.since(0).unwrap().0, BTreeSet::from([a.clone()]));
        log.push([&b, &a]);
        assert_eq!(log.since(pos).unwrap(), (BTreeSet::from([a.clone(), b.clone()]), 3));
        assert!(log.since(3).unwrap().0.is_empty());
        log.overflow();
        assert!(log.since(3).is_none(), "a caught-up reader must see the loss");
        assert!(log.since(log.position()).unwrap().0.is_empty());
    }

    #[test]
    fn covered_paths() {
        let mut m: BTreeMap<RelPath, ()> = ["a", "a/b", "a/c/d", "a0", "a.rb", "ab/c"].iter().map(|s| (s.to_string(), ())).collect();
        remove_covered(&mut m, "a");
        assert_eq!(m.keys().cloned().collect::<Vec<_>>(), vec!["a.rb", "a0", "ab/c"]);
    }

    #[test]
    fn packed_refs() {
        let p = parse_packed_refs("# pack-refs with: peeled fully-peeled sorted \naaaa refs/heads/main\nbbbb refs/tags/v1\n^cccc\n");
        assert_eq!(p.get("refs/heads/main").map(String::as_str), Some("aaaa"));
        assert_eq!(p.get("refs/tags/v1").map(String::as_str), Some("bbbb"));
        assert_eq!(p.len(), 2);
    }
}
