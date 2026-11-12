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
//! **Sleep.** After `daemon.sleep_after_seconds` without requests or file
//! events (and with nothing pending), the indexer writes the current
//! snapshot to disk ([`crate::snapfile`]) and continues from a memory-mapped
//! copy of it, dropping the engine (extractor state, edge lists, the heap
//! graph). Memory freed inside a process mostly stays with the allocator,
//! so the binary instead re-executes itself (`execve`, same pid), handing
//! the listening socket and the lock file to the new image, which maps the
//! snapshot and serves from it. Clients queued meanwhile wait in the listen
//! backlog. The new image's watcher starts after the old one stopped; the
//! gap is covered by a stat sweep ([`Snapshot::sweep`]) of every workspace
//! file and directory against the sleep time (minus [`SLEEP_MARGIN`]).
//! While asleep, changed paths are resolved against the mapped snapshot:
//! if none is relevant (ignored or unindexed files) nothing happens;
//! otherwise the daemon wakes: a full index (as at start), during which
//! queries get the usual freshness treatment (stale answers after
//! `max_wait_ms`).
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

use crate::engine::{Engine, Pipeline, Snapshot, resolve_paths};
use crate::git::DiffSpec;
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
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
/// The post-sleep sweep re-checks files written this long before the
/// snapshot was saved (events still in flight then, coarse mtimes).
pub const SLEEP_MARGIN: Duration = Duration::from_secs(2);
/// Environment variable carrying [`Resume`] into the re-executed image.
const RESUME_ENV: &str = "RIPPLE_DAEMON_RESUME";

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
    /// The idle timer asks the indexer to go to sleep.
    sleep_requested: bool,
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
    /// Milliseconds after `started` of the last request (the idle timeout).
    last_request_ms: AtomicU64,
    /// The same, not counting `stats` (the sleep timer: monitoring a daemon
    /// does not keep its index in memory).
    last_use_ms: AtomicU64,
    in_flight: AtomicUsize,
    /// Open connections, bounded by [`MAX_CONNECTIONS`].
    conns: Mutex<usize>,
    conns_cv: Condvar,
    idle_timeout: Option<Duration>,
    sleep: SleepCtl,
}

/// Sleep bookkeeping (see the module docs).
struct SleepCtl {
    /// Overrides `daemon.sleep_after_seconds` (tests). `Some(ZERO)` = never.
    after: Option<Duration>,
    /// Sleep by re-executing the binary (else in this process).
    reexec: bool,
    path: PathBuf,
    asleep: AtomicBool,
    /// Listening socket and lock file descriptors handed to the next image.
    listener_fd: std::sync::atomic::AtomicI32,
    lock_fd: RawFd,
    /// Times slept / woken over the daemon's life (across re-executions).
    sleeps: AtomicU64,
    wakes: AtomicU64,
    /// `(last_request_ms, delivered)` when a sleep last failed: not retried
    /// until something happens.
    failed_at: Mutex<Option<(u64, u64)>>,
    /// `(requests, delivered)` when free memory was last handed back while asleep.
    released: Mutex<Option<(u64, u64)>>,
    /// Size of the snapshot file, sweep findings and duration of the last sleep / resume.
    last: Mutex<Value>,
}

/// What a daemon that re-executes itself to sleep passes to the new image.
#[derive(serde::Serialize, serde::Deserialize)]
struct Resume {
    listener: RawFd,
    lock: RawFd,
    since_ns: u64,
    idle_ms: u64,
    specs: Vec<String>,
    sleeps: u64,
    wakes: u64,
}

impl Resume {
    /// Reads and clears the variable (children must not see it).
    fn take() -> Option<Self> {
        let v = std::env::var(RESUME_ENV).ok()?;
        // SAFETY: called at daemon start, before this process starts threads of its own.
        unsafe { std::env::remove_var(RESUME_ENV) };
        serde_json::from_str(&v).map_err(|e| eprintln!("ripple: ignoring bad {RESUME_ENV}: {e}")).ok()
    }
}

