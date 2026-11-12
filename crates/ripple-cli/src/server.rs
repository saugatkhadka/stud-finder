//! The resident daemon: one per repo root, serving the protocol in
//! [`crate::protocol`] over a Unix socket while a watcher keeps the index fresh.
//!
//! Threads: one accept loop, one thread per connection (requests on a
//! connection are answered in order; connections run concurrently; at most
//! [`MAX_CONNECTIONS`] at once, further clients wait in the listen queue),
//! one indexer thread that owns the [`Engine`], the watcher's own thread,
//! and an idle timer that stops the daemon after `daemon.idle_timeout_minutes`
//! without a request.
//!
//! Diff-based `impact` requests are answered from [`GitTracker`], which
//! caches the changed-file set and updates it from the watcher's events.
//!
//! Queries never take a lock that the indexer holds while indexing: the
//! current snapshot is an `Arc` behind an `RwLock` held only to clone or
//! replace the `Arc`, and the pending-change bookkeeping is a separate mutex
//! held only for bookkeeping. See [`crate::protocol`] for the freshness contract.
//!
//! Edits are coalesced: the indexer takes every path delivered so far as
//! one batch, so N editors x M edits cost one pass per indexer cycle, not
//! N x M passes. A waiting request only skips the debounce.
//!
//! **Watcher sync latency (macOS).** The cookie round trip used to take
//! either ~0.5 ms or ~11 ms. The 11 ms is not ours (notify already uses
//! FSEvents latency 0 with `NoDefer`): the kernel's fsevents device wakes
//! its reader (`fseventsd`) at once only when more than about 16 events
//! are queued, otherwise after a 10 ms timer. A raw FSEvents client shows
//! the same 10-12 ms mode, `FSEventStreamFlushSync` does not help (right
//! after the write, `FSEventsGetCurrentEventId()` is still below the
//! cookie's event id: fseventsd has not seen it yet), and adding 15+ cheap
//! events after the cookie removes the mode. So [`cookie_sync`] follows the
//! cookie with [`KICK_EVENTS`] `fchmod`s of it. Kicked, an edit's event can
//! arrive just after a later cookie's, so it does [`COOKIE_ROUNDS`] round
//! trips (see there).

use crate::engine::{Engine, Pipeline, Snapshot};
use crate::gitd::GitTracker;
use crate::historyd::{self, HistoryD};
use crate::paths::DaemonPaths;
use crate::protocol::{ErrorBody, FileParams, Freshness, ImpactParams, MAX_PENDING_LISTED, RankParams, Request, WaitMode, WhyParams, err_line, ok_line, ok_line_fresh, ok_line_history};
use crate::signals::{CoverageD, RankD, Sources};
use crate::query;
use anyhow::{Context, Result, bail};
use ripple_core::RelPath;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

/// Quiet period after the last watcher event before an unprompted re-index.
pub const DEBOUNCE: Duration = Duration::from_millis(30);
/// Upper bound on how long continuous events can postpone a re-index.
pub const MAX_DEBOUNCE: Duration = Duration::from_millis(300);
/// How long a `sync` request waits for the index to catch up before failing.
const MAX_WAIT: Duration = Duration::from_secs(300);
/// How long a `sync` (or `diff`) request waits for its cookie.
const COOKIE_TIMEOUT: Duration = Duration::from_secs(2);
const COOKIE_PREFIX: &str = "ripple-cookie-";
/// Extra file events generated after a cookie so the kernel hands it to
/// `fseventsd` at once (see the module docs). Measured threshold: 15.
#[cfg(target_os = "macos")]
const KICK_EVENTS: usize = 24;
/// Cookie round trips per sync. With the kick, FSEvents can (rarely) hand
/// over an edit's event a fraction of a millisecond *after* a cookie
/// created later (measured: 27 of 4,000 edits, each 0.24-0.43 ms late); a
/// second round trip, started once the first cookie is seen, has caught all
/// of them (0 of 6,000). inotify keeps one queue in order: one round.
#[cfg(target_os = "macos")]
const COOKIE_ROUNDS: usize = 2;
#[cfg(not(target_os = "macos"))]
const COOKIE_ROUNDS: usize = 1;
/// Cookie round-trip times kept for `stats`.
const COOKIE_SAMPLES: usize = 1024;
/// Distinguishes the cookies of daemons in one process (tests).
static DAEMON_SEQ: AtomicU64 = AtomicU64::new(0);
/// Connections served at once. Each is a thread; CLI clients hold one for a
/// single request, so this bounds threads, not clients.
pub const MAX_CONNECTIONS: usize = 128;
/// A connection with no request for this long is closed (frees its slot).
const CONN_IDLE: Duration = Duration::from_secs(600);

