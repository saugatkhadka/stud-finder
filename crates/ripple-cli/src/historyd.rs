//! Git-history signals inside the daemon: churn of the changed files and
//! the files that usually change with them ([`ripple_history`]), loaded in
//! the background and never on a query's path.
//!
//! # State machine
//!
//! ```text
//!   start ── history.enabled: false ─────────────────────────▶ disabled (no thread, no file)
//!     │ ── not a git repo / shallow clone ───────────────────▶ unavailable(reason) (no thread)
//!     ▼
//!   building ── sidecar usable (same options, head ⊑ HEAD) ──▶ ready (+ updating, in the background)
//!     │ ── else: build from scratch ─────────────────────────▶ ready
//!     │ ── unborn HEAD / build error ────────────────────────▶ unavailable(reason) (retried on the next HEAD move)
//!   ready ── HEAD moved (debounced) ── update ──────────────▶ ready (updating while it runs)
//!   any ── `reload` with history.enabled: false ─────────────▶ disabled (thread exits)
//! ```
//!
//! *ready* means a [`History`] is published. Two separate flags say what it
//! is good for: `churn` (any commit in the window) and `cochange` (enough
//! history whose commits are not mostly sweeps; see [`assess`]).
//!
//! # Threads and locks
//!
//! One background thread per daemon, at utility QoS on macOS (nice 10 on
//! Linux); the `git` processes it starts run at nice 10 (thread priority is
//! not inherited by them). It sleeps on a condvar until a HEAD
//! move is reported: idle cost is zero. Queries read the published [`View`]
//! by cloning an `Arc` under an `RwLock` that the thread takes for writing
//! only to swap in a finished one.
//!
//! HEAD moves come from the daemon's existing machinery, no extra watcher or
//! polling: the file watcher's events for `.git/HEAD`, `.git/refs/heads/*`
//! and `.git/packed-refs` (which the indexer otherwise drops), and
//! [`crate::gitd::GitTracker`] noticing a new HEAD while answering a diff
//! query (linked work trees, whose git dir is outside the watched root).
//! Moves are debounced ([`DEBOUNCE`] of quiet, and not while a rebase, `am`
//! or cherry-pick sequence is in progress, up to [`MAX_DEFER`]), so a
//! rebase of 50 commits causes one update. A newer move cancels a running
//! update (not the first build from scratch, which would never finish on a
//! busy large repo; it is followed by an update instead); shutdown cancels
//! anything.
//!
//! # Sidecar
//!
//! `<git common dir>/ripple/history.bin`, shared by the work trees of one
//! repository, written atomically (temp file + rename) after every build or
//! update. One-shot mode (`--no-daemon`) only reads it; `ripple history
//! build` writes it in the foreground.

use crate::engine::Snapshot;
use anyhow::{Context, Result, bail};
use ripple_core::Config;
use ripple_history::{BuildStats, History, Options, PartnerQuery};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The sidecar, relative to the git common dir.
pub const SIDECAR: &str = "ripple/history.bin";
/// Quiet period after the last HEAD move before updating.
pub const DEBOUNCE: Duration = Duration::from_millis(500);
/// Longest an update waits for a rebase / am / cherry-pick sequence to end.
pub const MAX_DEFER: Duration = Duration::from_secs(60);
/// Co-change needs at least this many non-sweep commits in the window.
pub const MIN_PAIR_COMMITS: u32 = 300;
/// Co-change is off when more than this share of the window's commits are
/// sweeps (touch more than `max_files_per_commit` files): mirrors and
/// squash-imported histories bundle unrelated changes.
pub const MAX_SWEEP_SHARE: f64 = 0.25;
/// After this many updates in a row were cancelled by newer HEAD moves, the
/// next one runs to completion.
const MAX_CANCELS_IN_ROW: u32 = 3;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// `now` rounded down to the day, so `history.max_age_days` changes the
/// options (and forces a rebuild) at most once a day.
fn options_for(cfg: &Config) -> Options {
    Options::from_config(cfg, now_unix() / 86_400 * 86_400)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Disabled,
    Building,
    Ready,
    Unavailable(String),
}

impl State {
    pub fn name(&self) -> &'static str {
        match self {
            State::Disabled => "disabled",
            State::Building => "building",
            State::Ready => "ready",
            State::Unavailable(_) => "unavailable",
        }
    }
    fn reason(&self) -> Option<String> {
        match self {
            State::Unavailable(r) => Some(r.clone()),
            _ => None,
        }
    }
}

/// What a built history is good for, from cheap statistics of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Assessment {
    pub churn: bool,
    /// `Err(reason)`: co-change partners are not shown.
    pub cochange: Result<(), String>,
    pub commits: usize,
    pub pair_commits: u32,
    /// Share of the window's commits that are sweeps.
    pub sweep_share: f64,
    pub pairs: usize,
}

