//! ripple-history: "files that usually change together", from git history.
//!
//! A separate signal from the dependency graph: never an edge, never input
//! to risk. It answers "which files usually change with these, judging by
//! past commits", so an agent can be told about files that are not in its
//! diff and that the graph does not explain (schema ↔ model, locale ↔ view,
//! presenter ↔ JS page, parallel siblings).
//!
//! # Lifecycle
//!
//! * [`History::build`] walks `git log` once (one subprocess, streamed) at a
//!   commit and computes everything in memory. Cancellable between commits.
//! * [`History::save`] / [`History::load`] persist it to one sidecar file
//!   (intended location `.git/ripple/history.bin`).
//! * [`History::update`] moves it to a newer commit: when the old commit is an
//!   ancestor of the new one only the new commits are walked; otherwise (or
//!   when the options changed) it rebuilds.
//! * [`History::partners`] answers queries from the in-memory structure.
//!
//! A `History` is immutable once built: a daemon keeps it behind an `Arc`,
//! builds or updates the next one on a background thread and swaps it in.
//!
//! # Statistics
//!
//! The window is the last `max_commits` non-merge commits reachable from the
//! build commit (optionally only those since a date). Paths are followed
//! through renames to their names at the build commit; files that no longer
//! exist there, and ignored files, are dropped. Commits touching more than
//! `max_files_per_commit` files ("sweeps": formatting, dependency bumps,
//! translation imports) count toward churn but not toward co-change.
//!
//! For each file X, `of(X)` is the number of non-sweep commits in the window
//! touching X, and for each other file Y `together(X, Y)` the number of those
//! that also touch Y. The score is the confidence `together / of`: "changed
//! together in 9 of the last 11 commits that touched X". To keep files that
//! change with everything (lockfiles, `schema.rb`, changelogs, `en.yml`)
//! from crowding out real partners, a partner must also have a lift
//! `confidence / base_rate(Y) >= min_lift`, where `base_rate(Y)` is the
//! share of all non-sweep commits that touch Y. See [`PartnerQuery`].

mod gitlog;
mod store;

pub use gitlog::{Cancelled, resolve_rev};

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::AtomicBool;

/// What a history covers. Part of the sidecar header: a sidecar built with
/// other options is rebuilt, not updated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Last N non-merge commits reachable from the build commit. 0 = all.
    pub max_commits: usize,
    /// Only commits with a committer time at or after this (unix seconds).
    pub since: Option<i64>,
    /// Commits touching more than this many files are sweeps: churn only.
    pub max_files_per_commit: usize,
    /// Pairs changed together fewer times than this are not stored.
    pub min_together: u32,
    /// Partners kept per file (the ones changed together most often).
    pub keep_per_file: usize,
    /// Globs (`ripple.yml` `ignore` syntax) of files never recorded.
    pub ignore: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self { max_commits: 5000, since: None, max_files_per_commit: 30, min_together: 2, keep_per_file: 48, ignore: ripple_core::Config::default().ignore }
    }
}

impl Options {
    /// Options from `ripple.yml` (`history` and `ignore`). `now` (unix
    /// seconds) turns `max_age_days` into a fixed `since`.
    pub fn from_config(cfg: &ripple_core::Config, now: i64) -> Self {
        let h = &cfg.history;
        Self {
            max_commits: h.max_commits,
            since: h.max_age_days.map(|d| now - i64::from(d) * 86_400),
            max_files_per_commit: h.max_files_per_commit,
            min_together: h.min_together.max(1),
            keep_per_file: Options::default().keep_per_file,
            ignore: cfg.ignore.clone(),
        }
    }

    fn ignore_matcher(&self) -> Result<ripple_policy::Policy> {
        let cfg = ripple_core::Config { ignore: self.ignore.clone(), ..Default::default() };
        ripple_policy::Policy::compile(cfg).map_err(|e| anyhow::anyhow!("history ignore globs: {e}"))
    }
}