#[derive(Default)]
struct State {
    /// Changed paths delivered by the watcher (or found stale) and not yet picked up.
    dirty: BTreeSet<RelPath>,
    /// Bumped whenever paths are added to `dirty`.
    delivered: u64,
    /// `delivered` value covered by the published snapshot.
    applied: u64,
    /// Paths of the batch being indexed right now.
    in_progress: Option<BTreeSet<RelPath>>,
    /// A request is waiting: skip the debounce.
    urgent: bool,
    first_event: Option<Instant>,
    last_event: Option<Instant>,
    reload_requested: u64,
    reload_done: u64,
    /// Cookies requests are waiting for -> reported by the watcher yet.
    cookies: HashMap<String, bool>,
    shutdown: bool,
    fatal: Option<String>,
    /// When the running pass started.
    pass_started: Option<Instant>,
    /// Duration of the last incremental pass (predicts the next one).
    last_pass: Option<Duration>,
    counters: Counters,
}

/// Indexer and freshness counters for `stats`.
#[derive(Default)]
struct Counters {
    passes: u64,
    /// Paths over all incremental passes, and the largest batch.
    paths: u64,
    max_batch: usize,
    /// Recent cookie round trips, microseconds.
    cookie_us: VecDeque<u32>,
    cookie_timeouts: u64,
    fresh: u64,
    /// Answered fresh after waiting for the index.
    waited: u64,
    stale: u64,
    /// Stale answers given before `max_wait` because the pass would not finish in time.
    predicted: u64,
}

fn percentile(v: &VecDeque<u32>, p: f64) -> u32 {
    let mut s: Vec<u32> = v.iter().copied().collect();
    s.sort_unstable();
    s.get(((s.len().max(1) - 1) as f64 * p).round() as usize).copied().unwrap_or(0)
}

pub struct Shared {
    root: PathBuf,
    paths: DaemonPaths,
    state: Mutex<State>,
    cv: Condvar,
    snapshot: RwLock<Option<Arc<Snapshot>>>,
    started: Instant,
    cookie_dir: PathBuf,
    cookie_seq: AtomicU64,
    /// `{COOKIE_PREFIX}{pid}-{daemon}-`
    cookie_stem: String,
    requests: AtomicU64,
    git: GitTracker,
    /// Git-history signals, built in the background (see [`crate::historyd`]).
    history: Arc<HistoryD>,
    /// Coverage reports, loaded in the background (see [`crate::signals`]).
    coverage: Arc<CoverageD>,
    /// The cached hotspot ranking.
    rank: Arc<RankD>,
    /// Milliseconds after `started` of the last request.
    last_request_ms: AtomicU64,
    in_flight: AtomicUsize,
    /// Open connections, bounded by [`MAX_CONNECTIONS`].
    conns: Mutex<usize>,
    conns_cv: Condvar,
    idle_timeout: Option<Duration>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
    fn current(&self) -> Option<Arc<Snapshot>> {
        self.snapshot.read().unwrap_or_else(|p| p.into_inner()).clone()
    }
    fn set_current(&self, s: Arc<Snapshot>) {
        *self.snapshot.write().unwrap_or_else(|p| p.into_inner()) = Some(s);
    }
    fn generation(&self) -> u64 {
        self.current().map(|s| s.generation).unwrap_or(0)
    }
    /// The signal sources of a query (clones three `Arc`s).
    fn sources(&self) -> Sources {
        Sources { history: self.history.view(), coverage: self.coverage.view(), rank: self.rank.clone(), background_rank: true }
    }

    fn max_wait_ms(&self) -> u64 {
        self.current().map(|s| s.policy.config.daemon.max_wait_ms).unwrap_or(ripple_core::config::DEFAULT_MAX_WAIT_MS)
    }

    /// `paths` are relative to the root, except cookies (bare file names).
    fn deliver(&self, paths: impl IntoIterator<Item = RelPath>) {
        let (cookies, paths): (Vec<RelPath>, Vec<RelPath>) = paths.into_iter().partition(|p| p.starts_with(COOKIE_PREFIX));
        // Before the cookies: a request that sees its cookie must find every
        // earlier path in the git log too.
        self.git.log.push(&paths);
        let mut st = self.lock();
        let mut any = false;
        for c in cookies {
            if let Some(seen) = st.cookies.get_mut(&c) {
                *seen = true;
                any = true;
            }
        }
        for p in paths {
            st.dirty.insert(p);
            st.delivered += 1;
            let now = Instant::now();
            st.last_event = Some(now);
            st.first_event.get_or_insert(now);
            any = true;
        }
        if any {
            self.cv.notify_all();
        }
    }
}

#[derive(Default)]
pub struct ServeOptions {
    /// Install SIGTERM/SIGINT/SIGHUP handlers that remove the socket (binary only).
    pub signals: bool,
    /// Overrides `daemon.idle_timeout_minutes` (tests). `Some(ZERO)` = never.
    pub idle_timeout: Option<Duration>,
    /// Holds every git-history build / update this long (tests).
    pub history_hold: Duration,
}