/// macOS: re-executes this process with `MallocSpaceEfficient=1` unless it
/// is set. The default allocator configuration keeps per-CPU magazines and
/// freed large blocks; with it the daemon's footprint on a large repository
/// is ~40% lower (GitLab: ~690 -> ~400 MB awake) at the same indexing speed,
/// and a sleeper stays small after loading. It must be in the environment
/// when the process starts, hence the re-execution (binary only).
fn space_efficient_malloc() {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::process::CommandExt;
        const VAR: &str = "MallocSpaceEfficient";
        if std::env::var_os(VAR).is_some() {
            return;
        }
        let Ok(exe) = std::env::current_exe() else { return };
        let e = std::process::Command::new(exe).args(std::env::args_os().skip(1)).env(VAR, "1").exec();
        eprintln!("ripple: could not re-execute with {VAR}=1: {e}");
    }
}

/// Returns free allocator pages to the system (best effort).
fn release_free_memory() {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        // SAFETY: a null zone means all zones; goal 0 means as much as possible.
        unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: no preconditions.
    unsafe {
        libc::malloc_trim(0);
    }
}

fn encode_spec(s: &DiffSpec) -> String {
    match s {
        DiffSpec::WorkingTree => "w".into(),
        DiffSpec::Staged => "s".into(),
        DiffSpec::Base(r) => format!("b:{r}"),
    }
}

fn decode_spec(s: &str) -> Option<DiffSpec> {
    match s {
        "w" => Some(DiffSpec::WorkingTree),
        "s" => Some(DiffSpec::Staged),
        _ => s.strip_prefix("b:").map(|r| DiffSpec::Base(r.to_string())),
    }
}

fn set_cloexec(fd: RawFd, on: bool) -> bool {
    // SAFETY: fcntl on a descriptor this process owns.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        flags >= 0 && libc::fcntl(fd, libc::F_SETFD, if on { flags | libc::FD_CLOEXEC } else { flags & !libc::FD_CLOEXEC }) == 0
    }
}

/// Where the sleeping daemon's snapshot goes: next to the history sidecar
/// in the git directory when there is one (never in the work tree), else
/// next to the socket.
fn snapshot_path(root: &Path, paths: &DaemonPaths) -> PathBuf {
    let stem = paths.socket.file_stem().and_then(|s| s.to_str()).unwrap_or("index").to_string();
    let dir = crate::gitd::GitLayout::discover(root).map(|l| l.common.join("ripple")).unwrap_or_else(|| paths.dir.clone());
    dir.join(format!("index-{stem}.snap"))
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
    /// Overrides `daemon.sleep_after_seconds` (tests). `Some(ZERO)` = never.
    pub sleep_after: Option<Duration>,
    /// Sleep by re-executing the current binary with the same arguments,
    /// which returns the indexer's memory to the system (binary only: it
    /// replaces the whole process). Otherwise sleep happens in-process.
    pub reexec: bool,
}

/// Runs the daemon until a `shutdown` request. Fails fast if another daemon
/// holds the lock for this root.
pub fn serve(root: &Path, pipeline: Pipeline, paths: DaemonPaths, opts: ServeOptions) -> Result<()> {
    if opts.reexec {
        space_efficient_malloc();
    }
    paths.create_dir()?;
    let snap_path = snapshot_path(root, &paths);
    let resume = if opts.reexec { Resume::take() } else { None };
    // A re-executed sleeper: the lock and the socket are already ours.
    let (lock, listener) = match &resume {
        // SAFETY: descriptors handed over by the previous image of this process.
        Some(r) => unsafe { (std::fs::File::from_raw_fd(r.lock), Some(UnixListener::from_raw_fd(r.listener))) },
        None => (acquire_lock(root, &paths)?, None),
    };
    set_cloexec(lock.as_raw_fd(), true);
    if let Some(l) = &listener {
        set_cloexec(l.as_raw_fd(), true);
    }
    // ... and its snapshot (if it cannot be used: index from scratch).
    let t = Instant::now();
    let resumed: Option<Arc<Snapshot>> = resume.as_ref().and_then(|_| match crate::snapfile::load(&snap_path, root) {
        Ok(s) => Some(Arc::new(s)),
        Err(e) => {
            eprintln!("ripple: not resuming from {}: {e}", snap_path.display());
            None
        }
    });
    let load_ms = t.elapsed().as_secs_f64() * 1000.0;
    serve_with(root, pipeline, paths, opts, lock, listener, snap_path, resume, resumed, load_ms)
}

