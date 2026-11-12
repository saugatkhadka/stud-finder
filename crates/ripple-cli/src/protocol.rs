//! Daemon wire protocol.
//!
//! Newline-delimited JSON over a Unix domain socket. A client sends one JSON
//! object per line and receives exactly one response line per request, in
//! order, on the same connection (open several connections for parallelism).
//!
//! Request:  `{"id": <any>, "method": "<name>", "params": {...}}`
//! Response: `{"id": <same>, "ok": true,  "protocol": 2, "generation": N, "server_us": N, "result": ...}`
//!       or  `{"id": <same>, "ok": false, "protocol": 2, "generation": N, "server_us": N, "error": {"code": "...", "message": "..."}}`
//!
//! `protocol` is [`PROTOCOL`]; daemons from before it existed omit it
//! (read as 0) and ignore `impact`'s `diff` parameter.
//!
//! `generation` is the index generation the answer was computed from (0
//! before the first index is ready). `server_us` is the time from reading
//! the request line to having the response ready, including the freshness
//! check, the query, policy evaluation and rendering.
//!
//! Methods and params:
//! * `impact {files?: [path], changed?: [ChangedFile], diff?: {base?: ref, staged?: bool}, scopes?: [name], file_level?, max_depth?, min_confidence?, format?: json|text|markdown, color?, limit?, all?, policy?, max_listed?, sync?, no_wait?, max_wait_ms?}`
//!   → the [`ripple_policy::Report`] for `format: json` (default), else a string.
//!   Lists are cut to their default budget unless `all` (or `limit`);
//!   `policy` always includes the policy section (`check` sets both).
//!   `diff` asks the daemon to find the changed files itself, exactly as
//!   `ripple impact [--base REF | --staged]` would (`{}` = working tree vs
//!   HEAD); it excludes `files` and `changed`.
//!   A `changed` entry may carry `hunks` (new-side `git diff -U0` hunks);
//!   the daemon maps them onto the file's method
//!   scopes from its own index, so line numbers always match what it indexed.
//!   `scopes` limits the bare `files` (exactly one) to those methods / actions.
//!   `file_level: true` ignores every scope (seeds and edges).
//! * `dependents {file, scopes?, depth?, min_confidence?, all?, limit?}` → the
//!   [`ripple_policy::Report`] of `file` (or those `scopes` of it) as if it
//!   had changed: the same model as `impact`.
//! * `dependencies {file, depth? = 1, min_confidence?}` → the files `file` depends on.
//!   Every query also takes `sync?`, `no_wait?`, `max_wait_ms?`.
//! * `why {from, to, scopes?}` → shortest dependency path between two files
//!   (`scopes`: only through these methods / actions of `to`).
//! * `areas {file}` → how the policy classifies a file.
//! * `stats` → index and daemon statistics. `reload` → full re-index. `shutdown`.
//!
//! **Freshness contract.** Every query answer reflects every file edit that
//! completed before the request was sent, or says which ones it does not.
//! Before answering, the daemon (1) writes a cookie file and waits for the
//! file watcher to report it, so every earlier event has been delivered
//! (skipped with `no_wait`; always done for `diff`), (2) re-stats the files
//! named in the request (`files`, `changed[].path`, `file`, `from`, `to`)
//! and marks any that changed since they were indexed, and (3) waits for
//! all of that to be indexed, for at most `daemon.max_wait_ms` (default
//! 150; `max_wait_ms` overrides it per request). If the index has not caught
//! up by then (or is predicted not to, from the last pass's duration), it
//! answers from the current snapshot and the response says so:
//!
//! `{"id": .., "ok": true, ..., "stale": true, "synced": true, "pending_count": 3, "pending": ["app/a.rb", ...], "result": ...}`
//!
//! * `stale`: some change made before the request is not in this answer.
//! * `pending`: changed but not yet indexed paths (at most
//!   [`MAX_PENDING_LISTED`], sorted); `pending_count`: all of them.
//! * `synced`: the cookie round trip completed, so `pending` is complete;
//!   `false` with `no_wait`, without a file watcher, or if the watcher did
//!   not answer in time (then only the named files were checked).
//!
//! In a stale `impact` answer, a changed file that is itself pending is
//! treated as wholly changed: its diff hunks are not mapped onto method
//! ranges that were indexed from older contents.
//!
//! **Git history.** An `impact` answer carries one more envelope field,
//! `"history": {"state": "ready|building|disabled|unavailable", "reason"?,
//! "head"?, "commits", "churn_available", "cochange", "cochange_reason"?,
//! "updating"?, "recent_days"?, "churn"?: {path: {"commits",
//! "recent_commits", "last_changed"?}}, "usually_changes_with"?: [{"path",
//! "via", "together", "of", "score", "lift", "last_together", "seeds"}],
//! "lookup_us"?}` ([`crate::historyd::HistoryInfo`]); it never waits for
//! history to be built. `stats` has the status (and build details) under
//! `result.history`.
//!
//! `sync: true` waits without a time limit and is never stale (it fails
//! with `sync_timeout` if the watcher does not answer within 2 s).
//! `no_wait: true` never waits. `stats` never waits.