/// Runs the daemon until a `shutdown` request. Fails fast if another daemon
/// holds the lock for this root.
pub fn serve(root: &Path, pipeline: Pipeline, paths: DaemonPaths, opts: ServeOptions) -> Result<()> {
    paths.create_dir()?;
    let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&paths.lock).with_context(|| format!("opening {}", paths.lock.display()))?;
    // A daemon that is exiting (idle timeout) still holds the lock for a
    // moment after it stops accepting connections: wait for it briefly.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {
                if UnixStream::connect(&paths.socket).is_ok() || Instant::now() >= deadline {
                    bail!("a ripple daemon is already serving {} (pid {})", root.display(), paths.recorded_pid().map(|p| p.to_string()).unwrap_or("?".into()))
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e).context("locking the daemon lock file"),
        }
    }
    lock.set_len(0)?;
    (&lock).write_all(format!("{}\n", std::process::id()).as_bytes())?;

    let config = ripple_policy::load_config(root).map(|l| l.config).unwrap_or_default();
    let history = HistoryD::start_with_hold(root, &config, opts.history_hold);
    let coverage = CoverageD::new(root, &config.coverage);
    let cookie_dir = if root.join(".git").is_dir() { root.join(".git") } else { root.to_path_buf() };
    let cookie_stem = format!("{COOKIE_PREFIX}{}-{}-", std::process::id(), DAEMON_SEQ.fetch_add(1, Ordering::Relaxed));
    let shared = Arc::new(Shared {
        root: root.to_path_buf(),
        paths: paths.clone(),
        state: Mutex::new(State::default()),
        cv: Condvar::new(),
        snapshot: RwLock::new(None),
        started: Instant::now(),
        cookie_dir,
        cookie_seq: AtomicU64::new(0),
        cookie_stem,
        requests: AtomicU64::new(0),
        git: GitTracker::new(root),
        history,
        coverage,
        rank: RankD::new(),
        last_request_ms: AtomicU64::new(0),
        in_flight: AtomicUsize::new(0),
        conns: Mutex::new(0),
        conns_cv: Condvar::new(),
        idle_timeout: opts.idle_timeout,
    });

    {
        let h = shared.history.clone();
        shared.git.on_head_move(move || h.nudge());
    }
    {
        let weak = Arc::downgrade(&shared);
        shared.coverage.start(Box::new(move || weak.upgrade()?.current().map(|s| s.files.clone())));
    }
    let _watcher = start_watcher(&shared).map_err(|e| eprintln!("ripple: file watching unavailable ({e:#}); only re-stat freshness applies")).ok();
    // Git results are only cached while the watcher reports changes.
    // (Also: whether there is a watcher to sync with.)
    shared.git.watching.store(_watcher.is_some(), Ordering::SeqCst);
    {
        let shared = shared.clone();
        std::thread::Builder::new().name("ripple-indexer".into()).spawn(move || indexer(shared, pipeline))?;
    }
    if _watcher.is_some() && crate::files::in_git_repo(root) {
        let shared = shared.clone();
        std::thread::Builder::new().name("ripple-git-prewarm".into()).spawn(move || shared.git.prewarm())?;
    }
    {
        let shared = shared.clone();
        std::thread::Builder::new().name("ripple-idle".into()).spawn(move || idle_timer(shared))?;
    }

    // Bind only once the watcher is running: a client that can connect can
    // rely on every later file change being observed.
    // We hold the lock, so any existing socket is left over from a dead daemon.
    if paths.socket.exists() {
        std::fs::remove_file(&paths.socket).with_context(|| format!("removing stale socket {}", paths.socket.display()))?;
    }
    let listener = UnixListener::bind(&paths.socket).with_context(|| format!("binding {}", paths.socket.display()))?;
    std::fs::set_permissions(&paths.socket, std::fs::Permissions::from_mode(0o600))?;
    if opts.signals {
        install_signal_handlers(&paths.socket);
    }


    eprintln!("ripple: serving {} on {}", root.display(), paths.socket.display());
    for stream in listener.incoming() {
        if shared.lock().shutdown {
            break;
        }
        let Ok(stream) = stream else { continue };
        // Wait for a free slot; meanwhile new clients queue in the backlog.
        {
            let mut n = shared.conns.lock().unwrap_or_else(|p| p.into_inner());
            while *n >= MAX_CONNECTIONS {
                n = shared.conns_cv.wait(n).unwrap_or_else(|p| p.into_inner());
            }
            *n += 1;
        }
        let worker = shared.clone();
        let spawned = std::thread::Builder::new().name("ripple-conn".into()).spawn(move || {
            handle_conn(stream, &worker);
            *worker.conns.lock().unwrap_or_else(|p| p.into_inner()) -= 1;
            worker.conns_cv.notify_one();
        });
        if spawned.is_err() {
            *shared.conns.lock().unwrap_or_else(|p| p.into_inner()) -= 1;
        }
    }
    let _ = std::fs::remove_file(&paths.socket);
    drop(lock);
    // A running history build is cancelled between commits; wait for it.
    shared.history.join();
    shared.coverage.shutdown();
    shared.coverage.join();
    Ok(())
}

