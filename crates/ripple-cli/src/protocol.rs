//! Daemon wire protocol.
//!
//! Newline-delimited JSON over a Unix domain socket. A client sends one JSON
//! object per line and receives exactly one response line per request, in
//! order, on the same connection (open several connections for parallelism).
//!
//! Request:  `{"id": <any>, "method": "<name>", "params": {...}}`
//! Response: `{"id": <same>, "ok": true,  "generation": N, "server_us": N, "result": ...}`
//!       or  `{"id": <same>, "ok": false, "generation": N, "server_us": N, "error": {"code": "...", "message": "..."}}`
//!
//! `generation` is the index generation the answer was computed from (0
//! before the first index is ready). `server_us` is the time from reading
//! the request line to having the response ready, including the freshness
//! check, the query, policy evaluation and rendering.
//!
//! Methods and params:
//! * `impact {files?: [path], changed?: [ChangedFile], scopes?: [name], file_level?, max_depth?, min_confidence?, format?: json|text|markdown, color?, limit?, max_listed?, sync?}`
//!   → the [`ripple_policy::Report`] for `format: json` (default), else a string.
//!   A `changed` entry may carry `hunks` (new-side `git diff -U0` hunks,
//!   computed by the client); the daemon maps them onto the file's method
//!   scopes from its own index, so line numbers always match what it indexed.
//!   `scopes` limits the bare `files` (exactly one) to those methods / actions.
//!   `file_level: true` ignores every scope (seeds and edges).
//! * `dependents {file, scopes?, depth? = 1, min_confidence?}` / `dependencies {file, depth? = 1, min_confidence?}`
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ImpactParams {
    /// Changed paths without diff details (status inferred from the file system).
    pub files: Vec<String>,
    /// Changed files with status / line counts / hunks.
    pub changed: Vec<ChangedFile>,
    /// Only these methods / actions of the (single) file in `files` changed.
    pub scopes: Vec<String>,
    /// Ignore method / action scopes on seeds and edges.
    pub file_level: bool,
    pub max_depth: Option<u32>,
    pub min_confidence: Option<Confidence>,
    pub format: Format,
    /// ANSI colours for `format: text`.
    pub color: bool,
    /// Truncate the impacted file list (and tests) to this many entries.
    pub limit: Option<usize>,
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
    s.push_str(&format!(",\"ok\":true,\"generation\":{generation},\"server_us\":{server_us},\"result\":"));
    s.push_str(result);
    s.push('}');
    s
}

pub fn err_line(id: &Value, generation: u64, server_us: u64, err: &ErrorBody) -> String {
    serde_json::json!({"id": id, "ok": false, "generation": generation, "server_us": server_us, "error": err}).to_string()
}

/// A parsed response; `result` is kept as raw JSON text so large reports are
/// passed through without re-serialisation.
#[derive(Debug, Deserialize)]
pub struct Response<'a> {
    #[serde(default)]
    pub id: Value,
    pub ok: bool,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub server_us: u64,
    #[serde(borrow, default)]
    pub result: Option<&'a serde_json::value::RawValue>,
    #[serde(default)]
    pub error: Option<ErrorBody>,
}
