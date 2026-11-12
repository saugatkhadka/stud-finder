//! Coverage and the hotspot ranking inside the daemon, and the per-query
//! signal lookup that feeds [`ripple_policy::SignalInput`].
//!
//! # Coverage ([`CoverageD`])
//!
//! ```text
//!   start ── coverage.enabled: false ──────────▶ disabled
//!     ▼
//!   loading ── no report found ────────────────▶ unavailable (re-checked)
//!     │ ── every report unreadable ────────────▶ unavailable(error) (retried after a minute)
//!     ▼
//!   ready ── report added / changed / removed ─▶ ready (new load swapped in)
//! ```
//!
//! One low-priority thread per daemon, asleep on a condvar. It wakes when
//! the file watcher reports a path under `coverage/` (or a configured
//! report path), when a query finds the last check [`CHECK_INTERVAL`] old
//! (reports outside the work tree are not watched), and on `reload`. A
//! wake-up only `stat`s the report files ([`Coverage::sources_changed`]);
//! a report is parsed only when one appeared or changed, and the new
//! [`Coverage`] is swapped in behind an `Arc`. Queries never parse, wait
//! or stat: they clone the published [`CovView`]. ripple never runs tests.
//!
//! One-shot mode ([`CovView::load`]) reads the reports inline, and only
//! when there are any (discovery is a few `stat`s).
//!
//! # Hotspot ranking ([`RankD`], [`Ranking`])
//!
//! See [`Ranking::compute`] for the formula. A ranking is computed once per
//! (index generation, history head, coverage load) and cached. `impact`
//! never computes one: it uses the cached ranking, even an older one, and
//! starts a background recompute when the key moved (at most one at a
//! time, at most every [`RANK_MIN_INTERVAL`]). `rank` computes in the
//! foreground when the cache is out of date; `file` only when there is none.

use crate::engine::{FileSet, Snapshot};
use crate::historyd::{HistoryInfo, State as HistState, View};
use ripple_core::config::{CoverageConfig, RankWeights};
use ripple_coverage::Coverage;
use ripple_history::PartnerQuery;
use ripple_policy::{ChangedFile, ChurnMetric, CoChangeEntry, CoverageMetric, FileSignals, FileStatus, HotspotMetric, Language, SignalInput, SignalState, SignalStatus, Signals, cochange_why, round1};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