static SOCKET_PATH: std::sync::OnceLock<std::ffi::CString> = std::sync::OnceLock::new();

extern "C" fn on_signal(_: libc::c_int) {
    if let Some(p) = SOCKET_PATH.get() {
        // SAFETY: unlink and _exit are async-signal-safe; the CString is immutable.
        unsafe { libc::unlink(p.as_ptr()) };
    }
    unsafe { libc::_exit(0) };
}

fn install_signal_handlers(socket: &Path) {
    if let Ok(c) = std::ffi::CString::new(socket.as_os_str().as_encoded_bytes()) {
        let _ = SOCKET_PATH.set(c);
    }
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: installing a handler that only calls async-signal-safe functions.
        unsafe { libc::signal(sig, on_signal as *const () as libc::sighandler_t) };
    }
}

fn start_watcher(shared: &Arc<Shared>) -> Result<notify::RecommendedWatcher> {
    use notify::{EventKind, RecursiveMode, Watcher};
    let s = shared.clone();
    let roots: Vec<PathBuf> = vec![shared.root.clone()];
    let debug = std::env::var_os("RIPPLE_DEBUG").is_some();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
        Ok(ev) => {
            if matches!(ev.kind, EventKind::Access(_)) {
                return;
            }
            if ev.need_rescan() {
                s.git.log.overflow();
                let mut st = s.lock();
                st.reload_requested += 1;
                s.cv.notify_all();
                return;
            }
            let rels = ev.paths.iter().filter_map(|p| relative(&roots, p)).filter(|r| !r.is_empty());
            // Cookies are passed on as bare names; only those a request waits for count.
            // Git's own files are dropped, but HEAD / branch moves feed the history thread.
            let mut head_moved = false;
            let mut coverage = false;
            let rels: Vec<RelPath> = rels
                .filter_map(|r| match r.rsplit('/').next() {
                    Some(name) if name.starts_with(COOKIE_PREFIX) => Some(name.to_string()),
                    _ => {
                        let git = r == ".git" || r.starts_with(".git/");
                        head_moved |= git && historyd::is_head_event(&r);
                        coverage |= !git && !coverage && s.coverage.is_report_path(&r);
                        (!git).then_some(r)
                    }
                })
                .collect();
            if head_moved {
                s.history.nudge();
            }
            if coverage {
                s.coverage.nudge();
            }
            if debug {
                eprintln!("ripple: event {:?} {:?} -> {rels:?}", ev.kind, ev.paths);
            }
            if !rels.is_empty() {
                s.deliver(rels);
            }
        }
        Err(e) => {
            s.git.log.overflow();
            eprintln!("ripple: watcher error: {e}")
        }
    })?;
    watcher.watch(&shared.root, RecursiveMode::Recursive)?;
    Ok(watcher)
}

fn relative(roots: &[PathBuf], p: &Path) -> Option<String> {
    roots.iter().find_map(|r| p.strip_prefix(r).ok()).and_then(|r| r.to_str()).map(str::to_string)
}

fn indexer(shared: Arc<Shared>, pipeline: Pipeline) {
    let start_seq = shared.lock().delivered;
    let mut engine = match Engine::open(&shared.root, pipeline) {
        Ok((engine, snap)) => {
            shared.set_current(snap);
            engine
        }
        Err(e) => {
            let mut st = shared.lock();
            st.fatal = Some(format!("initial index failed: {e:#}"));
            shared.cv.notify_all();
            return;
        }
    };
    {
        let mut st = shared.lock();
        st.applied = start_seq;
        shared.cv.notify_all();
    }
    let debug = std::env::var_os("RIPPLE_DEBUG").is_some();
    loop {
        let mut st = shared.lock();
        loop {
            if st.shutdown {
                return;
            }
            if st.reload_requested > st.reload_done {
                break;
            }
            if !st.dirty.is_empty() {
                let quiet = st.last_event.is_none_or(|t| t.elapsed() >= DEBOUNCE);
                let overdue = st.first_event.is_some_and(|t| t.elapsed() >= MAX_DEBOUNCE);
                if st.urgent || quiet || overdue {
                    break;
                }
                let wait = DEBOUNCE.saturating_sub(st.last_event.map(|t| t.elapsed()).unwrap_or_default());
                st = shared.cv.wait_timeout(st, wait.max(Duration::from_millis(1))).unwrap_or_else(|p| p.into_inner()).0;
                continue;
            }
            st = shared.cv.wait(st).unwrap_or_else(|p| p.into_inner());
        }
        let reload = (st.reload_requested > st.reload_done).then_some(st.reload_requested);
        // Everything delivered so far is one batch (edits coalesce).
        let batch = std::mem::take(&mut st.dirty);
        let target = st.delivered;
        st.in_progress = Some(batch.clone());
        st.urgent = false;
        st.first_event = None;
        let started = Instant::now();
        st.pass_started = Some(started);
        drop(st);

        if debug {
            eprintln!("ripple: indexing {} path(s){}: {:?}", batch.len(), if reload.is_some() { " (full reload)" } else { "" }, batch.iter().take(20).collect::<Vec<_>>());
        }
        let result = if reload.is_some() { engine.full_build() } else { engine.apply_paths(&batch) };
        let took = started.elapsed();
        match result {
            Ok(snap) => shared.set_current(snap),
            Err(e) => eprintln!("ripple: re-index failed: {e:#}"),
        }
        let mut st = shared.lock();
        st.applied = target;
        st.in_progress = None;
        st.pass_started = None;
        if let Some(r) = reload {
            st.reload_done = r;
        } else {
            st.last_pass = Some(took);
            st.counters.passes += 1;
            st.counters.paths += batch.len() as u64;
            st.counters.max_batch = st.counters.max_batch.max(batch.len());
        }
        shared.cv.notify_all();
    }
}