use crate::git::DiffSpec;
use ripple_core::Confidence;
use ripple_policy::ChangedFile;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

impl ErrorBody {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.to_string(), message: message.into() }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    #[default]
    Json,
    Text,
    Markdown,
}

/// Version of this protocol, sent in every response.
pub const PROTOCOL: u32 = 2;

/// Which git diff an `impact` request means (see [`DiffSpec`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiffRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub staged: bool,
}

impl DiffRequest {
    pub fn from_spec(spec: &DiffSpec) -> Self {
        match spec {
            DiffSpec::WorkingTree => Self::default(),
            DiffSpec::Staged => Self { base: None, staged: true },
            DiffSpec::Base(b) => Self { base: Some(b.clone()), staged: false },
        }
    }
    pub fn spec(&self) -> DiffSpec {
        match (&self.base, self.staged) {
            (Some(b), _) => DiffSpec::Base(b.clone()),
            (None, true) => DiffSpec::Staged,
            (None, false) => DiffSpec::WorkingTree,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ImpactParams {
    /// Changed paths without diff details (status inferred from the file system).
    pub files: Vec<String>,
    /// Changed files with status / line counts / hunks.
    pub changed: Vec<ChangedFile>,
    /// Let the daemon compute `changed` from git.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffRequest>,
    /// Only these methods / actions of the (single) file in `files` changed.
    pub scopes: Vec<String>,
    /// Ignore method / action scopes on seeds and edges.
    pub file_level: bool,
    pub max_depth: Option<u32>,
    pub min_confidence: Option<Confidence>,
    pub format: Format,
    /// ANSI colours for `format: text`.
    pub color: bool,
    /// Cut every list to at most this many entries (counts stay complete).
    pub limit: Option<usize>,
    /// No list budget: every dependent, further file, test and mention.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub all: bool,
    /// Always include the policy section (risk, rules, owners, areas) with
    /// full evidence (`check`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub policy: bool,
    /// Files listed by text/markdown output (default `ci.max_listed`).
    pub max_listed: Option<usize>,
    pub sync: bool,
    /// Answer from the current index at once (see the freshness contract).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub no_wait: bool,
    /// Overrides `daemon.max_wait_ms` for this request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_wait_ms: Option<u64>,
}

impl ImpactParams {
    pub fn named_files(&self) -> Vec<String> {
        let mut v: Vec<String> = self.files.clone();
        for c in &self.changed {
            v.push(c.path.clone());
            v.extend(c.old_path.clone());
        }
        v
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FileParams {
    pub file: String,
    /// `dependents` only: start from these methods / actions of `file`.
    pub scopes: Vec<String>,
    pub depth: Option<u32>,
    pub min_confidence: Option<Confidence>,
    /// `dependents` only: no list budget.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub all: bool,
    /// `dependents` only: cut every list to this many entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    pub sync: bool,
    pub no_wait: bool,
    pub max_wait_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WhyParams {
    pub from: String,
    pub to: String,
    /// Only paths that go through these methods / actions of `to`.
    pub scopes: Vec<String>,
    pub sync: bool,
    pub no_wait: bool,
    pub max_wait_ms: Option<u64>,
}

/// How long a request may wait for the index (see the freshness contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitMode {
    /// Do not wait (and do not sync with the watcher).
    NoWait,
    /// Wait at most this long, then answer marked stale.
    Bounded(std::time::Duration),
    /// Wait until everything before the request is indexed.
    Full,
}

impl WaitMode {
    /// From a request's `sync` / `no_wait` / `max_wait_ms`; `default_ms` is `daemon.max_wait_ms`.
    pub fn from_params(sync: bool, no_wait: bool, max_wait_ms: Option<u64>, default_ms: u64) -> Self {
        if sync {
            WaitMode::Full
        } else if no_wait {
            WaitMode::NoWait
        } else {
            WaitMode::Bounded(std::time::Duration::from_millis(max_wait_ms.unwrap_or(default_ms)))
        }
    }
}

/// At most this many pending paths are listed in a response.
pub const MAX_PENDING_LISTED: usize = 50;

/// How fresh an answer is (sent in the response envelope; see the
/// freshness contract).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Freshness {
    /// Some change made before the request is not reflected in the answer.
    pub stale: bool,
    /// Every file event before the request was observed (cookie round trip).
    pub synced: bool,
    /// Changed but not yet indexed paths, sorted, at most [`MAX_PENDING_LISTED`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<String>,
    /// All pending paths.
    pub pending_count: usize,
}

/// Builds a response line (without the trailing newline). `result` must be valid JSON.
pub fn ok_line(id: &Value, generation: u64, server_us: u64, result: &str) -> String {
    ok_line_fresh(id, generation, server_us, None, result)
}

/// [`ok_line`] with the freshness fields of a query answer.
pub fn ok_line_fresh(id: &Value, generation: u64, server_us: u64, fresh: Option<&Freshness>, result: &str) -> String {
    ok_line_history(id, generation, server_us, fresh, None, result)
}

/// [`ok_line_fresh`] plus the `history` object of an `impact` answer
/// (valid JSON; see [`crate::historyd::HistoryInfo`]).
pub fn ok_line_history(id: &Value, generation: u64, server_us: u64, fresh: Option<&Freshness>, history: Option<&str>, result: &str) -> String {
    let id = serde_json::to_string(id).unwrap_or_else(|_| "null".into());
    let mut s = String::with_capacity(result.len() + 160);
    s.push_str("{\"id\":");
    s.push_str(&id);
    s.push_str(&format!(",\"ok\":true,\"protocol\":{PROTOCOL},\"generation\":{generation},\"server_us\":{server_us}"));
    if let Some(f) = fresh {
        s.push_str(&format!(",\"stale\":{},\"synced\":{},\"pending_count\":{}", f.stale, f.synced, f.pending_count));
        if !f.pending.is_empty() {
            s.push_str(",\"pending\":");
            s.push_str(&serde_json::to_string(&f.pending).unwrap_or_else(|_| "[]".into()));
        }
    }
    if let Some(h) = history {
        s.push_str(",\"history\":");
        s.push_str(h);
    }
    s.push_str(",\"result\":");
    s.push_str(result);
    s.push('}');
    s
}

pub fn err_line(id: &Value, generation: u64, server_us: u64, err: &ErrorBody) -> String {
    serde_json::json!({"id": id, "ok": false, "protocol": PROTOCOL, "generation": generation, "server_us": server_us, "error": err}).to_string()
}

/// A parsed response; `result` is kept as raw JSON text so large reports are
/// passed through without re-serialisation.
#[derive(Debug, Deserialize)]
pub struct Response<'a> {
    #[serde(default)]
    pub id: Value,
    pub ok: bool,
    #[serde(default)]
    pub protocol: u32,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub server_us: u64,
    /// Freshness fields (absent from daemons before them, and on errors).
    #[serde(default)]
    pub stale: bool,
    #[serde(default)]
    pub synced: Option<bool>,
    #[serde(default)]
    pub pending: Vec<String>,
    #[serde(default)]
    pub pending_count: usize,
    /// Git-history signals of an `impact` answer (absent from older daemons).
    #[serde(borrow, default)]
    pub history: Option<&'a serde_json::value::RawValue>,
    #[serde(borrow, default)]
    pub result: Option<&'a serde_json::value::RawValue>,
    #[serde(default)]
    pub error: Option<ErrorBody>,
}
