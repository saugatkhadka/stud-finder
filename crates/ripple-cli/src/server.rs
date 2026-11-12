//! The resident daemon: one per repo root, serving the protocol in
//! [`crate::protocol`] over a Unix socket while a watcher keeps the index fresh.
//!
//! Threads: one accept loop, one thread per connection (requests on a
//! connection are answered in order; connections run concurrently), one
//! indexer thread that owns the [`Engine`], and the watcher's own thread.
//!
//! Queries never take a lock that the indexer holds while indexing: the
//! current snapshot is an `Arc` behind an `RwLock` held only to clone or
//! replace the `Arc`, and the pending-change bookkeeping is a separate mutex
//! held only for bookkeeping. See [`crate::protocol`] for the freshness contract.

use crate::engine::{Engine, Pipeline, Snapshot};
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

/// Quiet period after the last watcher event before an unprompted re-index.
pub const DEBOUNCE: Duration = Duration::from_millis(30);
/// Upper bound on how long continuous events can postpone a re-index.
pub const MAX_DEBOUNCE: Duration = Duration::from_millis(300);
/// How long a request waits for the index to catch up before answering anyway.
const MAX_WAIT: Duration = Duration::from_secs(300);
const COOKIE_PREFIX: &str = "ripple-cookie-";

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
        let mut st = self.lock();
        let mut any = false;
        for p in paths {
            if p.starts_with(COOKIE_PREFIX) || p.contains(&format!("/{COOKIE_PREFIX}")) {
                st.cookies.insert(p);
                any = true;
                continue;
            }
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

pub struct ServeOptions {
    /// Install SIGTERM/SIGINT/SIGHUP handlers that remove the socket (binary only).
    pub signals: bool,
}

/// Runs the daemon until a `shutdown` request. Fails fast if another daemon
/// holds the lock for this root.
pub fn serve(root: &Path, pipeline: Pipeline, paths: DaemonPaths, opts: ServeOptions) -> Result<()> {
    paths.create_dir()?;
    let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&paths.lock).with_context(|| format!("opening {}", paths.lock.display()))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            bail!("a ripple daemon is already serving {} (pid {})", root.display(), paths.recorded_pid().map(|p| p.to_string()).unwrap_or("?".into()))
        }
        Err(std::fs::TryLockError::Error(e)) => return Err(e).context("locking the daemon lock file"),
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
    });

    let _watcher = start_watcher(&shared).map_err(|e| eprintln!("ripple: file watching unavailable ({e:#}); only re-stat freshness applies")).ok();
    {
        let shared = shared.clone();
        std::thread::Builder::new().name("ripple-indexer".into()).spawn(move || indexer(shared, pipeline))?;
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
        let shared = shared.clone();
        let _ = std::thread::Builder::new().name("ripple-conn".into()).spawn(move || handle_conn(stream, &shared));
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
        Err(e) => eprintln!("ripple: watcher error: {e}"),
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

fn handle_conn(stream: UnixStream, shared: &Arc<Shared>) {
    let Ok(read) = stream.try_clone() else { return };
    let reader = BufReader::new(read);
    let mut writer = std::io::BufWriter::new(stream);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let start = Instant::now();
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle_line(shared, &line, start)))
            .unwrap_or_else(|_| err_line(&Value::Null, shared.generation(), 0, &ErrorBody::new("internal", "request handler panicked")));
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
            let p: ImpactParams = params(&req.params)?;
            let f = refresh(shared, &p.named_files(), p.sync)?;
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
            let mut st = shared.lock();
            st.shutdown = true;
            shared.cv.notify_all();
            drop(st);
            // Wake the accept loop so it can exit.
            let _ = UnixStream::connect(&shared.paths.socket);
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