/// Outcome of the freshness step for one request.
struct Fresh {
    snap: Arc<Snapshot>,
    info: Freshness,
    /// Every pending path (`info.pending` is truncated).
    pending: BTreeSet<RelPath>,
    refresh_us: u64,
}

fn wait_for<'a>(shared: &'a Shared, mut st: MutexGuard<'a, State>, done: impl Fn(&State) -> bool) -> Result<MutexGuard<'a, State>, ErrorBody> {
    let start = Instant::now();
    while !done(&st) {
        if let Some(f) = &st.fatal {
            return Err(ErrorBody::new("index_failed", f.clone()));
        }
        if st.shutdown {
            return Err(ErrorBody::new("shutting_down", "daemon is shutting down"));
        }
        if start.elapsed() > MAX_WAIT {
            return Err(ErrorBody::new("timeout", "timed out waiting for the index"));
        }
        st = shared.cv.wait_timeout(st, Duration::from_millis(500)).unwrap_or_else(|p| p.into_inner()).0;
    }
    Ok(st)
}

/// Makes the kernel hand pending file events to `fseventsd` now instead of
/// after its 10 ms timer: it does so once enough events are queued.
#[cfg(target_os = "macos")]
fn kick(file: &std::fs::File) {
    for i in 0..KICK_EVENTS {
        let _ = file.set_permissions(std::fs::Permissions::from_mode(if i % 2 == 0 { 0o644 } else { 0o600 }));
    }
}

#[cfg(not(target_os = "macos"))]
fn kick(_: &std::fs::File) {}

/// Writes a cookie file and waits (until `deadline`) for the watcher to
/// report it: every file event that happened before this call has then been
/// delivered. `Ok(false)` when the deadline passed first.
fn cookie_sync(shared: &Shared, deadline: Instant) -> Result<bool, ErrorBody> {
    let t = Instant::now();
    let mut ok = true;
    for _ in 0..COOKIE_ROUNDS {
        ok = cookie_round(shared, deadline)?;
        if !ok {
            break;
        }
    }
    let mut st = shared.lock();
    if ok {
        let c = &mut st.counters.cookie_us;
        if c.len() >= COOKIE_SAMPLES {
            c.pop_front();
        }
        c.push_back(t.elapsed().as_micros().min(u32::MAX as u128) as u32);
    } else {
        st.counters.cookie_timeouts += 1;
    }
    Ok(ok)
}

/// One cookie round trip: create the cookie, kick, wait until it is seen.
fn cookie_round(shared: &Shared, deadline: Instant) -> Result<bool, ErrorBody> {
    let n = shared.cookie_seq.fetch_add(1, Ordering::Relaxed);
    let name = format!("{}{n}", shared.cookie_stem);
    let path = shared.cookie_dir.join(&name);
    shared.lock().cookies.insert(name.clone(), false);
    match std::fs::File::create(&path) {
        Ok(f) => kick(&f),
        Err(e) => {
            shared.lock().cookies.remove(&name);
            return Err(ErrorBody::new("sync_failed", format!("writing {}: {e}", path.display())));
        }
    }
    let mut st = shared.lock();
    let ok = loop {
        if st.cookies.get(&name) == Some(&true) {
            break true;
        }
        let now = Instant::now();
        if now >= deadline {
            break false;
        }
        st = shared.cv.wait_timeout(st, deadline - now).unwrap_or_else(|p| p.into_inner()).0;
    };
    st.cookies.remove(&name);
    drop(st);
    let _ = std::fs::remove_file(&path);
    Ok(ok)
}