/// How partners are ranked and filtered at query time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Score {
    /// `together / of` (default).
    Confidence,
    /// `together / (of + 2)`: confidence shrunk towards 0 for files with few
    /// commits (3/3 ranks below 9/10).
    Shrunk,
    /// `together / (of(X) + of(Y) - together)`: punishes partners that change
    /// often on their own.
    Jaccard,
}

/// A query. Defaults are what the replay evaluation supports (see the crate
/// README section in the eval report).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PartnerQuery {
    pub limit: usize,
    /// Minimum number of commits that touched the seed and the partner.
    pub min_together: u32,
    /// Minimum `together / of` of the best seed.
    pub min_confidence: f32,
    /// Minimum lift: confidence divided by the partner's base rate.
    pub min_lift: f32,
    pub score: Score,
    /// Combine seeds by noisy-or (`1 - Π(1 - score)`) instead of taking the best one.
    pub noisy_or: bool,
}

impl Default for PartnerQuery {
    fn default() -> Self {
        Self { limit: 5, min_together: 3, min_confidence: 0.3, min_lift: 2.0, score: Score::Confidence, noisy_or: false }
    }
}

impl PartnerQuery {
    /// The query `ripple.yml` `history:` asks for.
    pub fn from_config(h: &ripple_core::config::HistoryConfig) -> Self {
        Self { limit: h.limit, min_together: h.min_together, min_confidence: h.min_confidence, min_lift: h.min_lift, ..Default::default() }
    }
}

/// A file that usually changes with the query files.
#[derive(Debug, Clone, PartialEq)]
pub struct Partner {
    pub path: String,
    /// Ranking score (see [`Score`]), 0..=1.
    pub score: f32,
    /// Commits in the window that touched both `via` and `path`...
    pub together: u32,
    /// ...out of the commits that touched `via` (sweeps excluded).
    pub of: u32,
    /// The query file this partner is best explained by.
    pub via: String,
    /// Committer time (unix seconds) of the newest commit touching both.
    pub last_together: i64,
    /// Confidence divided by the partner's base rate.
    pub lift: f32,
    /// How many query files have this partner in their lists.
    pub seeds: u32,
}

impl Partner {
    /// "changed together with `X` in 9 of the last 11 commits that touched it".
    pub fn explain(&self) -> String {
        format!("changed together with `{}` in {} of the last {} commits that touched it", self.via, self.together, self.of)
    }
}

/// Per-file churn in the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Churn {
    /// Commits in the window touching the file, sweeps included.
    pub changes: u32,
    /// The same, sweeps excluded (the `of` of confidences).
    pub pair_changes: u32,
    /// Committer time (unix seconds) of the newest one.
    pub last_changed: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pair {
    pub other: u32,
    pub together: u32,
    /// Index into `commits` of the newest commit with both.
    pub last: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommitMeta {
    pub oid: String,
    pub time: i64,
    /// Files in the commit as git reported them (before any filtering).
    pub raw_files: u32,
}

/// Co-change statistics at one commit. Immutable; see the crate docs.
#[derive(Debug, Clone, PartialEq)]
pub struct History {
    pub(crate) head: String,
    pub(crate) opts: Options,
    /// Sorted, distinct paths: `names[name_off[i]..name_off[i + 1]]`.
    pub(crate) names: String,
    pub(crate) name_off: Vec<u32>,
    /// Window commits, newest first.
    pub(crate) commits: Vec<CommitMeta>,
    /// Files of commit `c`: `tx[tx_off[c]..tx_off[c + 1]]`, sorted ids.
    pub(crate) tx_off: Vec<u32>,
    pub(crate) tx: Vec<u32>,
    /// Per file: changes (all), pair_changes (non-sweep), newest commit index.
    pub(crate) churn: Vec<(u32, u32, u32)>,
    /// Partners of file `i`: `adj[adj_off[i]..adj_off[i + 1]]`, by together desc.
    pub(crate) adj_off: Vec<u32>,
    pub(crate) adj: Vec<Pair>,
    /// Non-sweep commits in the window (the denominator of base rates).
    pub(crate) pair_commits: u32,
}

/// What a build or update did, for logs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildStats {
    /// true: walked only the new commits.
    pub incremental: bool,
    /// Commits read from git.
    pub walked: usize,
    pub reason: Option<String>,
}