fn acquire_lock(root: &Path, paths: &DaemonPaths) -> Result<std::fs::File> {
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
    Ok(lock)
}

#[allow(clippy::too_many_arguments)]
fn serve_with(
    root: &Path,
    pipeline: Pipeline,
    paths: DaemonPaths,
    opts: ServeOptions,
    lock: std::fs::File,
    listener: Option<UnixListener>,
    snap_path: PathBuf,
    resume: Option<Resume>,
    resumed: Option<Arc<Snapshot>>,
    load_ms: f64,
) -> Result<()> {

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
        last_use_ms: AtomicU64::new(0),
        in_flight: AtomicUsize::new(0),
        conns: Mutex::new(0),
        conns_cv: Condvar::new(),
        idle_timeout: opts.idle_timeout,
        sleep: SleepCtl {
            after: opts.sleep_after,
            reexec: opts.reexec,
            path: snap_path,
            asleep: AtomicBool::new(false),
            listener_fd: std::sync::atomic::AtomicI32::new(-1),
            lock_fd: lock.as_raw_fd(),
            sleeps: AtomicU64::new(resume.as_ref().map_or(0, |r| r.sleeps)),
            wakes: AtomicU64::new(resume.as_ref().map_or(0, |r| r.wakes)),
            failed_at: Mutex::new(None),
            released: Mutex::new(None),
            last: Mutex::new(Value::Null),
        },
    });
    // A resumed daemon has been idle since before the re-execution.
    let mut shared = shared;
    if let Some(r) = &resume
        && let Some(s) = Instant::now().checked_sub(Duration::from_millis(r.idle_ms))
    {
        Arc::get_mut(&mut shared).expect("not shared yet").started = s;
    }

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
    if let Some(snap) = &resumed {
        // Serve the mapped snapshot; what changed while no watcher ran is
        // found by the sweep and goes through the indexer like any event.
        shared.set_current(snap.clone());
        shared.sleep.asleep.store(true, Ordering::SeqCst);
        let since = UNIX_EPOCH + Duration::from_nanos(resume.as_ref().map_or(0, |r| r.since_ns));
        let t = Instant::now();
        let found = snap.sweep(since.checked_sub(SLEEP_MARGIN).unwrap_or(UNIX_EPOCH));
        let sweep_ms = t.elapsed().as_secs_f64() * 1000.0;
        *shared.sleep.last.lock().unwrap_or_else(|p| p.into_inner()) = json!({"resumed": true, "load_ms": load_ms, "sweep_ms": sweep_ms, "sweep_found": found.len()});
        if std::env::var_os("RIPPLE_DEBUG").is_some() {
            eprintln!("ripple: resumed; sweep found {:?}", found.iter().take(20).collect::<Vec<_>>());
        }
        if !found.is_empty() {
            shared.deliver(found);
        }
    }
    {
        let shared = shared.clone();
        let resumed = resumed.is_some();
        std::thread::Builder::new().name("ripple-indexer".into()).spawn(move || indexer(shared, pipeline, resumed))?;
    }
    let git_prewarm = _watcher.is_some() && crate::files::in_git_repo(root);
    if git_prewarm || resumed.is_some() {
        let shared = shared.clone();
        let specs: Option<Vec<DiffSpec>> = resume.as_ref().map(|r| r.specs.iter().filter_map(|s| decode_spec(s)).collect());
        std::thread::Builder::new().name("ripple-git-prewarm".into()).spawn(move || {
            match specs {
                Some(s) if git_prewarm => shared.git.prewarm_specs(&s),
                None if git_prewarm => shared.git.prewarm(),
                _ => {}
            }
            if shared.sleep.asleep.load(Ordering::SeqCst) {
                // A resumed sleeper: once history is loaded, hand back what loading freed.
                let t = Instant::now();
                while shared.history.busy() && t.elapsed() < Duration::from_secs(30) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                release_free_memory();
            }
        })?;
    }
    {
        let shared = shared.clone();
        std::thread::Builder::new().name("ripple-idle".into()).spawn(move || idle_timer(shared))?;
    }

    // Bind only once the watcher is running: a client that can connect can
    // rely on every later file change being observed.
    // We hold the lock, so any existing socket is left over from a dead daemon.
    let listener = match listener {
        Some(l) => l,
        None => {
            if paths.socket.exists() {
                std::fs::remove_file(&paths.socket).with_context(|| format!("removing stale socket {}", paths.socket.display()))?;
            }
            let l = UnixListener::bind(&paths.socket).with_context(|| format!("binding {}", paths.socket.display()))?;
            std::fs::set_permissions(&paths.socket, std::fs::Permissions::from_mode(0o600))?;
            l
        }
    };
    shared.sleep.listener_fd.store(listener.as_raw_fd(), Ordering::SeqCst);
    if opts.signals {
        install_signal_handlers(&paths.socket);
    }


    if resumed.is_none() {
        eprintln!("ripple: serving {} on {}", root.display(), paths.socket.display());
    }
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

