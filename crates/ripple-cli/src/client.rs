//! Talking to the daemon, starting it on demand, and the in-process fallback.

use crate::engine::{Engine, Pipeline, Snapshot};
use crate::paths::DaemonPaths;
use crate::protocol::{ErrorBody, Freshness, ImpactParams, PROTOCOL, Response};
use crate::query;
use crate::signals::Sources;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A response with its `result` still as raw JSON text.
#[derive(Debug)]
pub struct Reply {
    /// The daemon's [`PROTOCOL`] version (0 for daemons that predate it).
    pub protocol: u32,
    pub generation: u64,
    pub server_us: u64,
    /// How fresh the answer is (see the freshness contract in
    /// [`crate::protocol`]). In-process answers are always fresh.
    pub fresh: Freshness,
    pub result: Result<String, ErrorBody>,
    /// Git-history signals of an `impact` answer (see [`crate::historyd`]);
    /// `None` for other methods and daemons that predate it.
    pub history: Option<crate::historyd::HistoryInfo>,
}

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    line: String,
}

impl Client {
    /// Connects to the daemon for these paths; `None` when none is listening
    /// (no socket, or a stale socket left by a dead daemon).
    pub fn connect(paths: &DaemonPaths) -> Option<Self> {
        let stream = UnixStream::connect(&paths.socket).ok()?;
        let reader = BufReader::new(stream.try_clone().ok()?);
        Some(Self { reader, writer: stream, next_id: 1, line: String::new() })
    }

    pub fn call(&mut self, method: &str, params: &Value) -> Result<Reply> {
        let id = self.next_id;
        self.next_id += 1;
        let req = serde_json::json!({"id": id, "method": method, "params": params});
        let mut bytes = serde_json::to_vec(&req)?;
        bytes.push(b'\n');
        self.writer.write_all(&bytes).context("sending request to the ripple daemon")?;
        self.line.clear();
        let n = self.reader.read_line(&mut self.line).context("reading the daemon's response")?;
        if n == 0 {
            bail!("the ripple daemon closed the connection (it may have crashed; see `ripple daemon status`)");
        }
        let resp: Response = serde_json::from_str(&self.line).context("parsing the daemon's response")?;
        let result = if resp.ok {
            Ok(resp.result.map(|r| r.get().to_string()).unwrap_or_else(|| "null".into()))
        } else {
            Err(resp.error.unwrap_or_else(|| ErrorBody::new("unknown", "error without details")))
        };
        let fresh = Freshness { stale: resp.stale, synced: resp.synced.unwrap_or(false), pending: resp.pending, pending_count: resp.pending_count };
        let history = resp.history.and_then(|h| serde_json::from_str(h.get()).ok());
        Ok(Reply { protocol: resp.protocol, generation: resp.generation, server_us: resp.server_us, fresh, result, history })
    }
}

/// Autostart is off when `RIPPLE_NO_DAEMON` is set (to anything but `0`) or in CI.
pub fn autostart_allowed() -> bool {
    let set = |k: &str| std::env::var(k).is_ok_and(|v| !v.is_empty() && v != "0" && v != "false");
    !(set("RIPPLE_NO_DAEMON") || set("CI") || set("GITHUB_ACTIONS"))
}

/// Starts `ripple serve --foreground` detached from this process (own
/// process group, stdio redirected to the daemon log).
pub fn spawn_daemon(root: &Path, paths: &DaemonPaths) -> Result<()> {
    paths.create_dir()?;
    let exe = std::env::current_exe().context("locating the ripple executable")?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&paths.log)?;
    let mut cmd = Command::new(exe);
    cmd.arg("serve").arg("--foreground").arg("--root").arg(root);
    if let Some(cfg) = ripple_policy::config_override() {
        cmd.arg("--config").arg(cfg);
    }
    cmd.stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()
        .context("starting the ripple daemon")?;
    Ok(())
}