/// One commit of the window before interning.
struct Tx {
    meta: CommitMeta,
    files: Vec<String>,
}

impl History {
    /// Walks the history of `oid` (a commit-ish) in `repo`.
    /// Returns [`Cancelled`] (as the error's root cause) when `cancel` is set.
    pub fn build(repo: &Path, oid: &str, opts: &Options, cancel: &AtomicBool) -> Result<Self> {
        let head = resolve_rev(repo, oid).with_context(|| format!("resolving {oid}"))?;
        let txs = walk_window(repo, &[head.clone()], opts, &head, cancel)?;
        Ok(Self::assemble(head, opts.clone(), txs))
    }

    /// The history at `new_oid`, reusing `self` when possible: when
    /// `self.head()` is an ancestor of `new_oid` and the options are the same,
    /// only `self.head()..new_oid` is walked. Otherwise rebuilds.
    pub fn update(&self, repo: &Path, new_oid: &str, opts: &Options, cancel: &AtomicBool) -> Result<(Self, BuildStats)> {
        let new_head = resolve_rev(repo, new_oid).with_context(|| format!("resolving {new_oid}"))?;
        let rebuild = |reason: &str| -> Result<(Self, BuildStats)> {
            let h = Self::build(repo, &new_head, opts, cancel)?;
            let walked = h.commits.len();
            Ok((h, BuildStats { incremental: false, walked, reason: Some(reason.to_string()) }))
        };
        if &self.opts != opts {
            return rebuild("options changed");
        }
        if new_head == self.head {
            return Ok((self.clone(), BuildStats { incremental: true, walked: 0, reason: None }));
        }
        if !gitlog::is_ancestor(repo, &self.head, &new_head)? {
            return rebuild("old head is not an ancestor");
        }
        let exists = gitlog::tracked_files(repo, &new_head)?;
        let ignore = opts.ignore_matcher()?;
        let mut renames = gitlog::Renames::default();
        let mut fresh = Vec::new();
        let mut too_many = false;
        let range = [new_head.clone(), format!("^{}", self.head)];
        gitlog::walk(repo, &range, 0, opts.since, cancel, |c| {
            if opts.max_commits > 0 && fresh.len() >= opts.max_commits {
                too_many = true;
                return Ok(());
            }
            fresh.push(to_tx(&mut renames, c, &exists, &ignore));
            Ok(())
        })?;
        if too_many {
            return rebuild("more new commits than the window");
        }
        let walked = fresh.len();
        // Old commits: re-key their files through the new renames.
        let mut txs = fresh;
        for (c, meta) in self.commits.iter().enumerate() {
            if opts.max_commits > 0 && txs.len() >= opts.max_commits {
                break;
            }
            if opts.since.is_some_and(|s| meta.time < s) {
                continue;
            }
            let mut files: Vec<String> = self.commit_files(c).filter_map(|p| renames.resolve(p)).filter(|p| exists.contains(p)).collect();
            files.sort_unstable();
            files.dedup();
            txs.push(Tx { meta: meta.clone(), files });
        }
        Ok((Self::assemble(new_head, opts.clone(), txs), BuildStats { incremental: true, walked, reason: None }))
    }

    /// A history from explicit commits (newest first) instead of git: for
    /// tests and synthetic benchmarks. Paths are taken as they are (no
    /// rename following, existence or ignore filtering); a commit's file
    /// count decides whether it is a sweep.
    pub fn from_commits(head: &str, opts: &Options, commits: &[(&str, i64, &[&str])]) -> Self {
        let txs = commits
            .iter()
            .map(|(oid, time, files)| {
                let mut f: Vec<String> = files.iter().map(|s| s.to_string()).collect();
                f.sort_unstable();
                f.dedup();
                Tx { meta: CommitMeta { oid: oid.to_string(), time: *time, raw_files: files.len() as u32 }, files: f }
            })
            .collect();
        Self::assemble(head.to_string(), opts.clone(), txs)
    }

