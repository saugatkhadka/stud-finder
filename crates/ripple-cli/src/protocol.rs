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
//! * `impact {files?: [path], changed?: [ChangedFile], diff?: {base?: ref, staged?: bool}, scopes?: [name], file_level?, max_depth?, min_confidence?, format?: json|text|markdown, color?, limit?, all?, policy?, max_listed?, sync?}`
//!   → the [`ripple_policy::Report`] for `format: json` (default), else a string.
//!   Lists are cut to their default budget unless `all` (or `limit`);
//!   `policy` always includes the policy section (`check` sets both).
//!   `diff` asks the daemon to find the changed files itself, exactly as
//!   `ripple impact [--base REF | --staged]` would (`{}` = working tree vs
//!   HEAD); it excludes `files` and `changed`, and implies `sync`.
//!   A `changed` entry may carry `hunks` (new-side `git diff -U0` hunks);
//!   the daemon maps them onto the file's method
//!   scopes from its own index, so line numbers always match what it indexed.
//!   `scopes` limits the bare `files` (exactly one) to those methods / actions.
//!   `file_level: true` ignores every scope (seeds and edges).
//! * `dependents {file, scopes?, depth?, min_confidence?, all?, limit?}` → the
//!   [`ripple_policy::Report`] of `file` (or those `scopes` of it) as if it
//!   had changed: the same model as `impact`.
//! * `dependencies {file, depth? = 1, min_confidence?}` → the files `file` depends on.
//! * `why {from, to, scopes?}` → shortest dependency path between two files
//!   (`scopes`: only through these methods / actions of `to`).
//! * `areas {file}` → how the policy classifies a file.
//! * `stats` → index and daemon statistics. `reload` → full re-index. `shutdown`.
//!
//! **Freshness contract.** Before answering, the daemon (1) folds every file
//! event the watcher has already delivered into its pending set, (2) re-stats
//! the files named in the request (`files`, `changed[].path`, `file`, `from`,
//! `to`) and marks any that changed since they were indexed, and (3) waits
//! until all of that is indexed. The one exception: if a re-index is already
//! running and none of the request's files are pending, it answers from the
//! current snapshot immediately and reports the number of not-yet-indexed
//! changes in the result's `index.pending_changes` (impact only). With
//! `sync: true` it additionally writes a cookie file and waits for the
//! watcher to report it, which guarantees that *every* change made before
//! the request (not just to the named files) is indexed.

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
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WhyParams {
    pub from: String,
    pub to: String,
    /// Only paths that go through these methods / actions of `to`.
    pub scopes: Vec<String>,
    pub sync: bool,
}

/// Builds a response line (without the trailing newline). `result` must be valid JSON.
pub fn ok_line(id: &Value, generation: u64, server_us: u64, result: &str) -> String {
    let id = serde_json::to_string(id).unwrap_or_else(|_| "null".into());
    let mut s = String::with_capacity(result.len() + 96);
    s.push_str("{\"id\":");
    s.push_str(&id);
    s.push_str(&format!(",\"ok\":true,\"protocol\":{PROTOCOL},\"generation\":{generation},\"server_us\":{server_us},\"result\":"));
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
    #[serde(borrow, default)]
    pub result: Option<&'a serde_json::value::RawValue>,
    #[serde(default)]
    pub error: Option<ErrorBody>,
}