/// Polls until the daemon accepts connections.
pub fn wait_for_daemon(paths: &DaemonPaths, timeout: Duration) -> Option<Client> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(c) = Client::connect(paths) {
            return Some(c);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Where requests go.
pub enum Backend {
    Daemon(DaemonBackend),
    /// Indexed in this process (`--no-daemon`, CI, or autostart unavailable).
    Local(Arc<Snapshot>),
}

pub struct DaemonBackend {
    client: Client,
    root: std::path::PathBuf,
    autostart: bool,
    /// A request has been answered on this connection.
    answered: bool,
}

impl Backend {
    /// Connects to a running daemon, else (if `autostart`) starts one and
    /// waits for it, else indexes in-process.
    pub fn open(root: &Path, no_daemon: bool, autostart: bool) -> Result<Self> {
        Ok(match Self::open_client(root, no_daemon, autostart) {
            Some(client) => Backend::Daemon(DaemonBackend { client, root: root.to_path_buf(), autostart, answered: false }),
            None => {
                let (_, snap) = Engine::open(root, Pipeline::standard())?;
                Backend::Local(snap)
            }
        })
    }

    fn open_client(root: &Path, no_daemon: bool, autostart: bool) -> Option<Client> {
        let paths = if no_daemon {
            None
        } else {
            match DaemonPaths::for_root(root) {
                Ok(p) => Some(p),
                Err(e) => {
                    eprintln!("ripple: {e:#}; running in-process");
                    None
                }
            }
        };
        let paths = paths?;
        if let Some(c) = Client::connect(&paths) {
            return Some(c);
        }
        if autostart && autostart_allowed() {
            match spawn_daemon(root, &paths) {
                Ok(()) => {
                    if let Some(c) = wait_for_daemon(&paths, Duration::from_secs(10)) {
                        return Some(c);
                    }
                    eprintln!("ripple: daemon did not start (see {}); running in-process", paths.log.display());
                }
                Err(e) => eprintln!("ripple: could not start daemon ({e:#}); running in-process"),
            }
        }
        None
    }

    pub fn is_daemon(&self) -> bool {
        matches!(self, Backend::Daemon(_))
    }

    pub fn call(&mut self, method: &str, params: &Value) -> Result<Reply> {
        match self {
            Backend::Daemon(d) => match d.client.call(method, params) {
                Ok(r) => {
                    d.answered = true;
                    Ok(r)
                }
                // A daemon that exits (idle timeout, `daemon stop`) can close a
                // connection it already accepted: reconnect (or start a new
                // daemon) and retry once. Only the first request on a
                // connection is retried, so a request that crashed the daemon
                // is not sent twice.
                Err(e) if !d.answered && method != "shutdown" => {
                    std::thread::sleep(Duration::from_millis(20));
                    match Self::open_client(&d.root, false, d.autostart) {
                        Some(c) => {
                            d.client = c;
                            let r = d.client.call(method, params)?;
                            d.answered = true;
                            Ok(r)
                        }
                        None => Err(e),
                    }
                }
                Err(e) => Err(e),
            },
            Backend::Local(snap) => {
                let root = snap.root.clone();
                let mut history = None;
                let (result, us) = query::timed(|| match method {
                    "impact" => serde_json::from_value::<ImpactParams>(params.clone())
                        .map_err(|e| ErrorBody::new("bad_params", e.to_string()))
                        .and_then(|mut p| {
                            // In-process: the reference git implementation.
                            if let Some(d) = p.diff.take() {
                                p.changed = crate::git::changed_files(&root, &d.spec()).map_err(|e| ErrorBody::new("git_failed", format!("{e:#}")))?;
                            }
                            // The history sidecar and coverage reports, read now:
                            // one-shot mode never builds history or runs tests.
                            let (out, h) = query::impact_result(snap, &p, 0, 0, &Sources::oneshot(snap))?;
                            history = Some(h);
                            Ok(out)
                        }),
                    "stats" => {
                        let mut v = query::stats(snap);
                        v["history"] = crate::historyd::oneshot_view(&root, &snap.policy.config).stats_json();
                        Ok(v.to_string())
                    }
                    "reload" | "shutdown" => Err(ErrorBody::new("no_daemon", "no daemon is running")),
                    m @ ("dependents" | "file" | "rank") => query::execute(snap, m, params, 0, &Sources::oneshot(snap)),
                    m => query::execute(snap, m, params, 0, &Sources::none()),
                });
                let fresh = Freshness { synced: true, ..Freshness::default() };
                Ok(Reply { protocol: PROTOCOL, generation: snap.generation, server_us: us, fresh, result, history })
            }
        }
    }
}
