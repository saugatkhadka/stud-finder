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

use crate::engine::{Engine, Pipeline, Snapshot};
use crate::gitd::GitTracker;
use crate::paths::DaemonPaths;
use crate::protocol::{ErrorBody, FileParams, ImpactParams, Request, WhyParams, err_line, ok_line};
use crate::query;
use anyhow::{Context, Result, bail};
use ripple_core::RelPath;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashSet};
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
/// How long a request waits for the index to catch up before answering anyway.
const MAX_WAIT: Duration = Duration::from_secs(300);
const COOKIE_PREFIX: &str = "ripple-cookie-";
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
    cookies: HashSet<String>,
    shutdown: bool,
    fatal: Option<String>,
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
    requests: AtomicU64,
    git: GitTracker,
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

    fn deliver(&self, paths: impl IntoIterator<Item = RelPath>) {
        let (cookies, paths): (Vec<RelPath>, Vec<RelPath>) = paths.into_iter().partition(|p| p.starts_with(COOKIE_PREFIX) || p.contains(&format!("/{COOKIE_PREFIX}")));
        // Before the cookies: a request that sees its cookie must find every
        // earlier path in the git log too.
        self.git.log.push(&paths);
        let mut st = self.lock();
        let mut any = !cookies.is_empty();
        st.cookies.extend(cookies);
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

    let cookie_dir = if root.join(".git").is_dir() { root.join(".git") } else { root.to_path_buf() };
    let shared = Arc::new(Shared {
        root: root.to_path_buf(),
        paths: paths.clone(),
        state: Mutex::new(State::default()),
        cv: Condvar::new(),
        snapshot: RwLock::new(None),
        started: Instant::now(),
        cookie_dir,
        cookie_seq: AtomicU64::new(0),
        requests: AtomicU64::new(0),
        git: GitTracker::new(root),
        last_request_ms: AtomicU64::new(0),
        in_flight: AtomicUsize::new(0),
        conns: Mutex::new(0),
        conns_cv: Condvar::new(),
        idle_timeout: opts.idle_timeout,
    });

    let _watcher = start_watcher(&shared).map_err(|e| eprintln!("ripple: file watching unavailable ({e:#}); only re-stat freshness applies")).ok();
    // Git results are only cached while the watcher reports changes.
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
            let rels: Vec<RelPath> = rels
                .filter(|r| {
                    if r.contains(COOKIE_PREFIX) {
                        // Only the creation matters; ignore the cookie's own deletion.
                        return s.root.join(r).exists();
                    }
                    r != ".git" && !r.starts_with(".git/")
                })
                .map(|r| if r.contains(COOKIE_PREFIX) { r.rsplit('/').next().unwrap_or(&r).to_string() } else { r })
                .collect();
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
        let batch = std::mem::take(&mut st.dirty);
        let target = st.delivered;
        st.in_progress = Some(batch.clone());
        st.urgent = false;
        st.first_event = None;
        drop(st);

        if std::env::var_os("RIPPLE_DEBUG").is_some() {
            eprintln!("ripple: indexing {} path(s){}: {:?}", batch.len(), if reload.is_some() { " (full reload)" } else { "" }, batch.iter().take(20).collect::<Vec<_>>());
        }
        let result = if reload.is_some() { engine.full_build() } else { engine.apply_paths(&batch) };
        match result {
            Ok(snap) => shared.set_current(snap),
            Err(e) => eprintln!("ripple: re-index failed: {e:#}"),
        }
        let mut st = shared.lock();
        st.applied = target;
        st.in_progress = None;
        if let Some(r) = reload {
            st.reload_done = r;
        }
        shared.cv.notify_all();
    }
}