/// The auto-off heuristic (see [`MIN_PAIR_COMMITS`], [`MAX_SWEEP_SHARE`]).
pub fn assess(h: &History) -> Assessment {
    let commits = h.commit_count();
    let pair_commits = h.pair_commit_count();
    let sweep_share = if commits == 0 { 0.0 } else { h.sweep_commit_count() as f64 / commits as f64 };
    let pairs = h.pair_count();
    let cochange = if commits == 0 {
        Err("no commits in the window".to_string())
    } else if pair_commits < MIN_PAIR_COMMITS {
        Err(format!("history too short: {pair_commits} commits of at most {} files, co-change needs {MIN_PAIR_COMMITS}", h.options().max_files_per_commit))
    } else if sweep_share > MAX_SWEEP_SHARE {
        Err(format!(
            "low-signal history: {:.0}% of the last {commits} commits touch more than {} files (a mirror or squashed imports?), co-change is off above {:.0}%",
            sweep_share * 100.0,
            h.options().max_files_per_commit,
            MAX_SWEEP_SHARE * 100.0
        ))
    } else if pairs == 0 {
        Err("no files changed together often enough".to_string())
    } else {
        Ok(())
    };
    Assessment { churn: commits > 0, cochange, commits, pair_commits, sweep_share, pairs }
}

/// How the published history was obtained, for `stats`.
#[derive(Debug, Clone, Default)]
pub struct BuildInfo {
    /// Loaded from the sidecar (possibly then updated).
    pub from_sidecar: bool,
    pub incremental: bool,
    pub walked: usize,
    pub build_ms: u64,
    pub finished_unix: i64,
    pub sidecar_bytes: u64,
    pub note: Option<String>,
}

/// One published state. Immutable; queries hold it through an `Arc`.
#[derive(Debug, Clone)]
pub struct View {
    pub state: State,
    pub history: Option<Arc<History>>,
    pub assessment: Option<Assessment>,
    /// A build or update is running in the background.
    pub updating: bool,
    /// The history is older than HEAD (one-shot mode with a stale sidecar).
    pub behind_head: bool,
    pub sidecar: Option<PathBuf>,
    pub build: BuildInfo,
}

impl View {
    fn new(state: State) -> Self {
        Self { state, history: None, assessment: None, updating: false, behind_head: false, sidecar: None, build: BuildInfo::default() }
    }

    /// The status fields only (no per-file data).
    pub fn info(&self) -> HistoryInfo {
        let a = self.assessment.as_ref();
        HistoryInfo {
            state: self.state.name().to_string(),
            reason: self.state.reason(),
            head: self.history.as_ref().map(|h| h.head().to_string()),
            commits: self.history.as_ref().map_or(0, |h| h.commit_count()),
            churn_available: a.is_some_and(|a| a.churn),
            cochange: a.is_some_and(|a| a.cochange.is_ok()),
            cochange_reason: a.and_then(|a| a.cochange.clone().err()),
            updating: self.updating,
            behind_head: self.behind_head,
            ..HistoryInfo::default()
        }
    }

    /// [`View::info`] plus churn of `changed` and their co-change partners
    /// (`exclude(path)` drops partners: files in the diff, direct graph
    /// neighbours, ...). Microseconds: binary searches and one partner merge.
    pub fn lookup<S: AsRef<str>>(&self, changed: &[S], query: &PartnerQuery, exclude: impl Fn(&str) -> bool) -> HistoryInfo {
        let t = Instant::now();
        let mut info = self.info();
        let (Some(h), Some(a)) = (&self.history, &self.assessment) else { return info };
        if a.churn {
            info.recent_days = ripple_history::RECENT_DAYS as u32;
            for c in changed {
                let c = c.as_ref();
                let ch = h.churn(c);
                info.churn.insert(
                    c.to_string(),
                    FileChurn { commits: ch.map_or(0, |c| c.changes), recent_commits: ch.map_or(0, |c| c.recent_changes), last_changed: ch.map(|c| c.last_changed) },
                );
            }
        }
        if a.cochange.is_ok() && query.limit > 0 {
            info.usually_changes_with = h
                .partners_with(changed, query, exclude)
                .into_iter()
                .map(|p| CoChange { path: p.path, via: p.via, together: p.together, of: p.of, score: round3(p.score), lift: round3(p.lift), last_together: p.last_together, seeds: p.seeds })
                .collect();
        }
        info.lookup_us = Some(t.elapsed().as_micros() as u64);
        info
    }