/// The freshness step (see the freshness contract in [`crate::protocol`]).
/// `synced`: a cookie round trip this request already did (diff requests).
fn refresh(shared: &Shared, named: &[String], mode: WaitMode, synced: Option<bool>) -> Result<Fresh, ErrorBody> {
    let t = Instant::now();
    let deadline = match mode {
        WaitMode::Bounded(d) => t + d,
        _ => t + MAX_WAIT,
    };
    let watching = shared.git.watching.load(Ordering::SeqCst);
    let synced = match synced {
        Some(s) => s,
        None if mode == WaitMode::NoWait || !watching => false,
        None => {
            let cookie_deadline = if mode == WaitMode::Full { t + COOKIE_TIMEOUT } else { deadline.max(t + Duration::from_millis(1)) };
            let ok = cookie_sync(shared, cookie_deadline)?;
            if !ok && mode == WaitMode::Full {
                return Err(ErrorBody::new("sync_timeout", "the file watcher did not report the sync cookie within 2s"));
            }
            ok
        }
    };
    let st = wait_for(shared, shared.lock(), |_| shared.current().is_some())?;
    drop(st);
    let snap = shared.current().ok_or_else(|| ErrorBody::new("internal", "no snapshot"))?;
    let named: Vec<String> = named.iter().map(|n| query::clean(n)).collect();
    let stale = snap.stale_files(&named);

    let mut st = shared.lock();
    if !stale.is_empty() {
        st.dirty.extend(stale);
        st.delivered += 1;
        st.first_event.get_or_insert_with(Instant::now);
    }
    let target = st.delivered;
    let mut waited = false;
    if st.applied < target && mode != WaitMode::NoWait {
        st.urgent = true;
        shared.cv.notify_all();
        waited = true;
    }
    match mode {
        WaitMode::Full => st = wait_for(shared, st, |s| s.applied >= target)?,
        WaitMode::Bounded(_) => {
            while st.applied < target {
                if let Some(f) = &st.fatal {
                    return Err(ErrorBody::new("index_failed", f.clone()));
                }
                if st.shutdown {
                    return Err(ErrorBody::new("shutting_down", "daemon is shutting down"));
                }
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                // Don't wait for a pass that cannot finish in time.
                if let Some(eta) = eta(&st, now)
                    && eta > deadline
                {
                    st.counters.predicted += 1;
                    break;
                }
                st = shared.cv.wait_timeout(st, deadline - now).unwrap_or_else(|p| p.into_inner()).0;
            }
        }
        WaitMode::NoWait => {}
    }
    let is_stale = st.applied < target;
    let pending: BTreeSet<RelPath> = if is_stale { st.dirty.iter().chain(st.in_progress.iter().flatten()).cloned().collect() } else { BTreeSet::new() };
    match (is_stale, waited) {
        (true, _) => st.counters.stale += 1,
        (false, true) => st.counters.waited += 1,
        (false, false) => st.counters.fresh += 1,
    }
    drop(st);
    let snap = shared.current().unwrap_or(snap);
    let info = Freshness { stale: is_stale, synced, pending: pending.iter().take(MAX_PENDING_LISTED).cloned().collect(), pending_count: pending.len() };
    Ok(Fresh { snap, info, pending, refresh_us: t.elapsed().as_micros() as u64 })
}

/// When everything delivered so far will be indexed, judging by the last
/// pass's duration; `None` if unknown.
fn eta(st: &State, now: Instant) -> Option<Instant> {
    let pass = st.last_pass?;
    let current_end = st.pass_started.map(|s| (s + pass).max(now));
    Some(match current_end {
        // The running pass, then one more for what arrived meanwhile.
        Some(end) if !st.dirty.is_empty() => end + pass,
        Some(end) => end,
        None => now + pass,
    })
}

/// Stops the daemon after the configured time without requests. The
/// timeout comes from the current snapshot's `daemon.idle_timeout_minutes`.
fn idle_timer(shared: Arc<Shared>) {
    loop {
        let timeout = shared.idle_timeout.or_else(|| shared.current().map(|s| Duration::from_secs(s.policy.config.daemon.idle_timeout_minutes.saturating_mul(60))));
        let check = match timeout {
            Some(t) if !t.is_zero() => (t / 4).clamp(Duration::from_millis(10), Duration::from_secs(30)),
            _ => Duration::from_secs(30),
        };
        {
            let st = shared.lock();
            if st.shutdown {
                return;
            }
            let _ = shared.cv.wait_timeout(st, check);
        }
        let Some(timeout) = timeout.filter(|t| !t.is_zero()) else { continue };
        let last = Duration::from_millis(shared.last_request_ms.load(Ordering::SeqCst));
        let idle = shared.started.elapsed().saturating_sub(last);
        if idle >= timeout && shared.in_flight.load(Ordering::SeqCst) == 0 {
            eprintln!("ripple: no requests for {}s; exiting", idle.as_secs());
            begin_shutdown(&shared);
            return;
        }
    }
}

