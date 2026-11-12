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
    /// The part of `from` the reference sits in, when only that part depends
    /// on `to`: today a controller action name (`"index"`). `None` = the whole
    /// file depends on `to` (class body, callbacks, private helpers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_scope: Option<String>,
    /// The part of `to` that `from` depends on: a controller action name for
    /// `ApiCall` / `ViewMount` edges. `None` = depends on the whole file.
    ///
    /// Impact queries use the two scopes together: a dependent whose edge
    /// names `to_scope: "show"` is only reached through a controller that was
    /// itself reached via an unscoped reference or one with `from_scope: "show"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_scope: Option<String>,
    /// `from` inherits `to`'s behaviour (superclass, `include`, `extend`,
    /// `prepend`): whatever part of `to` is affected, the same-named part of
    /// `from` is affected. Scopes on the edge are ignored.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub inherit: bool,
}

/// `to_scope` value meaning "depends only on the class-level part of `to`"
/// (constants, DSL, anything outside a method): followed only when the whole
/// of `to` is affected, never for a change confined to named methods.
pub const SCOPE_BODY: &str = "@body";

/// A named part of a file (a Ruby method / controller action) and its lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeRange {
    pub name: String,
    /// 1-based line of the `def`.
    pub start: u32,
    /// 1-based line of the closing `end`, inclusive.
    pub end: u32,
}

impl Edge {
    pub fn new(from: impl Into<RelPath>, to: impl Into<RelPath>, kind: EdgeKind, confidence: Confidence) -> Self {
        Self { from: from.into(), to: to.into(), kind, confidence, line: 0, detail: None, from_scope: None, to_scope: None, inherit: false }
    }
    pub fn at(mut self, line: u32) -> Self {
        self.line = line;
        self
    }
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
    pub fn from_scope(mut self, scope: impl Into<String>) -> Self {
        self.from_scope = Some(scope.into());
        self
    }
    pub fn to_scope(mut self, scope: impl Into<String>) -> Self {
        self.to_scope = Some(scope.into());
        self
    }
    pub fn inherit(mut self) -> Self {
        self.inherit = true;
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
    /// True when the URL is only navigated to (`href=`, `<Link>`, `<a>`,
    /// `router.visit`, `window.location = ...`) and no response is consumed by
    /// this file. Such a file does not break when the controller changes, so
    /// linkers skip these unless `boundary.link_edges` is on.
    #[serde(default)]
    pub link: bool,
}

/// Cross-language facts extractors hand to linkers. Extend by adding fields;
/// every field must default to empty.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Facts {
    pub js_call_sites: Vec<JsCallSite>,
    /// Per file, the scopes that edges may name in `from_scope`/`to_scope`,
    /// with their line ranges, sorted by `start`. Lets a diff's changed lines
    /// be turned into "these methods changed". A file absent here, or a line
    /// outside every range, means "treat the whole file as changed".
    #[serde(default)]
    pub scopes: std::collections::BTreeMap<RelPath, Vec<ScopeRange>>,
}

impl Facts {
    pub fn merge(&mut self, other: Facts) {
        self.js_call_sites.extend(other.js_call_sites);
        self.scopes.extend(other.scopes);
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