    /// The `history` object of `stats`.
    pub fn stats_json(&self) -> Value {
        let mut v = serde_json::to_value(self.info()).unwrap_or(Value::Null);
        if let Some(a) = &self.assessment {
            v["pair_commits"] = json!(a.pair_commits);
            v["sweep_share"] = json!(round3(a.sweep_share as f32));
            v["pairs"] = json!(a.pairs);
        }
        if let Some(h) = &self.history {
            v["files"] = json!(h.file_count());
            v["heap_bytes"] = json!(h.heap_bytes());
            v["time_span"] = json!(h.time_span());
        }
        if let Some(p) = &self.sidecar {
            v["sidecar"] = json!(p);
        }
        let b = &self.build;
        v["build"] = json!({
            "from_sidecar": b.from_sidecar,
            "incremental": b.incremental,
            "walked": b.walked,
            "ms": b.build_ms,
            "finished_unix": b.finished_unix,
            "sidecar_bytes": b.sidecar_bytes,
            "note": b.note,
        });
        v
    }
}

/// A view saying history is turned off.
pub fn disabled_view() -> View {
    View::new(State::Disabled)
}

fn round3(x: f32) -> f32 {
    (x * 1000.0).round() / 1000.0
}

/// The `history` object of a reply (see [`crate::protocol`]). Status fields
/// always; `churn` and `usually_changes_with` on `impact` replies when ready.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryInfo {
    /// `ready`, `building`, `disabled` or `unavailable`.
    pub state: String,
    /// Why it is unavailable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The commit the published history was built at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Commits in the window.
    pub commits: usize,
    /// Churn is meaningful (some commit in the window).
    pub churn_available: bool,
    /// Co-change partners are shown (see `cochange_reason` when not).
    pub cochange: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cochange_reason: Option<String>,
    /// A newer history is being built in the background.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub updating: bool,
    /// The history predates HEAD (one-shot mode with an older sidecar).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub behind_head: bool,
    /// The window of `FileChurn::recent_commits`, in days.
    #[serde(skip_serializing_if = "is_zero")]
    pub recent_days: u32,
    /// Per changed file (every changed file is listed).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub churn: BTreeMap<String, FileChurn>,
    /// Files outside the diff and its direct graph neighbours that usually
    /// change with it, best first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub usually_changes_with: Vec<CoChange>,
    /// Time the churn + partner lookup took.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lookup_us: Option<u64>,
}

fn is_zero(x: &u32) -> bool {
    *x == 0
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FileChurn {
    /// Commits in the window touching the file (0: none; a new file?).
    pub commits: u32,
    /// ... of which in the last `recent_days` of the window.
    pub recent_commits: u32,
    /// Committer time (unix seconds) of the newest one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_changed: Option<i64>,
}

/// "`path` changed together with `via` in `together` of the last `of`
/// commits that touched `via`".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CoChange {
    pub path: String,
    pub via: String,
    pub together: u32,
    pub of: u32,
    /// `together / of`.
    pub score: f32,
    /// `score` over the partner's base rate.
    pub lift: f32,
    /// Committer time (unix seconds) of the newest commit with both.
    pub last_together: i64,
    /// Changed files that have this partner.
    pub seeds: u32,
}

/// The `history` object of an `impact` reply: churn of `changed` and
/// partners outside the diff and its direct graph neighbours (dependents
/// and dependencies at depth 1), with the snapshot's `history` settings.
pub fn impact_history(view: &View, snap: &Snapshot, changed: &[String]) -> HistoryInfo {
    let cfg = &snap.policy.config.history;
    if !cfg.enabled {
        return View::new(State::Disabled).info();
    }
    if view.assessment.as_ref().is_none_or(|a| a.cochange.is_err()) {
        return view.lookup(changed, &PartnerQuery { limit: 0, ..PartnerQuery::from_config(cfg) }, |_| true);
    }
    let g = &snap.graph;
    let diff: HashSet<&str> = changed.iter().map(String::as_str).collect();
    let seeds: HashSet<u32> = changed.iter().filter_map(|c| g.id(c)).collect();
    // Direct dependencies of the change (usually few); a candidate's own
    // dependencies tell whether it is a direct dependent (fan-in can be huge).
    let deps: HashSet<u32> = seeds.iter().flat_map(|&s| g.dependencies(s).map(|(n, _)| n)).collect();
    let exclude = |p: &str| diff.contains(p) || g.id(p).is_some_and(|n| deps.contains(&n) || g.dependencies(n).any(|(d, _)| seeds.contains(&d)));
    view.lookup(changed, &PartnerQuery::from_config(cfg), exclude)
}

// ---------------------------------------------------------------------------
// Git facts

#[derive(Debug, Clone)]
pub struct GitDirs {
    /// Per work tree (rebase state lives here).
    pub gitdir: PathBuf,
    /// Shared by the work trees (the sidecar lives here).
    pub common: PathBuf,
    pub shallow: bool,
}