fn indexer(shared: Arc<Shared>, pipeline: Pipeline, resumed: bool) {
    // Asleep: `engine` is `None`, the published snapshot is mapped from
    // disk, and `pipeline` waits for the next wake.
    let mut pipeline = Some(pipeline);
    let mut engine: Option<Engine> = None;
    if !resumed {
        let start_seq = shared.lock().delivered;
        match Engine::open(&shared.root, pipeline.take().expect("pipeline")) {
            Ok((e, snap)) => {
                shared.set_current(snap);
                engine = Some(e);
            }
            Err(e) => {
                let mut st = shared.lock();
                st.fatal = Some(format!("initial index failed: {e:#}"));
                shared.cv.notify_all();
                return;
            }
        }
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
            if st.reload_requested > st.reload_done || st.sleep_requested {
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
        if std::mem::take(&mut st.sleep_requested) {
            if engine.is_some() && st.dirty.is_empty() && st.reload_requested == st.reload_done {
                drop(st);
                go_to_sleep(&shared, &mut engine, &mut pipeline);
            }
            continue;
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
        let result = match engine.as_mut() {
            Some(e) if reload.is_some() => e.full_build(),
            Some(e) => e.apply_paths(&batch),
            None => match asleep_pass(&shared, &batch, reload.is_some()) {
                Some(snap) => Ok(snap),
                None => wake(&shared, &mut engine, &mut pipeline),
            },
        };
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

/// While asleep: the current (mapped) snapshot if none of `batch` changes
/// what it indexed (records them as seen, like an incremental pass that
/// finds nothing to do); `None` when the daemon has to wake.
fn asleep_pass(shared: &Shared, batch: &BTreeSet<RelPath>, reload: bool) -> Option<Arc<Snapshot>> {
    if reload {
        return None;
    }
    let snap = shared.current()?;
    let pass_start = SystemTime::now();
    let (changes, config) = resolve_paths(&shared.root, &snap.files, &snap.policy, batch);
    if config || !changes.is_empty() {
        return None;
    }
    if let Ok(mut seen) = snap.seen.write() {
        for p in batch {
            seen.insert(p.clone(), pass_start);
        }
    }
    Some(snap)
}

/// Leaves sleep: a full index, as at start.
fn wake(shared: &Shared, engine: &mut Option<Engine>, pipeline: &mut Option<Pipeline>) -> Result<Arc<Snapshot>> {
    let generation = shared.generation();
    let p = pipeline.take().context("no pipeline to wake with")?;
    let (e, snap) = Engine::open_after(&shared.root, p, generation)?;
    *engine = Some(e);
    shared.sleep.asleep.store(false, Ordering::SeqCst);
    shared.sleep.wakes.fetch_add(1, Ordering::Relaxed);
    Ok(snap)
}

/// Writes the snapshot and continues from the mapped copy: by re-executing
/// (see the module docs) or, failing that / in-process, by dropping the engine.
fn go_to_sleep(shared: &Shared, engine: &mut Option<Engine>, pipeline: &mut Option<Pipeline>) {
    let Some(snap) = shared.current() else { return };
    let mark = || Some((shared.last_use_ms.load(Ordering::SeqCst), shared.lock().delivered));
    let since = SystemTime::now();
    let t = Instant::now();
    let bytes = match crate::snapfile::save(&snap, &shared.sleep.path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("ripple: not sleeping: {e}");
            *shared.sleep.failed_at.lock().unwrap_or_else(|p| p.into_inner()) = mark();
            return;
        }
    };
    let save_ms = t.elapsed().as_secs_f64() * 1000.0;
    for p in crate::snapfile::prune(&shared.sleep.path, SystemTime::now()) {
        if std::env::var_os("RIPPLE_DEBUG").is_some() {
            eprintln!("ripple: removed old snapshot {}", p.display());
        }
    }
    shared.sleep.sleeps.fetch_add(1, Ordering::Relaxed);
    if shared.sleep.reexec {
        let e = reexec(shared, since);
        eprintln!("ripple: could not re-execute to sleep ({e}); sleeping in-process");
    }
    match crate::snapfile::load(&shared.sleep.path, &shared.root) {
        Ok(m) => {
            shared.set_current(Arc::new(m));
            drop(snap);
            *pipeline = engine.take().map(Engine::into_pipeline);
            shared.sleep.asleep.store(true, Ordering::SeqCst);
            release_free_memory();
            *shared.sleep.last.lock().unwrap_or_else(|p| p.into_inner()) = json!({"resumed": false, "save_ms": save_ms, "bytes": bytes});
        }
        Err(e) => {
            eprintln!("ripple: not sleeping: reading back {}: {e}", shared.sleep.path.display());
            shared.sleep.sleeps.fetch_sub(1, Ordering::Relaxed);
            *shared.sleep.failed_at.lock().unwrap_or_else(|p| p.into_inner()) = mark();
        }
    }
}

/// Replaces this process with a fresh image of the same binary, which
/// resumes from the snapshot just written. Returns only on failure.
fn reexec(shared: &Shared, since: SystemTime) -> String {
    use std::os::unix::process::CommandExt;
    let listener = shared.sleep.listener_fd.load(Ordering::SeqCst);
    let lock = shared.sleep.lock_fd;
    if listener < 0 {
        return "no listening socket yet".into();
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return e.to_string(),
    };
    let last = Duration::from_millis(shared.last_request_ms.load(Ordering::SeqCst));
    let resume = Resume {
        listener,
        lock,
        since_ns: since.duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0),
        idle_ms: shared.started.elapsed().saturating_sub(last).as_millis() as u64,
        specs: shared.git.recent_specs().iter().map(encode_spec).collect(),
        sleeps: shared.sleep.sleeps.load(Ordering::Relaxed),
        wakes: shared.sleep.wakes.load(Ordering::Relaxed),
    };
    let Ok(json) = serde_json::to_string(&resume) else { return "encoding the resume state".into() };
    if !(set_cloexec(listener, false) && set_cloexec(lock, false)) {
        set_cloexec(listener, true);
        set_cloexec(lock, true);
        return "could not pass the socket on".into();
    }
    let mut args = std::env::args_os();
    let _argv0 = args.next();
    let err = std::process::Command::new(exe).args(args).env(RESUME_ENV, json).exec();
    set_cloexec(listener, true);
    set_cloexec(lock, true);
    err.to_string()
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

/// Stops the daemon after the configured time without requests, and asks
/// the indexer to sleep after `daemon.sleep_after_seconds` without requests
/// or file events. Both come from the current snapshot's config.
fn idle_timer(shared: Arc<Shared>) {
    loop {
        let cfg = shared.current().map(|s| s.policy.config.daemon.clone());
        let timeout = shared.idle_timeout.or_else(|| cfg.as_ref().map(|d| Duration::from_secs(d.idle_timeout_minutes.saturating_mul(60))));
        let sleep_after = shared.sleep.after.or_else(|| cfg.as_ref().map(|d| Duration::from_secs(d.sleep_after_seconds)));
        let check = [timeout, sleep_after]
            .into_iter()
            .flatten()
            .filter(|t| !t.is_zero())
            .map(|t| (t / 4).clamp(Duration::from_millis(10), Duration::from_secs(30)))
            .min()
            .unwrap_or(Duration::from_secs(30));
        {
            let st = shared.lock();
            if st.shutdown {
                return;
            }
            let _ = shared.cv.wait_timeout(st, check);
        }
        let since_start = shared.started.elapsed();
        let idle = since_start.saturating_sub(Duration::from_millis(shared.last_request_ms.load(Ordering::SeqCst)));
        let use_ms = shared.last_use_ms.load(Ordering::SeqCst);
        let unused = since_start.saturating_sub(Duration::from_millis(use_ms));
        // Asleep: after any activity has settled, return the memory it freed
        // (allocators keep freed pages; this is what keeps a sleeper small).
        if shared.sleep.asleep.load(Ordering::SeqCst) && shared.in_flight.load(Ordering::SeqCst) == 0 && !shared.history.busy() {
            let mark = Some((shared.requests.load(Ordering::Relaxed), shared.lock().delivered));
            let mut released = shared.sleep.released.lock().unwrap_or_else(|p| p.into_inner());
            if *released != mark {
                *released = mark;
                drop(released);
                release_free_memory();
            }
        }
        if let Some(timeout) = timeout.filter(|t| !t.is_zero())
            && idle >= timeout
            && shared.in_flight.load(Ordering::SeqCst) == 0
        {
            eprintln!("ripple: no requests for {}s; exiting", idle.as_secs());
            begin_shutdown(&shared);
            return;
        }
        if let Some(after) = sleep_after.filter(|t| !t.is_zero())
            && unused >= after
            && !shared.sleep.asleep.load(Ordering::SeqCst)
            && shared.in_flight.load(Ordering::SeqCst) == 0
            && !shared.history.busy()
            && (!shared.sleep.reexec || *shared.conns.lock().unwrap_or_else(|p| p.into_inner()) == 0)
        {
            let mut st = shared.lock();
            let quiet = st.dirty.is_empty()
                && st.in_progress.is_none()
                && st.applied == st.delivered
                && st.reload_done == st.reload_requested
                && st.cookies.is_empty()
                && st.fatal.is_none()
                && st.last_event.is_none_or(|t| t.elapsed() >= after);
            let failed = *shared.sleep.failed_at.lock().unwrap_or_else(|p| p.into_inner()) == Some((use_ms, st.delivered));
            if quiet && !failed && !st.sleep_requested {
                st.sleep_requested = true;
                shared.cv.notify_all();
            }
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
        // `stats` is monitoring, not use: it does not hold off sleep.
        #[derive(serde::Deserialize)]
        struct Method<'a> {
            #[serde(borrow)]
            method: std::borrow::Cow<'a, str>,
        }
        let counts = serde_json::from_str::<Method>(&line).map_or(true, |m| m.method != "stats");
        let touch = || {
            let now = shared.started.elapsed().as_millis() as u64;
            shared.last_request_ms.store(now, Ordering::SeqCst);
            if counts {
                shared.last_use_ms.store(now, Ordering::SeqCst);
            }
        };
        shared.in_flight.fetch_add(1, Ordering::SeqCst);
        touch();
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle_line(shared, &line, start)))
            .unwrap_or_else(|_| err_line(&Value::Null, shared.generation(), 0, &ErrorBody::new("internal", "request handler panicked")));
        touch();
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
                "sleep": {
                    "asleep": shared.sleep.asleep.load(Ordering::Relaxed),
                    "sleeps": shared.sleep.sleeps.load(Ordering::Relaxed),
                    "wakes": shared.sleep.wakes.load(Ordering::Relaxed),
                    "snapshot": shared.sleep.path,
                    "last": shared.sleep.last.lock().unwrap_or_else(|p| p.into_inner()).clone(),
                },
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