/// A query re-checks the coverage reports at most this often.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// A failed load is retried no sooner than this.
const RETRY_FAILED: Duration = Duration::from_secs(60);
/// Background rank recomputes start at most this often.
pub const RANK_MIN_INTERVAL: Duration = Duration::from_secs(2);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Utility QoS on macOS, nice 10 on Linux, for this thread.
fn lower_priority() {
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
// Coverage

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CovState {
    Disabled,
    Loading,
    Ready,
    Unavailable(String),
}

/// One published coverage state. Immutable.
#[derive(Debug, Clone)]
pub struct CovView {
    pub state: CovState,
    /// Present when ready, and as an empty value when no report was found
    /// (so a report that appears is noticed).
    pub coverage: Option<Arc<Coverage>>,
    /// Increases with every load; part of the ranking's cache key.
    pub load_id: u64,
    /// The HEAD commit's committer time when loaded, for staleness.
    pub head_time: Option<i64>,
    pub load_ms: u64,
}

impl CovView {
    fn new(state: CovState) -> Self {
        Self { state, coverage: None, load_id: 0, head_time: None, load_ms: 0 }
    }

    pub fn disabled() -> Self {
        Self::new(CovState::Disabled)
    }

    /// Reads the reports `cfg` points at (no thread; one-shot mode and the
    /// daemon's loader). Cheap when there are none.
    pub fn load(root: &Path, cfg: &CoverageConfig, known: &FileSet, load_id: u64) -> Self {
        if !cfg.enabled {
            return Self::disabled();
        }
        let t = Instant::now();
        let mut v = match Coverage::load(root, cfg, known.iter()) {
            Ok(c) if c.reports().is_empty() => {
                let where_ = if cfg.paths.is_empty() { "coverage/, tmp/coverage/, {spec,test,frontend,client,web}/coverage/, app/{javascript,frontend}/coverage/ (.resultset.json, lcov.info, lcov/*.lcov, coverage-final.json)".to_string() } else { format!("coverage.paths ({})", cfg.paths.join(", ")) };
                Self { coverage: Some(Arc::new(c)), ..Self::new(CovState::Unavailable(format!("no coverage report found in {where_}"))) }
            }
            Ok(c) => {
                let head_time = head_commit_time(root);
                Self { state: CovState::Ready, coverage: Some(Arc::new(c)), load_id, head_time, load_ms: 0 }
            }
            Err(e) => Self::new(CovState::Unavailable(format!("{e:#}"))),
        };
        v.load_id = load_id;
        v.load_ms = t.elapsed().as_millis() as u64;
        v
    }

    pub fn status(&self) -> SignalStatus {
        match &self.state {
            CovState::Disabled => SignalStatus::new(SignalState::Disabled, None),
            CovState::Loading => SignalStatus::new(SignalState::Loading, Some("reading coverage reports".to_string())),
            CovState::Unavailable(r) => SignalStatus::new(SignalState::Unavailable, Some(r.clone())),
            CovState::Ready => {
                let c = self.coverage.as_ref();
                let caveat = c.filter(|c| c.file_count() == 0).map(|_| "no file in the coverage report matches this repository".to_string());
                SignalStatus::new(SignalState::Ready, caveat)
            }
        }
    }

    /// Ready coverage data.
    pub fn ready(&self) -> Option<&Coverage> {
        (self.state == CovState::Ready).then_some(self.coverage.as_deref()).flatten()
    }

    pub fn stats_json(&self) -> Value {
        let st = self.status();
        let mut v = json!({"state": st.state.as_str(), "reason": st.reason, "load_id": self.load_id, "load_ms": self.load_ms});
        if let Some(c) = self.ready() {
            let t = c.totals();
            v["files"] = json!(c.file_count());
            v["percent"] = json!(t.percent().map(round1));
            v["unmapped"] = json!(c.unmapped());
            v["heap_bytes"] = json!(c.heap_bytes());
            v["measured_at"] = json!(c.measured_at());
            v["reports"] = json!(c.reports().iter().map(|r| json!({"path": r.path, "format": format!("{:?}", r.format).to_lowercase(), "entries": r.entries, "mapped": r.mapped, "error": r.error})).collect::<Vec<_>>());
        }
        v
    }
}

/// `git log -1 --format=%ct HEAD`.
fn head_commit_time(root: &Path) -> Option<i64> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["log", "-1", "--format=%ct", "HEAD"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

type FilesFn = Box<dyn Fn() -> Option<FileSet> + Send + Sync>;

struct CovCtl {
    cfg: CoverageConfig,
    /// Re-check the report files.
    check: bool,
    /// Reload whatever the files say (config changed).
    force: bool,
    shutdown: bool,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// The daemon's coverage loader (see the module docs).
pub struct CoverageD {
    root: PathBuf,
    view: RwLock<Arc<CovView>>,
    ctl: Mutex<CovCtl>,
    cv: Condvar,
    started: Instant,
    /// Milliseconds after `started` of the last query-triggered check.
    last_check_ms: AtomicU64,
    pub loads: AtomicU64,
    pub checks: AtomicU64,
}

impl CoverageD {
    /// Not started yet: see [`CoverageD::start`].
    pub fn new(root: &Path, cfg: &CoverageConfig) -> Arc<Self> {
        let state = if cfg.enabled { CovState::Loading } else { CovState::Disabled };
        Arc::new(Self {
            root: root.to_path_buf(),
            view: RwLock::new(Arc::new(CovView::new(state))),
            ctl: Mutex::new(CovCtl { cfg: cfg.clone(), check: true, force: true, shutdown: false, thread: None }),
            cv: Condvar::new(),
            started: Instant::now(),
            last_check_ms: AtomicU64::new(0),
            loads: AtomicU64::new(0),
            checks: AtomicU64::new(0),
        })
    }

    /// Starts the loader thread. `files` gives the workspace's files once
    /// the first index exists (report paths are mapped onto them).
    pub fn start(self: &Arc<Self>, files: FilesFn) {
        let me = self.clone();
        let t = std::thread::Builder::new().name("ripple-coverage".into()).spawn(move || me.run(files));
        match t {
            Ok(t) => lock(&self.ctl).thread = Some(t),
            Err(e) => self.publish(CovView::new(CovState::Unavailable(format!("could not start the coverage thread: {e}")))),
        }
    }

    pub fn view(&self) -> Arc<CovView> {
        self.view.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn publish(&self, v: CovView) {
        *self.view.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(v);
    }

    /// A report may have changed (watcher event). Cheap.
    pub fn nudge(&self) {
        let mut c = lock(&self.ctl);
        c.check = true;
        self.cv.notify_all();
    }

    /// Called by queries: nudges at most every [`CHECK_INTERVAL`]. One
    /// atomic load on the fast path.
    pub fn query_tick(&self) {
        let now = self.started.elapsed().as_millis() as u64;
        let last = self.last_check_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= CHECK_INTERVAL.as_millis() as u64 && self.last_check_ms.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
            self.nudge();
        }
    }

    /// Applies `coverage:` from `ripple.yml` (on `reload`).
    pub fn configure(&self, cfg: &CoverageConfig) {
        let mut c = lock(&self.ctl);
        if c.cfg != *cfg {
            c.cfg = cfg.clone();
            c.force = true;
            c.check = true;
            self.cv.notify_all();
        }
    }

    /// True when `rel` (repo-relative) may be a coverage report.
    pub fn is_report_path(&self, rel: &str) -> bool {
        if rel.starts_with("coverage/") || rel == "coverage" {
            return true;
        }
        let c = lock(&self.ctl);
        c.cfg.paths.iter().any(|p| {
            let p = p.trim_start_matches("./");
            let dir = p.rsplit_once('/').map_or("", |(d, _)| d);
            rel == p || rel.starts_with(&format!("{p}/")) || (p.contains('*') && (dir.is_empty() || rel.starts_with(&format!("{dir}/"))))
        })
    }

    pub fn shutdown(&self) {
        let mut c = lock(&self.ctl);
        c.shutdown = true;
        self.cv.notify_all();
    }

    pub fn join(&self) {
        let t = lock(&self.ctl).thread.take();
        if let Some(t) = t {
            let _ = t.join();
        }
    }

    fn run(&self, files: FilesFn) {
        lower_priority();
        // Report paths are mapped onto the workspace's files: wait for the first index.
        let known = loop {
            if let Some(f) = files() {
                break f;
            }
            let c = lock(&self.ctl);
            if c.shutdown {
                return;
            }
            let _ = self.cv.wait_timeout(c, Duration::from_millis(50));
        };
        let mut known = known;
        let mut failed_at: Option<Instant> = None;
        loop {
            let (cfg, force) = {
                let mut c = lock(&self.ctl);
                while !c.check && !c.shutdown {
                    c = self.cv.wait(c).unwrap_or_else(|p| p.into_inner());
                }
                if c.shutdown {
                    return;
                }
                c.check = false;
                (c.cfg.clone(), std::mem::take(&mut c.force))
            };
            self.checks.fetch_add(1, Ordering::Relaxed);
            let cur = self.view();
            if !force {
                if !cfg.enabled {
                    continue;
                }
                let changed = match &cur.coverage {
                    Some(c) => c.sources_changed(&self.root),
                    // The last load failed.
                    None => failed_at.is_none_or(|t| t.elapsed() >= RETRY_FAILED),
                };
                if !changed {
                    continue;
                }
            }
            if !cfg.enabled {
                self.publish(CovView::disabled());
                continue;
            }
            if let Some(f) = files() {
                known = f;
            }
            let id = self.loads.fetch_add(1, Ordering::Relaxed) + 1;
            let v = CovView::load(&self.root, &cfg, &known, id);
            failed_at = v.coverage.is_none().then(Instant::now);
            self.publish(v);
        }
    }

    pub fn stats_json(&self) -> Value {
        let mut v = self.view().stats_json();
        v["loads"] = json!(self.loads.load(Ordering::Relaxed));
        v["checks"] = json!(self.checks.load(Ordering::Relaxed));
        v
    }
}

// ---------------------------------------------------------------------------
// Ranking

/// What a ranking was computed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankKey {
    pub generation: u64,
    pub history_head: Option<String>,
    pub history_state: &'static str,
    pub coverage_load: u64,
    pub coverage_state: &'static str,
    /// `rank:` of ripple.yml (as bits: a policy reload changes it without a new index).
    pub config: [u64; 5],
}

impl RankKey {
    pub fn of(snap: &Snapshot, hist: &View, cov: &CovView) -> Self {
        Self {
            generation: snap.generation,
            history_head: hist.history.as_ref().map(|h| h.head().to_string()),
            history_state: hist.state.name(),
            coverage_load: cov.load_id,
            coverage_state: cov.status().state.as_str(),
            config: {
                let r = &snap.policy.config.rank;
                [r.hotspot_percent, r.weights.churn, r.weights.complexity, r.weights.dependents, r.weights.uncovered].map(f64::to_bits)
            },
        }
    }
}

/// The 0..1 inputs of a file's score (see [`Ranking::compute`]).
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Factors {
    pub churn: f64,
    pub complexity: f64,
    pub dependents: f64,
    /// `1 - coverage`; absent when coverage is unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uncovered: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RankRow {
    pub rank: u32,
    pub path: String,
    pub language: Language,
    /// 0..100.
    pub score: f64,
    /// Share of ranked files with a lower score, percent.
    pub percentile: f64,
    pub factors: Factors,
    pub commits: u32,
    pub recent_commits: u32,
    pub complexity: u32,
    pub max_method: u32,
    pub methods: u32,
    pub lines: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<f64>,
    pub dependents: usize,
    #[serde(skip)]
    pub areas: Vec<u16>,
    /// One line naming the factors that put it here.
    pub reason: String,
}

/// Every non-test Ruby / JS / TS file of the index, riskiest first.
#[derive(Debug, Clone)]
pub struct Ranking {
    pub key: RankKey,
    pub rows: Vec<RankRow>,
    pos: HashMap<String, u32>,
    /// Churn was available (else its weight is 0).
    pub churn: bool,
    /// Coverage was available for some file.
    pub coverage: bool,
    pub hotspot_percent: f64,
    pub compute_us: u64,
}

/// The rank formula for these weights, also printed by `ripple rank --format json`.
pub fn rank_formula(w: &RankWeights) -> String {
    let n = |x: f64| ripple_policy::fmt_pct(x);
    format!(
        "score = 100 * ({}*churn + {}*complexity + {}*dependents + {}*uncovered) / (sum of the weights used); \
churn = percentile of (commits in the last 90 days, then all commits in the history window); \
complexity = mean(percentile of file complexity, percentile of the most complex method), a file without methods counting 0 for both; \
dependents = percentile of direct non-test dependents (fan-in); uncovered = 1 - line coverage, only when the file is in a coverage report \
(else that weight is dropped; churn's too when git history is not available). \
percentile(x) = share of ranked files with a strictly smaller value. Ranked: non-test Ruby and JS/TS files of the index. \
Weights: rank.weights in ripple.yml. hotspot: rank <= ceil(files * rank.hotspot_percent / 100).",
        n(w.churn),
        n(w.complexity),
        n(w.dependents),
        n(w.uncovered)
    )
}

/// `p(x)` = share of `sorted` strictly below `x`.
fn pct_below<T: Ord>(sorted: &[T], x: &T) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted.partition_point(|v| v < x) as f64 / sorted.len() as f64
}

/// "top 3%" for a 0..1 percentile.
fn top(p: f64) -> String {
    format!("top {}%", ((1.0 - p) * 100.0).ceil().max(1.0) as u32)
}

impl Ranking {
    /// Scores every candidate file. The formula is [`rank_formula`]:
    /// percentile ranks (not raw products) so one huge value cannot dominate
    /// and the factors are comparable; method complexity guards against a
    /// file without methods whose decisions are all `||=` memoisation.
    pub fn compute(snap: &Snapshot, hist: &View, cov: &CovView) -> Ranking {
        let t = Instant::now();
        let key = RankKey::of(snap, hist, cov);
        let g = &snap.graph;
        let h = hist.history.as_deref().filter(|_| hist.assessment.as_ref().is_some_and(|a| a.churn));
        let c = cov.ready();
        let w = snap.policy.config.rank.weights;
        struct Cand {
            node: u32,
            churn: (u32, u32),
            cx: u32,
            mx: u32,
            fm: ripple_core::metrics::FileMetrics,
            deps: usize,
            cov: Option<f64>,
        }
        let mut cands = Vec::new();
        for node in 0..g.node_count() as u32 {
            let Some(class) = snap.index.get(node) else { continue };
            if class.test || !matches!(class.language, Language::Ruby | Language::Js) {
                continue;
            }
            let Some(fm) = g.metrics(node) else { continue };
            let path = g.path(node);
            let ch = h.and_then(|h| h.churn(path));
            let (cx, mx) = if fm.methods == 0 { (0, 0) } else { (fm.complexity, u32::from(fm.max_method)) };
            let cv = c.and_then(|c| c.file(path)).and_then(|f| f.percent()).map(|p| p / 100.0);
            // Distinct non-test dependents: a well-tested file is not riskier for its specs.
            let deps = g.dependents(node).map(|(d, _)| d).filter(|&d| !snap.index.get(d).is_some_and(|c| c.test)).collect::<HashSet<_>>().len();
            cands.push(Cand { node, churn: ch.map_or((0, 0), |c| (c.recent_changes, c.changes)), cx, mx, fm, deps, cov: cv });
        }
        let sorted = |f: &dyn Fn(&Cand) -> u64| {
            let mut v: Vec<u64> = cands.iter().map(f).collect();
            v.sort_unstable();
            v
        };
        let churn_v = sorted(&|c| (u64::from(c.churn.0) << 32) | u64::from(c.churn.1));
        let cx_v = sorted(&|c| u64::from(c.cx));
        let mx_v = sorted(&|c| u64::from(c.mx));
        let deps_v = sorted(&|c| c.deps as u64);
        let mut rows: Vec<RankRow> = cands
            .iter()
            .map(|c| {
                let pch = pct_below(&churn_v, &((u64::from(c.churn.0) << 32) | u64::from(c.churn.1)));
                let pcx = pct_below(&cx_v, &u64::from(c.cx));
                let pmx = pct_below(&mx_v, &u64::from(c.mx));
                let pdeps = pct_below(&deps_v, &(c.deps as u64));
                let f = Factors { churn: pch, complexity: (pcx + pmx) / 2.0, dependents: pdeps, uncovered: c.cov.map(|v| 1.0 - v) };
                let (mut num, mut den) = (w.complexity * f.complexity + w.dependents * f.dependents, w.complexity + w.dependents);
                if h.is_some() {
                    num += w.churn * f.churn;
                    den += w.churn;
                }
                if let Some(u) = f.uncovered {
                    num += w.uncovered * u;
                    den += w.uncovered;
                }
                let path = g.path(c.node).to_string();
                let class = snap.index.get(c.node).cloned().unwrap_or_default();
                let mut why = Vec::new();
                if h.is_some() {
                    why.push(format!("churn {}/90d, {} total ({})", c.churn.0, c.churn.1, top(pch)));
                }
                if c.fm.methods == 0 {
                    why.push(format!("cx {} but no methods (counts 0)", c.fm.complexity));
                } else {
                    why.push(format!("cx {} / max method {} ({})", c.cx, c.mx, top(f.complexity)));
                }
                why.push(format!("{} non-test dependents ({})", c.deps, top(pdeps)));
                match c.cov {
                    Some(v) => why.push(format!("cov {}%", ripple_policy::fmt_pct(v * 100.0))),
                    None if cov.ready().is_some() => why.push("cov unknown".to_string()),
                    None => {}
                }
                RankRow {
                    rank: 0,
                    path,
                    language: class.language,
                    score: if den > 0.0 { round1(100.0 * num / den) } else { 0.0 },
                    percentile: 0.0,
                    factors: Factors { churn: round3(f.churn), complexity: round3(f.complexity), dependents: round3(f.dependents), uncovered: f.uncovered.map(round3) },
                    commits: c.churn.1,
                    recent_commits: c.churn.0,
                    complexity: c.fm.complexity,
                    max_method: u32::from(c.fm.max_method),
                    methods: u32::from(c.fm.methods),
                    lines: c.fm.lines,
                    coverage: c.cov.map(|v| round1(v * 100.0)),
                    dependents: c.deps,
                    areas: class.areas.clone(),
                    reason: why.join(" · "),
                }
            })
            .collect();
        rows.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
        let scores: Vec<u64> = {
            let mut v: Vec<u64> = rows.iter().map(|r| (r.score * 10.0) as u64).collect();
            v.sort_unstable();
            v
        };
        for (i, r) in rows.iter_mut().enumerate() {
            r.rank = i as u32 + 1;
            r.percentile = round1(100.0 * pct_below(&scores, &((r.score * 10.0) as u64)));
        }
        let pos = rows.iter().enumerate().map(|(i, r)| (r.path.clone(), i as u32)).collect();
        Ranking {
            key,
            rows,
            pos,
            churn: h.is_some(),
            coverage: c.is_some_and(|c| c.file_count() > 0),
            hotspot_percent: snap.policy.config.rank.hotspot_percent,
            compute_us: t.elapsed().as_micros() as u64,
        }
    }

    pub fn row(&self, path: &str) -> Option<&RankRow> {
        self.pos.get(path).map(|&i| &self.rows[i as usize])
    }

    /// Ranks `1..=cutoff` are hotspots.
    pub fn cutoff(&self) -> u32 {
        ((self.rows.len() as f64) * self.hotspot_percent / 100.0).ceil() as u32
    }

    pub fn hotspot(&self, path: &str) -> Option<HotspotMetric> {
        let r = self.row(path)?;
        Some(HotspotMetric { rank: r.rank, of: self.rows.len() as u32, percentile: r.percentile, hotspot: r.rank <= self.cutoff() })
    }
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// The cached ranking (see the module docs).
#[derive(Default)]
pub struct RankD {
    cur: RwLock<Option<Arc<Ranking>>>,
    running: AtomicBool,
    last_start: Mutex<Option<Instant>>,
    pub computes: AtomicU64,
}

impl RankD {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn cached(&self) -> Option<Arc<Ranking>> {
        self.cur.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn store(&self, r: Arc<Ranking>) {
        let mut cur = self.cur.write().unwrap_or_else(|p| p.into_inner());
        // Never replace a ranking of a newer index with an older one.
        if cur.as_ref().is_none_or(|c| c.key.generation <= r.key.generation) {
            *cur = Some(r);
        }
    }

    /// Up to date for this key, computing in the foreground if needed.
    pub fn get_fresh(&self, snap: &Snapshot, hist: &View, cov: &CovView) -> Arc<Ranking> {
        let key = RankKey::of(snap, hist, cov);
        if let Some(r) = self.cached().filter(|r| r.key == key) {
            return r;
        }
        self.computes.fetch_add(1, Ordering::Relaxed);
        let r = Arc::new(Ranking::compute(snap, hist, cov));
        self.store(r.clone());
        r
    }

    /// The cached ranking, computing in the foreground only when there is none.
    pub fn get_any(&self, snap: &Snapshot, hist: &View, cov: &CovView) -> Arc<Ranking> {
        match self.cached() {
            Some(r) => r,
            None => self.get_fresh(snap, hist, cov),
        }
    }

    /// The cached ranking (possibly of an older key) without waiting; when
    /// the key moved, starts a background recompute.
    pub fn get_cached(self: &Arc<Self>, snap: &Arc<Snapshot>, hist: &Arc<View>, cov: &Arc<CovView>) -> Option<Arc<Ranking>> {
        let cur = self.cached();
        let key = RankKey::of(snap, hist, cov);
        if cur.as_ref().is_none_or(|r| r.key != key) {
            self.kick(snap.clone(), hist.clone(), cov.clone());
        }
        cur
    }

    fn kick(self: &Arc<Self>, snap: Arc<Snapshot>, hist: Arc<View>, cov: Arc<CovView>) {
        {
            let mut last = lock(&self.last_start);
            if last.is_some_and(|t| t.elapsed() < RANK_MIN_INTERVAL) && self.cached().is_some() {
                return;
            }
            if self.running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
                return;
            }
            *last = Some(Instant::now());
        }
        let me = self.clone();
        let spawned = std::thread::Builder::new().name("ripple-rank".into()).spawn(move || {
            lower_priority();
            me.computes.fetch_add(1, Ordering::Relaxed);
            let r = Arc::new(Ranking::compute(&snap, &hist, &cov));
            me.store(r);
            me.running.store(false, Ordering::SeqCst);
        });
        if spawned.is_err() {
            self.running.store(false, Ordering::SeqCst);
        }
    }

    pub fn stats_json(&self) -> Value {
        let c = self.cached();
        json!({
            "computes": self.computes.load(Ordering::Relaxed),
            "running": self.running.load(Ordering::Relaxed),
            "files": c.as_ref().map(|r| r.rows.len()),
            "generation": c.as_ref().map(|r| r.key.generation),
            "compute_us": c.as_ref().map(|r| r.compute_us),
        })
    }
}

// ---------------------------------------------------------------------------
// Per-query lookup

/// Where a query finds the non-graph signals.
pub struct Sources {
    pub history: Arc<View>,
    pub coverage: Arc<CovView>,
    pub rank: Arc<RankD>,
    /// Daemon: `impact` never computes a ranking in the foreground.
    pub background_rank: bool,
}

impl Sources {
    /// Nothing but complexity (tests, and callers without signals).
    pub fn none() -> Self {
        Self { history: Arc::new(crate::historyd::disabled_view()), coverage: Arc::new(CovView::disabled()), rank: RankD::new(), background_rank: false }
    }

    /// One-shot mode: the history sidecar and coverage reports, read now.
    pub fn oneshot(snap: &Snapshot) -> Self {
        let cfg = &snap.policy.config;
        let history = Arc::new(crate::historyd::oneshot_view(&snap.root, cfg));
        let coverage = Arc::new(CovView::load(&snap.root, &cfg.coverage, &snap.files, 1));
        Self { history, coverage, rank: RankD::new(), background_rank: false }
    }

    pub fn history_status(&self) -> (SignalStatus, SignalStatus) {
        let v = &self.history;
        let from_state = |s: &HistState| match s {
            HistState::Disabled => SignalStatus::new(SignalState::Disabled, None),
            HistState::Building => SignalStatus::new(SignalState::Building, Some("reading git history".to_string())),
            HistState::Unavailable(r) => SignalStatus::new(SignalState::Unavailable, Some(r.clone())),
            HistState::Ready => SignalStatus::ready(),
        };
        let base = from_state(&v.state);
        if !base.is_ready() {
            return (base.clone(), base);
        }
        let a = v.assessment.as_ref();
        let churn = if a.is_some_and(|a| a.churn) { SignalStatus::ready() } else { SignalStatus::new(SignalState::Unavailable, Some("no commits in the history window".to_string())) };
        let cochange = match a.map(|a| &a.cochange) {
            Some(Ok(())) => SignalStatus::ready(),
            Some(Err(r)) => SignalStatus::new(SignalState::Unavailable, Some(r.clone())),
            None => SignalStatus::new(SignalState::Unavailable, Some("no history".to_string())),
        };
        (churn, cochange)
    }

    /// The ranking for this snapshot, as `mode` allows, and the hotspot status.
    pub fn ranking(&self, snap: &Arc<Snapshot>, mode: RankMode) -> (Option<Arc<Ranking>>, SignalStatus) {
        if self.history.state == HistState::Building {
            return (None, SignalStatus::new(SignalState::Building, Some("waiting for git history".to_string())));
        }
        if self.coverage.state == CovState::Loading {
            return (None, SignalStatus::new(SignalState::Building, Some("waiting for coverage".to_string())));
        }
        let r = match mode {
            RankMode::Cached if self.background_rank => self.rank.get_cached(snap, &self.history, &self.coverage),
            RankMode::Cached | RankMode::Any => Some(self.rank.get_any(snap, &self.history, &self.coverage)),
            RankMode::Fresh => Some(self.rank.get_fresh(snap, &self.history, &self.coverage)),
        };
        match r {
            None => (None, SignalStatus::new(SignalState::Building, Some("ranking is being computed".to_string()))),
            Some(r) if r.rows.is_empty() => (Some(r), SignalStatus::new(SignalState::Unavailable, Some("no files to rank".to_string()))),
            Some(r) => {
                let caveat = (!r.churn).then(|| "ranked without churn (no git history)".to_string());
                (Some(r), SignalStatus::new(SignalState::Ready, caveat))
            }
        }
    }

    /// Everything [`ripple_policy::evaluate_with`] needs for `changed`, and
    /// the `history` envelope object of an `impact` reply.
    pub fn input(&self, snap: &Arc<Snapshot>, changed: &[ChangedFile], mode: RankMode) -> (SignalInput, HistoryInfo) {
        let mut paths: Vec<String> = changed.iter().map(|c| c.path.clone()).collect();
        paths.sort_unstable();
        paths.dedup();
        let info = crate::historyd::impact_history(&self.history, snap, &paths);
        let (churn_st, cochange_st) = self.history_status();
        let (ranking, hotspot_st) = self.ranking(snap, mode);
        let cov = self.coverage.ready();
        let now = ripple_coverage::now();
        let mut files = std::collections::BTreeMap::new();
        for c in changed {
            if c.status == FileStatus::Deleted {
                continue;
            }
            let churn = info.churn.get(&c.path).map(|f| ChurnMetric { commits: f.commits, recent_commits: f.recent_commits, last_changed: f.last_changed });
            let coverage = cov.and_then(|cv| coverage_metric(cv, &c.path, c.hunks.as_deref(), self.coverage.head_time, now));
            let hotspot = ranking.as_ref().and_then(|r| r.hotspot(&c.path));
            files.insert(c.path.clone(), FileSignals { churn, coverage, hotspot });
        }
        let usually_changes_with = info.usually_changes_with.iter().map(co_entry).collect();
        // `missing_cochange` also counts partners that are direct graph neighbours.
        let partners = if cochange_st.is_ready() && snap.policy.rules().any(|r| r.when.missing_cochange.is_some()) {
            let diff: HashSet<&str> = paths.iter().map(String::as_str).collect();
            let q = PartnerQuery { limit: 20, ..PartnerQuery::from_config(&snap.policy.config.history) };
            self.history.lookup(&paths, &q, |p| diff.contains(p)).usually_changes_with.iter().map(|p| (co_entry(p), p.score)).collect()
        } else {
            Vec::new()
        };
        let status = Signals { complexity: SignalStatus::ready(), churn: churn_st, cochange: cochange_st, coverage: self.coverage.status(), hotspot: hotspot_st };
        let input = SignalInput { status, files, usually_changes_with, partners, hotspot_percent: snap.policy.config.rank.hotspot_percent };
        (input, info)
    }
}

/// How a query may obtain the ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankMode {
    /// Never computes in the daemon's foreground (`impact`).
    Cached,
    /// Computes only when there is none (`file`).
    Any,
    /// Up to date (`rank`).
    Fresh,
}

pub fn co_entry(p: &crate::historyd::CoChange) -> CoChangeEntry {
    CoChangeEntry { path: p.path.clone(), via: p.via.clone(), together: p.together, of: p.of, why: cochange_why(&p.via, p.together, p.of) }
}

/// Coverage of `path`, and of its changed lines when `hunks` are given.
pub fn coverage_metric(cv: &Coverage, path: &str, hunks: Option<&[ripple_policy::Hunk]>, head_time: Option<i64>, now: i64) -> Option<CoverageMetric> {
    let f = cv.file(path)?;
    let ranges: Vec<(u32, u32)> = hunks.unwrap_or_default().iter().filter(|h| h.count > 0).map(|h| (h.start, h.start + h.count - 1)).collect();
    let changed = (!ranges.is_empty()).then(|| cv.lines(path, &ranges)).flatten().and_then(|l| l.percent());
    Some(CoverageMetric {
        percent: f.percent().map(round1),
        covered: f.covered,
        coverable: f.coverable,
        changed_lines_percent: changed.map(round1),
        stale: cv.freshness(Some(path), head_time, now).is_some_and(|fr| fr.is_stale()),
    })
}