impl GitDirs {
    /// `Err(reason)` when `root` is not in a git work tree.
    pub fn probe(root: &Path) -> Result<Self, String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "--path-format=absolute", "--absolute-git-dir", "--git-common-dir", "--is-shallow-repository"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| format!("running git: {e}"))?;
        if !out.status.success() {
            return Err("not a git repository".into());
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut l = text.lines();
        let (Some(g), Some(c), Some(s)) = (l.next(), l.next(), l.next()) else { return Err("not a git repository".into()) };
        Ok(Self { gitdir: PathBuf::from(g), common: PathBuf::from(c), shallow: s.trim() == "true" })
    }

    pub fn sidecar(&self) -> PathBuf {
        self.common.join(SIDECAR)
    }

    /// A rebase, `git am` or cherry-pick / revert sequence is running.
    fn sequence_in_progress(&self) -> bool {
        ["rebase-merge", "rebase-apply", "sequencer"].iter().any(|d| self.gitdir.join(d).exists())
    }
}

const SHALLOW: &str = "shallow clone: history is incomplete (run `git fetch --unshallow`)";

fn head_of(root: &Path) -> Result<String, String> {
    ripple_history::resolve_rev(root, "HEAD").ok().filter(|h| !h.is_empty()).ok_or_else(|| "no commits yet".to_string())
}

fn load_config(root: &Path) -> Config {
    ripple_policy::load_config(root).map(|l| l.config).unwrap_or_default()
}

fn is_cancelled(e: &anyhow::Error) -> bool {
    e.root_cause().downcast_ref::<ripple_history::Cancelled>().is_some()
}

/// Low priority for this thread and, separately, for the `git` processes it
/// starts (neither macOS QoS nor Linux per-thread nice is inherited by them).
fn lower_priority() {
    ripple_history::lower_child_priority();
    #[cfg(target_os = "macos")]
    // SAFETY: only changes this thread's QoS class.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
    }
    #[cfg(target_os = "linux")]
    // SAFETY: setpriority on this thread's id only.
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 10);
    }
}

// ---------------------------------------------------------------------------
// The daemon side

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Job {
    Build,
    Update,
}

#[derive(Default)]
struct Ctl {
    /// HEAD moves reported, and when the last one was.
    nudges: u64,
    first_pending: Option<Instant>,
    last_nudge: Option<Instant>,
    shutdown: bool,
    /// Config says disabled (after a `reload`): the thread exits.
    disabled: bool,
    job: Option<Job>,
    cancels_in_row: u32,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Counters for `stats`.
#[derive(Default)]
pub struct HistoryCounters {
    pub builds: AtomicU64,
    pub updates: AtomicU64,
    pub cancelled: AtomicU64,
    pub nudges: AtomicU64,
}

pub struct HistoryD {
    root: PathBuf,
    view: RwLock<Arc<View>>,
    ctl: Mutex<Ctl>,
    cv: Condvar,
    cancel: AtomicBool,
    pub counters: HistoryCounters,
    /// Tests: hold every build / update this long first.
    hold: Duration,
}

impl HistoryD {
    /// Starts the background thread unless history is disabled or `root`
    /// is not in a git repository.
    pub fn start(root: &Path, cfg: &Config) -> Arc<Self> {
        Self::start_with_hold(root, cfg, Duration::ZERO)
    }

    /// [`HistoryD::start`], holding every build / update for `hold` (tests).
    pub fn start_with_hold(root: &Path, cfg: &Config, hold: Duration) -> Arc<Self> {
        let me = Arc::new(Self {
            hold,
            root: root.to_path_buf(),
            view: RwLock::new(Arc::new(View::new(State::Building))),
            ctl: Mutex::new(Ctl::default()),
            cv: Condvar::new(),
            cancel: AtomicBool::new(false),
            counters: HistoryCounters::default(),
        });
        me.configure(cfg.history.enabled);
        me
    }