/// Outcome of the freshness step for one request.
struct Fresh {
    snap: Arc<Snapshot>,
    /// Changes known but not indexed (only when served during a busy re-index).
    pending: usize,
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

/// Writes a cookie file and waits until the watcher reports it: every file
/// event that happened before this call has then been delivered.
fn cookie_sync(shared: &Shared) -> Result<(), ErrorBody> {
    let n = shared.cookie_seq.fetch_add(1, Ordering::Relaxed);
    let name = format!("{COOKIE_PREFIX}{}-{n}", std::process::id());
    let path = shared.cookie_dir.join(&name);
    std::fs::write(&path, b"").map_err(|e| ErrorBody::new("sync_failed", format!("writing {}: {e}", path.display())))?;
    let st = shared.lock();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut st = st;
    let mut ok = true;
    while !st.cookies.contains(&name) {
        let now = Instant::now();
        if now >= deadline {
            ok = false;
            break;
        }
        st = shared.cv.wait_timeout(st, deadline - now).unwrap_or_else(|p| p.into_inner()).0;
    }
    st.cookies.remove(&name);
    drop(st);
    let _ = std::fs::remove_file(&path);
    if ok { Ok(()) } else { Err(ErrorBody::new("sync_timeout", "the file watcher did not report the sync cookie within 2s")) }
}

/// The freshness step (see [`crate::protocol`]).
fn refresh(shared: &Shared, named: &[String], sync: bool) -> Result<Fresh, ErrorBody> {
    let t = Instant::now();
    if sync {
        cookie_sync(shared)?;
    }
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
    if st.applied >= target {
        drop(st);
        let snap = shared.current().unwrap_or(snap);
        return Ok(Fresh { snap, pending: 0, refresh_us: t.elapsed().as_micros() as u64 });
    }
    let relevant = named.iter().any(|f| st.dirty.contains(f) || st.in_progress.as_ref().is_some_and(|b| b.contains(f)));
    if st.in_progress.is_some() && !relevant {
        let pending = st.dirty.len() + st.in_progress.as_ref().map_or(0, |b| b.len());
        drop(st);
        let snap = shared.current().unwrap_or(snap);
        return Ok(Fresh { snap, pending, refresh_us: t.elapsed().as_micros() as u64 });
    }
    st.urgent = true;
    shared.cv.notify_all();
    let st = wait_for(shared, st, |s| s.applied >= target)?;
    drop(st);
    let snap = shared.current().unwrap_or(snap);
    Ok(Fresh { snap, pending: 0, refresh_us: t.elapsed().as_micros() as u64 })
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

fn handle_line(shared: &Arc<Shared>, line: &str, start: Instant) -> String {
    shared.requests.fetch_add(1, Ordering::Relaxed);
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => return err_line(&Value::Null, shared.generation(), 0, &ErrorBody::new("bad_request", format!("invalid request JSON: {e}"))),
    };
    let result: Result<(String, u64), ErrorBody> = (|| match req.method.as_str() {
        "impact" => {
            let mut p: ImpactParams = params(&req.params)?;
            let mut sync = p.sync;
            if let Some(d) = p.diff.take() {
                if !p.files.is_empty() || !p.changed.is_empty() {
                    return Err(ErrorBody::new("bad_params", "`diff` excludes `files` and `changed`"));
                }
                // The git cache trusts the watcher's log: make sure every
                // change made before this request is in it. If the watcher
                // does not answer, its log cannot be trusted.
                if shared.git.watching.load(Ordering::SeqCst) && cookie_sync(shared).is_err() {
                    shared.git.log.overflow();
                }
                p.changed = shared.git.changed(&d.spec()).map_err(|e| ErrorBody::new("git_failed", e))?;
                // The cookie round trip above already covers `sync`.
                sync = false;
            }
            let f = refresh(shared, &p.named_files(), sync)?;
            Ok((query::impact_result(&f.snap, &p, f.pending, f.refresh_us)?, f.snap.generation))
        }
        "dependents" | "dependencies" | "areas" => {
            let p: FileParams = params(&req.params)?;
            let f = refresh(shared, std::slice::from_ref(&p.file), p.sync)?;
            Ok((query::execute(&f.snap, &req.method, &req.params, f.pending)?, f.snap.generation))
        }
        "why" => {
            let p: WhyParams = params(&req.params)?;
            let f = refresh(shared, &[p.from.clone(), p.to.clone()], p.sync)?;
            Ok((query::execute(&f.snap, "why", &req.params, f.pending)?, f.snap.generation))
        }
        "stats" => {
            let f = refresh(shared, &[], false)?;
            let mut v = query::stats(&f.snap);
            let st = shared.lock();
            v["daemon"] = json!({
                "pid": std::process::id(),
                "version": env!("CARGO_PKG_VERSION"),
                "socket": shared.paths.socket,
                "uptime_ms": shared.started.elapsed().as_millis() as u64,
                "requests": shared.requests.load(Ordering::Relaxed),
                "pending_changes": st.dirty.len(),
                "indexing": st.in_progress.is_some(),
                "connections": *shared.conns.lock().unwrap_or_else(|p| p.into_inner()),
                "git": {
                    "watching": shared.git.watching.load(Ordering::Relaxed),
                    "cache_hits": shared.git.counters.hits.load(Ordering::Relaxed),
                    "partial_updates": shared.git.counters.partial.load(Ordering::Relaxed),
                    "full_updates": shared.git.counters.full.load(Ordering::Relaxed),
                    "last_us": shared.git.counters.last_us.load(Ordering::Relaxed),
                },
            });
            Ok((v.to_string(), f.snap.generation))
        }
        "reload" => {
            let mut st = shared.lock();
            st.reload_requested += 1;
            let want = st.reload_requested;
            shared.cv.notify_all();
            let st = wait_for(shared, st, |s| s.reload_done >= want)?;
            drop(st);
            let g = shared.generation();
            Ok((json!({"generation": g}).to_string(), g))
        }
        "shutdown" => {
            begin_shutdown(shared);
            Ok((json!({"stopping": true}).to_string(), shared.generation()))
        }
        other => Err(ErrorBody::new("unknown_method", format!("unknown method `{other}`"))),
    })();
    let us = start.elapsed().as_micros() as u64;
    match result {
        Ok((body, generation)) => ok_line(&req.id, generation, us, &body),
        Err(e) => err_line(&req.id, shared.generation(), us, &e),
    }
}