/// Marks the daemon as stopping and wakes the accept loop so it exits.
fn begin_shutdown(shared: &Shared) {
    shared.history.shutdown();
    shared.coverage.shutdown();
    let mut st = shared.lock();
    st.shutdown = true;
    shared.cv.notify_all();
    drop(st);
    // Wake the accept loop. A full listen backlog refuses the connection, so retry.
    for _ in 0..200 {
        if UnixStream::connect(&shared.paths.socket).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn handle_conn(stream: UnixStream, shared: &Arc<Shared>) {
    let _ = stream.set_read_timeout(Some(CONN_IDLE));
    let Ok(read) = stream.try_clone() else { return };
    let reader = BufReader::new(read);
    let mut writer = std::io::BufWriter::new(stream);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let start = Instant::now();
        shared.in_flight.fetch_add(1, Ordering::SeqCst);
        shared.last_request_ms.store(shared.started.elapsed().as_millis() as u64, Ordering::SeqCst);
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle_line(shared, &line, start)))
            .unwrap_or_else(|_| err_line(&Value::Null, shared.generation(), 0, &ErrorBody::new("internal", "request handler panicked")));
        shared.last_request_ms.store(shared.started.elapsed().as_millis() as u64, Ordering::SeqCst);
        shared.in_flight.fetch_sub(1, Ordering::SeqCst);
        if writer.write_all(out.as_bytes()).and_then(|_| writer.write_all(b"\n")).and_then(|_| writer.flush()).is_err() {
            break;
        }
        if shared.lock().shutdown {
            break;
        }
    }
}

fn params<T: serde::de::DeserializeOwned + Default>(v: &Value) -> Result<T, ErrorBody> {
    if v.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(v.clone()).map_err(|e| ErrorBody::new("bad_params", e.to_string()))
}

/// Treats changed files that are still pending as wholly changed: their
/// hunks describe contents newer than the method ranges in the snapshot.
fn whole_file_if_pending(p: &mut ImpactParams, pending: &BTreeSet<RelPath>) {
    if pending.is_empty() {
        return;
    }
    for c in &mut p.changed {
        let path = query::clean(&c.path);
        if c.scopes.is_none() && (pending.contains(&path) || c.old_path.as_ref().is_some_and(|o| pending.contains(&query::clean(o)))) {
            c.hunks = None;
        }
    }
}