    /// The current state (cheap: clones an `Arc`).
    pub fn view(&self) -> Arc<View> {
        self.view.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn publish(&self, v: View) {
        *self.view.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(v);
    }

    fn set_updating(&self, updating: bool) {
        let mut v = (*self.view()).clone();
        if v.updating != updating {
            v.updating = updating;
            self.publish(v);
        }
    }

    /// Applies `history.enabled` (at start and on `reload`).
    pub fn configure(self: &Arc<Self>, enabled: bool) {
        let mut c = lock(&self.ctl);
        if c.shutdown {
            return;
        }
        if !enabled {
            c.disabled = true;
            self.cancel.store(true, Ordering::SeqCst);
            self.cv.notify_all();
            drop(c);
            self.publish(View::new(State::Disabled));
            return;
        }
        c.disabled = false;
        if c.thread.as_ref().is_some_and(|t| !t.is_finished()) {
            // Re-check options and HEAD.
            drop(c);
            self.nudge();
            return;
        }
        if !crate::files::in_git_repo(&self.root) {
            drop(c);
            self.publish(View::new(State::Unavailable("not a git repository".into())));
            return;
        }
        self.publish(View::new(State::Building));
        self.cancel.store(false, Ordering::SeqCst);
        let me = self.clone();
        match std::thread::Builder::new().name("ripple-history".into()).spawn(move || me.run()) {
            Ok(t) => c.thread = Some(t),
            Err(e) => {
                drop(c);
                self.publish(View::new(State::Unavailable(format!("could not start the history thread: {e}"))));
            }
        }
    }

    /// HEAD may have moved. Cheap; called from the watcher and request threads.
    pub fn nudge(&self) {
        self.counters.nudges.fetch_add(1, Ordering::Relaxed);
        let mut c = lock(&self.ctl);
        let now = Instant::now();
        c.nudges += 1;
        c.last_nudge = Some(now);
        c.first_pending.get_or_insert(now);
        if c.job == Some(Job::Update) && c.cancels_in_row < MAX_CANCELS_IN_ROW {
            self.cancel.store(true, Ordering::SeqCst);
        }
        self.cv.notify_all();
    }

    /// Stops the thread (cancelling a running build); see [`HistoryD::join`].
    pub fn shutdown(&self) {
        let mut c = lock(&self.ctl);
        c.shutdown = true;
        self.cancel.store(true, Ordering::SeqCst);
        self.cv.notify_all();
    }

    /// Waits for the thread after [`HistoryD::shutdown`].
    pub fn join(&self) {
        let t = lock(&self.ctl).thread.take();
        if let Some(t) = t {
            let _ = t.join();
        }
    }

    /// Whether the background thread is running (tests).
    pub fn thread_running(&self) -> bool {
        lock(&self.ctl).thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    fn stopping(&self) -> bool {
        let c = lock(&self.ctl);
        c.shutdown || c.disabled
    }

    /// Waits for a debounced HEAD move. `false`: stop.
    fn wait_for_move(&self, seen: &mut u64, dirs: &GitDirs) -> bool {
        let mut c = lock(&self.ctl);
        loop {
            if c.shutdown || c.disabled {
                return false;
            }
            if c.nudges == *seen {
                c = self.cv.wait(c).unwrap_or_else(|p| p.into_inner());
                continue;
            }
            let quiet = c.last_nudge.map_or(DEBOUNCE, |t| t.elapsed());
            if quiet < DEBOUNCE {
                c = self.cv.wait_timeout(c, DEBOUNCE - quiet).unwrap_or_else(|p| p.into_inner()).0;
                continue;
            }
            let deferred = c.first_pending.map_or(Duration::ZERO, |t| t.elapsed());
            drop(c);
            if deferred < MAX_DEFER && dirs.sequence_in_progress() {
                // Each step of the sequence moves HEAD again; this is a backstop.
                c = lock(&self.ctl);
                c = self.cv.wait_timeout(c, DEBOUNCE).unwrap_or_else(|p| p.into_inner()).0;
                continue;
            }
            c = lock(&self.ctl);
            *seen = c.nudges;
            c.first_pending = None;
            return true;
        }
    }

    fn begin(&self, job: Job) -> bool {
        let mut c = lock(&self.ctl);
        let until = Instant::now() + self.hold;
        while !(c.shutdown || c.disabled) && Instant::now() < until {
            c = self.cv.wait_timeout(c, until - Instant::now()).unwrap_or_else(|p| p.into_inner()).0;
        }
        if c.shutdown || c.disabled {
            return false;
        }
        c.job = Some(job);
        self.cancel.store(false, Ordering::SeqCst);
        true
    }

    fn end(&self, cancelled: bool) {
        let mut c = lock(&self.ctl);
        c.job = None;
        c.cancels_in_row = if cancelled { c.cancels_in_row + 1 } else { 0 };
    }

    fn run(self: Arc<Self>) {
        lower_priority();
        let debug = std::env::var_os("RIPPLE_DEBUG").is_some();
        let dirs = match GitDirs::probe(&self.root) {
            Ok(d) if d.shallow => {
                self.publish(View::new(State::Unavailable(SHALLOW.into())));
                return;
            }
            Ok(d) => d,
            Err(reason) => {
                self.publish(View::new(State::Unavailable(reason)));
                return;
            }
        };
        let sidecar = dirs.sidecar();
        let mut cur: Option<Arc<History>> = None;
        let mut info = BuildInfo::default();
        let mut seen = 0u64;
        let mut first = true;
        loop {
            if !first && !self.wait_for_move(&mut seen, &dirs) {
                return;
            }
            first = false;
            let cfg = load_config(&self.root);
            if !cfg.history.enabled {
                // `reload` will call `configure`; until then nothing to do.
                continue;
            }
            let opts = options_for(&cfg);
            let head = match head_of(&self.root) {
                Ok(h) => h,
                Err(reason) => {
                    if cur.is_none() {
                        self.publish(View::new(State::Unavailable(reason)));
                    }
                    continue;
                }
            };
            if cur.as_ref().is_some_and(|h| h.head() == head && h.options() == &opts) {
                continue;
            }
            let t = Instant::now();
            if cur.is_none() {
                match History::load(&sidecar) {
                    Ok(h) if h.options() == &opts && (h.head() == head || ripple_history::is_ancestor(&self.root, h.head(), &head).unwrap_or(false)) => {
                        let h = Arc::new(h);
                        info = BuildInfo { from_sidecar: true, build_ms: t.elapsed().as_millis() as u64, finished_unix: now_unix(), sidecar_bytes: file_len(&sidecar), ..BuildInfo::default() };
                        self.publish_ready(&h, &sidecar, &info, h.head() != head);
                        cur = Some(h);
                        if cur.as_ref().is_some_and(|h| h.head() == head) {
                            continue;
                        }
                    }
                    Ok(_) => info.note = Some("sidecar was for other options or another line of history; rebuilt".into()),
                    Err(e) if sidecar.exists() => {
                        eprintln!("ripple: ignoring the history cache {} ({e:#}); rebuilding", sidecar.display());
                        info.note = Some(format!("unreadable sidecar rebuilt: {e:#}"));
                    }
                    Err(_) => {}
                }
            }
            let job = if cur.is_some() { Job::Update } else { Job::Build };
            if !self.begin(job) {
                return;
            }
            if cur.is_some() {
                self.set_updating(true);
            }
            let t = Instant::now();
            let result: Result<(History, BuildStats)> = match &cur {
                None => History::build(&self.root, &head, &opts, &self.cancel).map(|h| {
                    let walked = h.commit_count();
                    (h, BuildStats { incremental: false, walked, reason: None })
                }),
                Some(old) => old.update(&self.root, &head, &opts, &self.cancel),
            };
            let cancelled = result.as_ref().err().is_some_and(is_cancelled);
            self.end(cancelled);
            match result {
                Ok((h, st)) => {
                    let counter = if st.incremental { &self.counters.updates } else { &self.counters.builds };
                    counter.fetch_add(1, Ordering::Relaxed);
                    let build_ms = t.elapsed().as_millis() as u64;
                    if let Err(e) = h.save(&sidecar) {
                        eprintln!("ripple: could not write the history cache {}: {e:#}", sidecar.display());
                    }
                    if debug {
                        eprintln!("ripple: history at {} ({} commits, walked {}, incremental {}) in {build_ms} ms", h.head(), h.commit_count(), st.walked, st.incremental);
                    }
                    let note = st.reason.or(info.note.take());
                    info = BuildInfo { from_sidecar: info.from_sidecar, incremental: st.incremental, walked: st.walked, build_ms, finished_unix: now_unix(), sidecar_bytes: file_len(&sidecar), note };
                    let h = Arc::new(h);
                    self.publish_ready(&h, &sidecar, &info, false);
                    cur = Some(h);
                }
                Err(_) if cancelled => {
                    self.counters.cancelled.fetch_add(1, Ordering::Relaxed);
                    if self.stopping() {
                        return;
                    }
                    if cur.is_some() {
                        self.set_updating(false);
                    }
                    // Cancelled by a newer HEAD move: the next wait picks it
                    // up after the debounce. (Otherwise retry at once.)
                    first = lock(&self.ctl).nudges == seen;
                }
                Err(e) => {
                    eprintln!("ripple: history build failed: {e:#}");
                    match &cur {
                        None => self.publish(View::new(State::Unavailable(format!("build failed: {e:#}")))),
                        Some(_) => self.set_updating(false),
                    }
                }
            }
        }
    }

    fn publish_ready(&self, h: &Arc<History>, sidecar: &Path, info: &BuildInfo, updating: bool) {
        if self.stopping() {
            return;
        }
        self.publish(View {
            state: State::Ready,
            history: Some(h.clone()),
            assessment: Some(assess(h)),
            updating,
            behind_head: false,
            sidecar: Some(sidecar.to_path_buf()),
            build: info.clone(),
        });
    }

    /// The `history` object of `stats`, with the thread's counters.
    pub fn stats_json(&self) -> Value {
        let mut v = self.view().stats_json();
        v["counters"] = json!({
            "builds": self.counters.builds.load(Ordering::Relaxed),
            "updates": self.counters.updates.load(Ordering::Relaxed),
            "cancelled": self.counters.cancelled.load(Ordering::Relaxed),
            "head_moves": self.counters.nudges.load(Ordering::Relaxed),
            "thread": self.thread_running(),
        });
        v
    }
}

fn file_len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// Is a watcher path (relative to the root) a HEAD or branch ref change?
pub fn is_head_event(rel: &str) -> bool {
    rel == ".git/HEAD" || rel == ".git/HEAD.lock" || rel.starts_with(".git/refs/heads/") || rel.starts_with(".git/packed-refs")
}

// ---------------------------------------------------------------------------
// One-shot mode and `ripple history`

/// The history one-shot mode (`--no-daemon`) uses: the sidecar if it is
/// usable, never a build.
pub fn oneshot_view(root: &Path, cfg: &Config) -> View {
    if !cfg.history.enabled {
        return View::new(State::Disabled);
    }
    let dirs = match GitDirs::probe(root) {
        Ok(d) if d.shallow => return View::new(State::Unavailable(SHALLOW.into())),
        Ok(d) => d,
        Err(r) => return View::new(State::Unavailable(r)),
    };
    let sidecar = dirs.sidecar();
    let rebuild = "run `ripple history build`";
    let h = match History::load(&sidecar) {
        Ok(h) => h,
        Err(_) if !sidecar.exists() => return View::new(State::Unavailable(format!("no history cache; {rebuild}"))),
        Err(e) => return View::new(State::Unavailable(format!("history cache unreadable ({e:#}); {rebuild}"))),
    };
    if h.options() != &options_for(cfg) {
        return View::new(State::Unavailable(format!("history cache was built with other settings; {rebuild}")));
    }
    let head = match head_of(root) {
        Ok(h) => h,
        Err(r) => return View::new(State::Unavailable(r)),
    };
    let behind = h.head() != head;
    if behind && !ripple_history::is_ancestor(root, h.head(), &head).unwrap_or(false) {
        return View::new(State::Unavailable(format!("history cache is for another line of history; {rebuild}")));
    }
    let h = Arc::new(h);
    View {
        state: State::Ready,
        assessment: Some(assess(&h)),
        history: Some(h),
        updating: false,
        behind_head: behind,
        build: BuildInfo { from_sidecar: true, sidecar_bytes: file_len(&sidecar), ..BuildInfo::default() },
        sidecar: Some(sidecar),
    }
}

/// `ripple history build`: brings the sidecar up to HEAD (or `at`) in the foreground.
pub fn cli_build(root: &Path, at: Option<&str>) -> Result<u8> {
    let cfg = load_config(root);
    if !cfg.history.enabled {
        println!("history is disabled (history.enabled: false in ripple.yml)");
        return Ok(0);
    }
    let dirs = GitDirs::probe(root).map_err(|r| anyhow::anyhow!("{r}"))?;
    if dirs.shallow {
        bail!("{SHALLOW}");
    }
    let sidecar = dirs.sidecar();
    let head = match at {
        Some(rev) => ripple_history::resolve_rev(root, rev)?,
        None => head_of(root).map_err(|r| anyhow::anyhow!("{r}"))?,
    };
    let opts = options_for(&cfg);
    let no = AtomicBool::new(false);
    let t = Instant::now();
    let (h, how) = match History::load(&sidecar) {
        Ok(old) => {
            let (h, st) = old.update(root, &head, &opts, &no)?;
            let how = if st.incremental { format!("updated ({} new commits)", st.walked) } else { format!("rebuilt ({})", st.reason.unwrap_or_default()) };
            (h, how)
        }
        Err(e) => {
            let how = if sidecar.exists() { format!("rebuilt (cache unreadable: {e:#})") } else { "built".to_string() };
            (History::build(root, &head, &opts, &no)?, how)
        }
    };
    h.save(&sidecar).with_context(|| format!("writing {}", sidecar.display()))?;
    let ms = t.elapsed().as_millis();
    println!("{how} in {ms} ms: {}", sidecar.display());
    print_summary(&h, file_len(&sidecar));
    Ok(0)
}

fn print_summary(h: &History, bytes: u64) {
    let a = assess(h);
    println!(
        "head {}  commits {} ({} sweeps, {:.1}%)  files {}  pairs {}  sidecar {:.1} MB  memory {:.1} MB",
        h.head(),
        a.commits,
        h.sweep_commit_count(),
        a.sweep_share * 100.0,
        h.file_count(),
        a.pairs,
        bytes as f64 / 1e6,
        h.heap_bytes() as f64 / 1e6
    );
    println!("churn: {}", if a.churn { "on" } else { "off" });
    match &a.cochange {
        Ok(()) => println!("co-change: on"),
        Err(r) => println!("co-change: off ({r})"),
    }
}

/// `ripple history status`: the daemon's view if one is running, else the sidecar's.
pub fn cli_status(root: &Path, daemon: Option<&Value>) -> Result<u8> {
    if let Some(v) = daemon {
        println!("{}", serde_json::to_string_pretty(v)?);
        return Ok(0);
    }
    let cfg = load_config(root);
    let view = oneshot_view(root, &cfg);
    match &view.state {
        State::Ready => {
            let h = view.history.as_ref().expect("ready has a history");
            println!("ready{} (no daemon running): {}", if view.behind_head { ", behind HEAD" } else { "" }, view.sidecar.as_ref().map(|p| p.display().to_string()).unwrap_or_default());
            print_summary(h, view.build.sidecar_bytes);
        }
        s => println!("{}{}", s.name(), s.reason().map(|r| format!(": {r}")).unwrap_or_default()),
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Options {
        Options { max_files_per_commit: 5, ..Options::default() }
    }

    /// `n` commits: every 4th touches `a` and `b` (plus `x{i}`), the rest one
    /// file each; `sweeps` extra commits of 10 files.
    fn history(n: usize, sweeps: usize) -> History {
        let names: Vec<String> = (0..n + sweeps).map(|i| format!("x{i}")).collect();
        let wide: Vec<String> = (0..10).map(|i| format!("w{i}")).collect();
        let mut files: Vec<Vec<&str>> = Vec::new();
        for i in 0..n {
            files.push(if i % 4 == 0 { vec!["a", "b", &names[i]] } else { vec![&names[i]] });
        }
        for _ in 0..sweeps {
            files.push(wide.iter().map(String::as_str).collect());
        }
        let commits: Vec<(String, i64)> = (0..files.len()).map(|i| (format!("c{i}"), 1_700_000_000 - i as i64 * 3600)).collect();
        let c: Vec<(&str, i64, &[&str])> = commits.iter().zip(&files).map(|((o, t), f)| (o.as_str(), *t, f.as_slice())).collect();
        History::from_commits("h", &opts(), &c)
    }

    #[test]
    fn auto_off_thresholds() {
        let a = assess(&history(400, 0));
        assert_eq!((a.churn, a.cochange.is_ok()), (true, true), "{a:?}");
        let a = assess(&history(200, 0));
        assert!(a.cochange.as_ref().unwrap_err().contains("too short"), "{a:?}");
        assert!(a.churn);
        // 400 + 200 sweeps: 33% > 25%.
        let a = assess(&history(400, 200));
        assert!(a.cochange.as_ref().unwrap_err().contains("low-signal"), "{a:?}");
        assert!(a.churn);
        let a = assess(&History::from_commits("h", &opts(), &[]));
        assert_eq!((a.churn, a.cochange.is_ok()), (false, false));
    }

    fn ready(h: History) -> View {
        let h = Arc::new(h);
        View { assessment: Some(assess(&h)), history: Some(h), ..View::new(State::Ready) }
    }

    #[test]
    fn lookup_reports_churn_and_partners() {
        let v = ready(history(400, 0));
        let q = PartnerQuery::default();
        let info = v.lookup(&["a"], &q, |_| false);
        assert_eq!(info.state, "ready");
        assert_eq!(info.churn["a"].commits, 100);
        assert_eq!(info.usually_changes_with.len(), 1);
        let p = &info.usually_changes_with[0];
        assert_eq!((p.path.as_str(), p.via.as_str(), p.together, p.of), ("b", "a", 100, 100));
        // Excluded (in the diff or a direct graph neighbour): not shown.
        assert!(v.lookup(&["a"], &q, |p| p == "b").usually_changes_with.is_empty());
        // Co-change off: churn only.
        let v = ready(history(400, 200));
        let info = v.lookup(&["a"], &q, |_| false);
        assert!(!info.cochange && info.usually_changes_with.is_empty() && info.churn["a"].commits == 100);
        let j = serde_json::to_value(&info).unwrap();
        assert!(j.get("usually_changes_with").is_none() && j["cochange_reason"].is_string());
    }

    #[test]
    fn head_events() {
        for p in [".git/HEAD", ".git/HEAD.lock", ".git/refs/heads/main", ".git/refs/heads/a/b.lock", ".git/packed-refs"] {
            assert!(is_head_event(p), "{p}");
        }
        for p in [".git/index", ".git/refs/remotes/origin/main", ".git/logs/HEAD", "HEAD", "src/.git/HEAD"] {
            assert!(!is_head_event(p), "{p}");
        }
    }
}