    /// Loads a sidecar written by [`History::save`]. Fails on a missing,
    /// truncated, corrupt or older-format file: the caller then rebuilds.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        store::decode(&bytes).with_context(|| format!("decoding {}", path.display()))
    }

    /// Writes the sidecar atomically (temp file + rename), creating the directory.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, store::encode(self)).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
        Ok(())
    }

    /// The serialised form (what [`History::save`] writes).
    pub fn to_bytes(&self) -> Vec<u8> {
        store::encode(self)
    }

    /// The commit this history was built at.
    pub fn head(&self) -> &str {
        &self.head
    }
    pub fn options(&self) -> &Options {
        &self.opts
    }
    /// Commits in the window (sweeps included).
    pub fn commit_count(&self) -> usize {
        self.commits.len()
    }
    /// Non-sweep commits in the window.
    pub fn pair_commit_count(&self) -> u32 {
        self.pair_commits
    }
    /// Distinct files with at least one change in the window.
    pub fn file_count(&self) -> usize {
        self.name_off.len().saturating_sub(1)
    }
    /// Stored partner entries (both directions).
    pub fn pair_count(&self) -> usize {
        self.adj.len()
    }
    /// Is this commit in the window? (For leakage checks.)
    pub fn contains_commit(&self, oid: &str) -> bool {
        self.commits.iter().any(|c| c.oid == oid)
    }
    /// Oldest and newest committer times in the window.
    pub fn time_span(&self) -> Option<(i64, i64)> {
        Some((self.commits.last()?.time, self.commits.first()?.time))
    }

    /// Approximate heap bytes held.
    pub fn heap_bytes(&self) -> usize {
        self.names.capacity()
            + self.name_off.capacity() * 4
            + self.commits.capacity() * std::mem::size_of::<CommitMeta>()
            + self.commits.iter().map(|c| c.oid.capacity()).sum::<usize>()
            + self.tx_off.capacity() * 4
            + self.tx.capacity() * 4
            + self.churn.capacity() * 12
            + self.adj_off.capacity() * 4
            + self.adj.capacity() * std::mem::size_of::<Pair>()
    }

    /// The `i`-th file with history (sorted by path), for iteration.
    pub fn path_at(&self, i: usize) -> Option<&str> {
        (i < self.file_count()).then(|| self.name(i as u32))
    }

    pub(crate) fn name(&self, id: u32) -> &str {
        &self.names[self.name_off[id as usize] as usize..self.name_off[id as usize + 1] as usize]
    }

    /// Id of `path` (binary search over the sorted path table).
    fn id(&self, path: &str) -> Option<u32> {
        let (mut lo, mut hi) = (0usize, self.file_count());
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.name(mid as u32).cmp(path) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid as u32),
            }
        }
        None
    }

    fn commit_files(&self, c: usize) -> impl Iterator<Item = &str> + '_ {
        self.tx[self.tx_off[c] as usize..self.tx_off[c + 1] as usize].iter().map(|&id| self.name(id))
    }

    /// Churn of one file in the window; `None` = not changed in the window.
    pub fn churn(&self, path: &str) -> Option<Churn> {
        let id = self.id(path)?;
        let (changes, pair_changes, last) = self.churn[id as usize];
        Some(Churn { changes, pair_changes, last_changed: self.commits[last as usize].time })
    }

    /// Files that usually change with `files`, with the default query
    /// settings and `limit`. The query files themselves are never returned.
    pub fn partners<S: AsRef<str>>(&self, files: &[S], limit: usize) -> Vec<Partner> {
        self.partners_with(files, &PartnerQuery { limit, ..Default::default() }, |_| false)
    }

    /// [`History::partners`] with explicit settings. `exclude(path)` drops
    /// candidates before the limit is applied (files already in the diff or
    /// already explained by the dependency graph, tests, ...).
    pub fn partners_with<S: AsRef<str>>(&self, files: &[S], q: &PartnerQuery, exclude: impl Fn(&str) -> bool) -> Vec<Partner> {
        let mut seeds: Vec<u32> = files.iter().filter_map(|f| self.id(f.as_ref())).collect();
        seeds.sort_unstable();
        seeds.dedup();
        let n = self.pair_commits.max(1) as f32;
        // (partner, score, together, of, seed, last, lift)
        let mut cand: Vec<(u32, f32, u32, u32, u32, u32, f32)> = Vec::new();
        for &x in &seeds {
            let of = self.churn[x as usize].1;
            if of == 0 {
                continue;
            }
            for p in &self.adj[self.adj_off[x as usize] as usize..self.adj_off[x as usize + 1] as usize] {
                if p.together < q.min_together || seeds.binary_search(&p.other).is_ok() {
                    continue;
                }
                let conf = p.together as f32 / of as f32;
                let of_y = self.churn[p.other as usize].1;
                let lift = conf / (of_y as f32 / n);
                if conf < q.min_confidence || lift < q.min_lift {
                    continue;
                }
                let score = match q.score {
                    Score::Confidence => conf,
                    Score::Shrunk => p.together as f32 / (of + 2) as f32,
                    Score::Jaccard => p.together as f32 / (of + of_y - p.together) as f32,
                };
                cand.push((p.other, score, p.together, of, x, p.last, lift));
            }
        }
        // Best explanation per partner first.
        cand.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.total_cmp(&a.1)).then(b.2.cmp(&a.2)).then(a.4.cmp(&b.4)));
        let mut out: Vec<(Partner, u32)> = Vec::new();
        let mut i = 0;
        while i < cand.len() {
            let mut j = i;
            let mut miss = 1.0f32;
            while j < cand.len() && cand[j].0 == cand[i].0 {
                miss *= 1.0 - cand[j].1;
                j += 1;
            }
            let (y, score, together, of, x, last, lift) = cand[i];
            let path = self.name(y);
            if !exclude(path) {
                let score = if q.noisy_or { 1.0 - miss } else { score };
                let p = Partner {
                    path: path.to_string(),
                    score,
                    together,
                    of,
                    via: self.name(x).to_string(),
                    last_together: self.commits[last as usize].time,
                    lift,
                    seeds: (j - i) as u32,
                };
                out.push((p, y));
            }
            i = j;
        }
        out.sort_by(|a, b| b.0.score.total_cmp(&a.0.score).then(b.0.seeds.cmp(&a.0.seeds)).then(b.0.together.cmp(&a.0.together)).then(a.1.cmp(&b.1)));
        out.truncate(q.limit);
        out.into_iter().map(|(p, _)| p).collect()
    }

    /// Interns the window and computes churn and partner lists.
    fn assemble(head: String, opts: Options, txs: Vec<Tx>) -> Self {
        let mut all: Vec<&str> = txs.iter().flat_map(|t| t.files.iter().map(String::as_str)).collect();
        all.sort_unstable();
        all.dedup();
        let mut names = String::with_capacity(all.iter().map(|s| s.len()).sum());
        let mut name_off = Vec::with_capacity(all.len() + 1);
        name_off.push(0u32);
        for p in &all {
            names.push_str(p);
            name_off.push(names.len() as u32);
        }
        let id_of = |p: &str| all.binary_search(&p).expect("interned") as u32;
        let nfiles = all.len();
        let mut tx_off = Vec::with_capacity(txs.len() + 1);
        let mut tx = Vec::new();
        tx_off.push(0u32);
        let mut commits = Vec::with_capacity(txs.len());
        let mut churn = vec![(0u32, 0u32, u32::MAX); nfiles];
        let mut pair_commits = 0u32;
        for (c, t) in txs.iter().enumerate() {
            let sweep = t.meta.raw_files as usize > opts.max_files_per_commit;
            pair_commits += u32::from(!sweep);
            let start = tx.len();
            tx.extend(t.files.iter().map(|p| id_of(p)));
            tx[start..].sort_unstable();
            for &f in &tx[start..] {
                let e = &mut churn[f as usize];
                e.0 += 1;
                e.1 += u32::from(!sweep);
                e.2 = e.2.min(c as u32);
            }
            tx_off.push(tx.len() as u32);
            commits.push(t.meta.clone());
        }
        drop(all);
        let is_sweep = |c: usize| commits[c].raw_files as usize > opts.max_files_per_commit;
        // Inverted index: file -> non-sweep commits (newest first).
        let mut inv_off = vec![0u32; nfiles + 1];
        for c in (0..commits.len()).filter(|&c| !is_sweep(c)) {
            for &f in &tx[tx_off[c] as usize..tx_off[c + 1] as usize] {
                inv_off[f as usize + 1] += 1;
            }
        }
        for i in 0..nfiles {
            inv_off[i + 1] += inv_off[i];
        }
        let mut fill = inv_off.clone();
        let mut inv = vec![0u32; inv_off[nfiles] as usize];
        for c in (0..commits.len()).filter(|&c| !is_sweep(c)) {
            for &f in &tx[tx_off[c] as usize..tx_off[c + 1] as usize] {
                inv[fill[f as usize] as usize] = c as u32;
                fill[f as usize] += 1;
            }
        }
        let mut count = vec![0u32; nfiles];
        let mut last = vec![0u32; nfiles];
        let mut touched = Vec::new();
        let mut adj_off = Vec::with_capacity(nfiles + 1);
        let mut adj = Vec::new();
        adj_off.push(0u32);
        let mut list = Vec::new();
        for x in 0..nfiles {
            for &c in &inv[inv_off[x] as usize..inv_off[x + 1] as usize] {
                for &y in &tx[tx_off[c as usize] as usize..tx_off[c as usize + 1] as usize] {
                    if y as usize == x {
                        continue;
                    }
                    if count[y as usize] == 0 {
                        touched.push(y);
                        last[y as usize] = c;
                    }
                    count[y as usize] += 1;
                }
            }
            list.clear();
            for &y in &touched {
                let t = count[y as usize];
                if t >= opts.min_together {
                    list.push(Pair { other: y, together: t, last: last[y as usize] });
                }
                count[y as usize] = 0;
            }
            touched.clear();
            // Most often together first; among equals the partner that
            // changes least on its own (higher lift), then by path.
            list.sort_unstable_by(|a, b| b.together.cmp(&a.together).then(churn[a.other as usize].1.cmp(&churn[b.other as usize].1)).then(a.other.cmp(&b.other)));
            list.truncate(opts.keep_per_file);
            adj.extend_from_slice(&list);
            adj_off.push(adj.len() as u32);
        }
        adj.shrink_to_fit();
        tx.shrink_to_fit();
        Self { head, opts, names, name_off, commits, tx_off, tx, churn, adj_off, adj, pair_commits }
    }
}

fn to_tx(renames: &mut gitlog::Renames, c: gitlog::RawCommit, exists: &HashSet<String>, ignore: &ripple_policy::Policy) -> Tx {
    let raw_files = c.entries.len() as u32;
    let mut files: Vec<String> = renames.commit(&c).into_iter().filter(|p| exists.contains(p) && !ignore.is_ignored(p)).collect();
    files.sort_unstable();
    files.dedup();
    Tx { meta: CommitMeta { oid: c.oid, time: c.time, raw_files }, files }
}

/// The window of `revs` (newest first), resolved to paths at `at`.
fn walk_window(repo: &Path, revs: &[String], opts: &Options, at: &str, cancel: &AtomicBool) -> Result<Vec<Tx>> {
    let exists = gitlog::tracked_files(repo, at)?;
    let ignore = opts.ignore_matcher()?;
    let mut renames = gitlog::Renames::default();
    let mut txs = Vec::new();
    gitlog::walk(repo, revs, opts.max_commits, opts.since, cancel, |c| {
        txs.push(to_tx(&mut renames, c, &exists, &ignore));
        Ok(())
    })?;
    Ok(txs)
}

#[cfg(test)]
mod tests;