fn handle_line(shared: &Arc<Shared>, line: &str, start: Instant) -> String {
    shared.requests.fetch_add(1, Ordering::Relaxed);
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => return err_line(&Value::Null, shared.generation(), 0, &ErrorBody::new("bad_request", format!("invalid request JSON: {e}"))),
    };
    let default_wait = || shared.max_wait_ms();
    // The `history` object of an impact reply (see [`crate::historyd`]).
    let mut history: Option<String> = None;
    let result: Result<(String, u64, Option<Freshness>), ErrorBody> = (|| match req.method.as_str() {
        "impact" => {
            let mut p: ImpactParams = params(&req.params)?;
            let mode = WaitMode::from_params(p.sync, p.no_wait, p.max_wait_ms, default_wait());
            let mut synced = None;
            if let Some(d) = p.diff.take() {
                if !p.files.is_empty() || !p.changed.is_empty() {
                    return Err(ErrorBody::new("bad_params", "`diff` excludes `files` and `changed`"));
                }
                // The git cache trusts the watcher's log: make sure every
                // change made before this request is in it. If the watcher
                // does not answer, its log cannot be trusted.
                if shared.git.watching.load(Ordering::SeqCst) {
                    let ok = cookie_sync(shared, Instant::now() + COOKIE_TIMEOUT)?;
                    if !ok {
                        shared.git.log.overflow();
                        if mode == WaitMode::Full {
                            return Err(ErrorBody::new("sync_timeout", "the file watcher did not report the sync cookie within 2s"));
                        }
                    }
                    synced = Some(ok);
                }
                p.changed = shared.git.changed(&d.spec()).map_err(|e| ErrorBody::new("git_failed", e))?;
            }
            let f = refresh(shared, &p.named_files(), mode, synced)?;
            whole_file_if_pending(&mut p, &f.pending);
            shared.coverage.query_tick();
            let (body, h) = query::impact_result(&f.snap, &p, f.info.pending_count, f.refresh_us, &shared.sources())?;
            history = serde_json::to_string(&h).ok();
            Ok((body, f.snap.generation, Some(f.info)))
        }
        "dependents" | "dependencies" | "areas" | "file" => {
            let p: FileParams = params(&req.params)?;
            let mode = WaitMode::from_params(p.sync, p.no_wait, p.max_wait_ms, default_wait());
            let f = refresh(shared, std::slice::from_ref(&p.file), mode, None)?;
            shared.coverage.query_tick();
            Ok((query::execute(&f.snap, &req.method, &req.params, f.info.pending_count, &shared.sources())?, f.snap.generation, Some(f.info)))
        }
        "rank" => {
            let p: RankParams = params(&req.params)?;
            let mode = WaitMode::from_params(p.sync, p.no_wait, p.max_wait_ms, default_wait());
            let f = refresh(shared, &[], mode, None)?;
            shared.coverage.query_tick();
            Ok((query::execute(&f.snap, "rank", &req.params, f.info.pending_count, &shared.sources())?, f.snap.generation, Some(f.info)))
        }
        "why" => {
            let p: WhyParams = params(&req.params)?;
            let mode = WaitMode::from_params(p.sync, p.no_wait, p.max_wait_ms, default_wait());
            let f = refresh(shared, &[p.from.clone(), p.to.clone()], mode, None)?;
            Ok((query::execute(&f.snap, "why", &req.params, f.info.pending_count, &Sources::none())?, f.snap.generation, Some(f.info)))
        }
        "stats" => {
            // Introspection never waits for the index (or syncs), unless asked to.
            let sync = req.params.get("sync").and_then(Value::as_bool).unwrap_or(false);
            let f = refresh(shared, &[], if sync { WaitMode::Full } else { WaitMode::NoWait }, None)?;
            let mut v = query::stats(&f.snap);
            v["history"] = shared.history.stats_json();
            v["coverage"] = shared.coverage.stats_json();
            v["rank"] = shared.rank.stats_json();
            let st = shared.lock();
            let c = &st.counters;
            v["daemon"] = json!({
                "pid": std::process::id(),
                "version": env!("CARGO_PKG_VERSION"),
                "socket": shared.paths.socket,
                "uptime_ms": shared.started.elapsed().as_millis() as u64,
                "requests": shared.requests.load(Ordering::Relaxed),
                "pending_changes": st.dirty.len() + st.in_progress.as_ref().map_or(0, |b| b.len()),
                "indexing": st.in_progress.is_some(),
                "connections": *shared.conns.lock().unwrap_or_else(|p| p.into_inner()),
                "git": {
                    "watching": shared.git.watching.load(Ordering::Relaxed),
                    "cache_hits": shared.git.counters.hits.load(Ordering::Relaxed),
                    "partial_updates": shared.git.counters.partial.load(Ordering::Relaxed),
                    "full_updates": shared.git.counters.full.load(Ordering::Relaxed),
                    "last_us": shared.git.counters.last_us.load(Ordering::Relaxed),
                },
                "indexer": {
                    "passes": c.passes,
                    "paths": c.paths,
                    "max_batch": c.max_batch,
                    "last_pass_us": st.last_pass.map_or(0, |d| d.as_micros() as u64),
                },
                "freshness": {
                    "max_wait_ms": shared.max_wait_ms(),
                    "answers_fresh": c.fresh,
                    "answers_after_wait": c.waited,
                    "answers_stale": c.stale,
                    "stale_predicted": c.predicted,
                    "cookies": c.cookie_us.len(),
                    "cookie_p50_us": percentile(&c.cookie_us, 0.5),
                    "cookie_p99_us": percentile(&c.cookie_us, 0.99),
                    "cookie_max_us": c.cookie_us.iter().copied().max().unwrap_or(0),
                    "cookie_timeouts": c.cookie_timeouts,
                },
            });
            Ok((v.to_string(), f.snap.generation, Some(f.info)))
        }
        "reload" => {
            let mut st = shared.lock();
            st.reload_requested += 1;
            let want = st.reload_requested;
            shared.cv.notify_all();
            let st = wait_for(shared, st, |s| s.reload_done >= want)?;
            drop(st);
            if let Some(s) = shared.current() {
                shared.history.configure(s.policy.config.history.enabled);
                shared.coverage.configure(&s.policy.config.coverage);
            }
            let g = shared.generation();
            Ok((json!({"generation": g}).to_string(), g, None))
        }
        "shutdown" => {
            begin_shutdown(shared);
            Ok((json!({"stopping": true}).to_string(), shared.generation(), None))
        }
        other => Err(ErrorBody::new("unknown_method", format!("unknown method `{other}`"))),
    })();
    let us = start.elapsed().as_micros() as u64;
    match result {
        Ok((body, generation, Some(fresh))) => match &history {
            Some(h) => ok_line_history(&req.id, generation, us, Some(&fresh), Some(h), &body),
            None => ok_line_fresh(&req.id, generation, us, Some(&fresh), &body),
        },
        Ok((body, generation, None)) => ok_line(&req.id, generation, us, &body),
        Err(e) => err_line(&req.id, shared.generation(), us, &e),
    }
}
