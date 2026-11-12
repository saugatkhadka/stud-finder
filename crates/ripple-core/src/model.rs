//! The shared vocabulary every ripple crate speaks.
//!
//! The whole system is one directed graph over **repo-relative file paths**.
//! An [`Edge`] `from -> to` means "`from` depends on `to`": if `to` changes,
//! `from` may be affected. Impact analysis is therefore reverse reachability
//! from the changed files.

use serde::{Deserialize, Serialize};

/// Repo-relative path, forward slashes, no leading `./` (e.g. `app/models/user.rb`).
pub type RelPath = String;

/// Why `from` depends on `to`. Kept coarse on purpose: the kind is shown to
/// humans and agents as the *reason* a file is in the blast radius.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// Ruby file references a constant defined in the target file.
    RubyConstant,
    /// Explicit `require` / `require_relative` / `load`.
    RubyRequire,
    /// Rails DSL implies the constant (`belongs_to :user`, `class_name: "X"`,
    /// `"Foo".constantize`, `perform_later` on a job, serializer lookup, ...).
    RailsInferred,
    /// Controller/mailer/component -> its templates; template -> partial/layout.
    RailsView,
    /// Route table file -> controller it routes to.
    RailsRoute,
    /// Static `import` / `export from` / `require()` between JS/TS modules.
    JsImport,
    /// `import()` / `React.lazy` / `require.context` style dynamic import.
    JsDynamicImport,
    /// JS/TS file calls an HTTP endpoint served by the target Ruby controller.
    ApiCall,
    /// Ruby view/controller mounts or renders a JS component/entrypoint
    /// (react_component helper, Inertia page, pack/vite entry tag).
    ViewMount,
    /// GraphQL operation in JS -> Ruby resolver/type file (or schema dump).
    GraphQl,
    /// Non-code file consumed by code (locale yml, json fixture, css module, ...).
    Asset,
}

/// How much to trust an edge. Queries can filter on this so the deterministic
/// core stays separable from guesses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// String/URL/name matching that can be wrong in either direction.
    Heuristic = 0,
    /// Derived from a framework convention that is normally reliable
    /// (Zeitwerk naming, association inflection, implicit render).
    Inferred = 1,
    /// Resolved by a parser/resolver with no guessing.
    Exact = 2,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Edge {
    /// The dependent file.
    pub from: RelPath,
    /// The file being depended on.
    pub to: RelPath,
    pub kind: EdgeKind,
    pub confidence: Confidence,
    /// 1-based line in `from` where the reference occurs; 0 if unknown.
    pub line: u32,
    /// Short human-readable evidence: the constant name, import specifier,
    /// URL, `controller#action`, ... Keep under ~80 chars.
    pub detail: Option<String>,
}

impl Edge {
    pub fn new(from: impl Into<RelPath>, to: impl Into<RelPath>, kind: EdgeKind, confidence: Confidence) -> Self {
        Self { from: from.into(), to: to.into(), kind, confidence, line: 0, detail: None }
    }
    pub fn at(mut self, line: u32) -> Self {
        self.line = line;
        self
    }
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// An HTTP call site found in JS/TS. Produced by `ripple-js`, consumed by
/// `ripple-boundary`, which matches it against the Rails route table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsCallSite {
    pub file: RelPath,
    /// 1-based.
    pub line: u32,
    /// Upper-case HTTP verb when it can be determined (`axios.post`, `method: "PUT"`), else `None`.
    pub method: Option<String>,
    /// URL path with dynamic segments normalised to `:param`
    /// (`` `/api/v1/statuses/${id}/favourite` `` -> `/api/v1/statuses/:param/favourite`).
    /// Query strings and origins stripped. The leading slash is kept exactly as
    /// written: `v1/foo` (relative to a client base URL) stays `v1/foo`.
    pub url: String,
    /// The callee as written (`fetch`, `axios.get`, `api().post`, `Routes.user_path`, ...).
    pub callee: String,
    /// True when the call goes through a generated route helper (js-routes etc.),
    /// in which case `url` holds the helper name instead and matching is exact.
    pub route_helper: bool,
}

/// Cross-language facts extractors hand to linkers. Extend by adding fields;
/// every field must default to empty.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Facts {
    pub js_call_sites: Vec<JsCallSite>,
}

impl Facts {
    pub fn merge(&mut self, other: Facts) {
        self.js_call_sites.extend(other.js_call_sites);
    }
}

/// A non-fatal problem worth telling the user about (unresolvable alias,
/// unparsable file, config that could not be honoured). Never silently drop
/// accuracy: emit a warning with a stable `code`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warning {
    /// Stable snake_case identifier, e.g. `js_unresolved_alias`.
    pub code: String,
    pub message: String,
    pub file: Option<RelPath>,
}

impl Warning {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.to_string(), message: message.into(), file: None }
    }
    pub fn file(mut self, file: impl Into<RelPath>) -> Self {
        self.file = Some(file.into());
        self
    }
}
