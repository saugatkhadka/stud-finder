//! The impact report: the stable JSON API consumed by coding agents and CI.
//!
//! Conventions: every list is deterministically ordered (documented per
//! field); optional values are omitted rather than `null`; counts never
//! include the changed files themselves unless stated.

use crate::changed::FileStatus;
use crate::lang::Language;
use ripple_core::{Confidence, EdgeKind, RelPath, Risk, Warning};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Bumped on any incompatible change to the report shape.
pub const SCHEMA_VERSION: u32 = 2;

/// Default list sizes (`all: false`). Chosen from measured distributions:
/// direct dependents per change are p50 2, p90 15, max 250 on Gumroad, so 30
/// lists every direct dependent of more than 90% of changes in full.
pub mod budget {
    /// `changed` entries.
    pub const CHANGED: usize = 30;
    /// `dependents` entries.
    pub const DEPENDENTS: usize = 30;
    /// `relies_on` entries.
    pub const RELIES_ON: usize = 10;
    /// `further.files` entries (notable files only).
    pub const FURTHER: usize = 10;
    /// `tests` entries (changed tests and tests at hop 1).
    pub const TESTS: usize = 10;
    /// `removed_methods` entries, and `mentions` per entry.
    pub const REMOVED: usize = 10;
    pub const MENTIONS: usize = 15;
    /// `also_mentioned` files per name.
    pub const ALSO_MENTIONED: usize = 15;
    /// A name with more definitions than this is common: `also_mentioned`
    /// gives its count only (`all` lists the files).
    pub const COMMON_NAME_DEFINITIONS: usize = 3;
    /// `inherit_only` entries.
    pub const INHERIT_ONLY: usize = 10;
    /// `incomplete` and `warnings` entries per reason / code; the rest are
    /// summarised in one entry each.
    pub const PER_KIND: usize = 5;
    /// Explanation paths per policy reason in the compact policy section.
    pub const EVIDENCE: usize = 1;
}

/// The impact report: the agent / CI API. Sections are described in docs/METRICS.md.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Report {
    pub schema_version: u32,
    /// One line: counts, risk (when shown) and whether the answer may be incomplete.
    pub summary: String,
    /// Exact totals, whatever the lists were cut to.
    pub counts: Counts,
    /// True when some list below was cut to its budget (`all` lifts the budget).
    pub truncated: bool,
    /// Changed files, sorted by path.
    pub changed: Vec<ChangedEntry>,
    /// Methods the change removed or renamed that files still mention:
    /// the references most likely to break. Sorted by file, then method.
    pub removed_methods: Vec<RemovedMethod>,
    /// Non-test files that directly depend on a changed file (hop 1), each
    /// with its evidence. Most useful first: files that mention a changed
    /// method (calls on the class's constant first), then crossing the
    /// Ruby/JS boundary, then in a high/critical area, then by confidence,
    /// then path. For a method-scoped change, heirs and includers that do
    /// not mention a changed method are in `inherit_only` instead.
    pub dependents: Vec<Dependent>,
    /// Direct dependents that inherit from / include a changed file but do
    /// not mention any of its changed methods (method-scoped changes only).
    /// Sorted by path.
    pub inherit_only: Vec<RelPath>,
    /// Per changed method name: files that mention the name but have no
    /// dependency edge to the changed file (receiver type unknown: views,
    /// mailers, association chains, or the same name on another class).
    /// Nearest path first.
    pub also_mentioned: Vec<AlsoMentioned>,
    /// What the changed files themselves depend on (hop 1, forward): context
    /// to read, not impact. Sorted by confidence, then file.
    pub relies_on: Vec<Dependency>,
    /// Non-test files 2 or more hops away, summarised.
    pub further: Further,
    /// Test files that depend on the change through the graph (not coverage,
    /// and not a selection of tests to run): changed tests (depth 0) and hop-1
    /// tests by default, all with `all`.
    pub tests: Vec<TestFile>,
    /// Why the lists above may miss files. Empty = nothing known to be missing.
    pub incomplete: Vec<Incomplete>,
    /// What a method-scoped answer cannot see, with the search to run
    /// instead. Empty for whole-file changes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blind_spots: Vec<String>,
    /// Files outside the diff, its direct dependents and its direct
    /// dependencies that usually change together with a changed file (git
    /// co-change), best first, at most [`crate::USUALLY_CHANGES_WITH`].
    /// Absent when there are none or the signal is not ready (see `signals`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub usually_changes_with: Vec<crate::signals::CoChangeEntry>,
    /// State of each non-graph signal (complexity, churn, co-change,
    /// coverage, hotspot). Absent when the caller provided none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signals: Option<crate::signals::Signals>,
    /// Risk, fired rules, required actions, owners and areas from
    /// `ripple.yml`. Present when the request asks for it (`check`), or when
    /// a `ripple.yml` defines rules or areas and a rule fired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<PolicyResult>,
    /// Sorted by code, then file.
    pub warnings: Vec<Warning>,
    pub query: QueryInfo,
    pub index: IndexInfo,
    pub timing: Timing,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Counts {
    pub changed: usize,
    /// Distinct non-test files at hop 1 (the fan-in of the change).
    pub dependents: usize,
    pub relies_on: usize,
    /// Non-test files at hops 2..=max_depth.
    pub further: usize,
    /// Those of them that are notable (see [`FurtherFile::notable`]).
    pub further_notable: usize,
    /// `dependents + further`: every impacted non-test file. Policy rules see the same set.
    pub impacted: usize,
    /// What `impacted` would be if every changed file were wholly changed
    /// and method / action scopes were ignored (a superset).
    pub impacted_file_level: usize,
    /// Tests that depend on the change at any depth, plus changed tests.
    pub tests: usize,
    /// Changed tests plus tests at hop 1.
    pub tests_direct: usize,
    /// Number of `removed_methods` entries before the budget.
    pub removed_methods: usize,
    /// Direct dependents that only inherit / include (part of `dependents`).
    pub inherit_only: usize,
    /// Files in `also_mentioned` before the budget, over all names.
    pub also_mentioned: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemovedMethod {
    /// The changed file that no longer defines `method`.
    pub file: RelPath,
    pub method: String,
    /// Files that still mention the name, most certain first: the file
    /// itself, its direct dependents, then other files by name only.
    pub mentions: Vec<Mention>,
    /// Mentions before the budget (by-name matches are counted from the
    /// index; listed ones are confirmed in the file text when it can be read).
    pub mentions_total: usize,
    /// Other files that define a method of this name. From
    /// [`budget::COMMON_NAME_DEFINITIONS`] on, by-name matches are counted in
    /// `mentions_total` but not listed (unless `all`).
    pub other_definitions: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AlsoMentioned {
    /// A changed (or `--scope`d) method name.
    pub name: String,
    /// The changed file defining it.
    pub of: RelPath,
    /// Files in the workspace that define a method of this name, the
    /// changed file included. 1 means
    /// the mentions are almost certainly calls of this method; many means
    /// the name is common.
    pub definitions: usize,
    /// Mentioning files without an edge, before the budget.
    pub total: usize,
    /// The changed method is a singleton (`def self.x`): a call that does
    /// not name the class constant is unlikely to be this method.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub singleton: bool,
    /// Nearest first (same directory tree, then path); `how` is `by_name`.
    /// Empty by default for a common name or a singleton method.
    pub files: Vec<Mention>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Mention {
    pub file: RelPath,
    /// First line mentioning the name, when the file could be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub how: MentionHow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MentionHow {
    /// The changed file itself.
    SameFile,
    /// A direct dependent of the changed file.
    Dependent,
    /// No dependency edge: matched by method name only (an untyped receiver,
    /// or the same name on another class).
    ByName,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Dependent {
    pub file: RelPath,
    /// Line in `file` where the reference is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub kind: EdgeKind,
    /// Constant, import specifier, `VERB /url → controller#action`, ...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub confidence: Confidence,
    /// The changed file it depends on.
    pub of: RelPath,
    /// Method / action of `file` holding the reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_scope: Option<String>,
    /// Method / action of `of` that is referenced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_scope: Option<String>,
    /// `file` inherits from / includes `of`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub inherit: bool,
    /// Changed, signature-changed or removed methods of `of` that `file`
    /// mentions by name (when method facts are known).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uses_methods: Vec<String>,
    /// First line in `file` mentioning one of `uses_methods` (the call
    /// itself; `line` is the constant / import reference), or for a route
    /// the line routing a changed action. When the file could be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mention_line: Option<u32>,
    /// For a changed singleton method (`def self.x`): the lines where `file`
    /// calls it on the class constant (`Const.x`, not just some `x`), at most 10.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls_on_constant: Vec<u32>,
    pub language: Language,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub areas: Vec<String>,
    /// On the other side of the Ruby/JS boundary from `of`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub crosses_boundary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Dependency {
    /// The file relied on.
    pub file: RelPath,
    /// The changed file that relies on it.
    pub of: RelPath,
    /// Line in `of` where the reference is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub kind: EdgeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub confidence: Confidence,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Further {
    /// Non-test files per hop count (2..).
    pub by_depth: BTreeMap<u32, usize>,
    pub by_language: BTreeMap<Language, usize>,
    /// Files per area (a file may be in several).
    pub by_area: BTreeMap<String, usize>,
    /// Files per highest area tier (files in no area are not counted).
    pub by_tier: BTreeMap<Risk, usize>,
    /// Notable files by default (nearest first, at most the budget); every
    /// file in BFS order with `all`.
    pub files: Vec<FurtherFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FurtherFile {
    pub file: RelPath,
    pub depth: u32,
    /// The next file towards the change.
    pub via: RelPath,
    /// The changed file it traces back to.
    pub of: RelPath,
    pub kind: EdgeKind,
    /// Weakest confidence along the path.
    pub confidence: Confidence,
    pub language: Language,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub areas: Vec<String>,
    /// Why it is worth a look: `crosses_boundary` (other side of Ruby/JS
    /// from `of`), `high_tier` (area of tier high or critical), `global`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notable: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TestFile {
    pub file: RelPath,
    /// 0 = a changed test file.
    pub depth: u32,
    /// The changed file it traces back to (absent for changed tests).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub of: Option<RelPath>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<RelPath>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<EdgeKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// First line mentioning a changed method (hop-1 tests of a
    /// method-scoped change), when the file could be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mention_line: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Incomplete {
    /// `global_file`, `not_in_graph`, `dynamic_dispatch`,
    /// `unresolved_references`, `stale_index`.
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<RelPath>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PolicyResult {
    /// The maximum over fired rules, `low` when none fired.
    pub risk: Risk,
    /// Rules that fired, highest risk first, then by rule id.
    pub reasons: Vec<Reason>,
    /// Union of `require` over fired rules, sorted.
    pub require: Vec<String>,
    /// Owners of every area in `areas`, sorted.
    pub owners: Vec<String>,
    /// Areas containing a changed or impacted non-test file; highest tier first, then name.
    pub areas: Vec<AreaReached>,
}

/// A rule that fired.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Reason {
    pub rule: String,
    pub risk: Risk,
    /// Human sentence explaining what matched.
    pub message: String,
    /// The rule's own description from `ripple.yml`, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub require: Vec<String>,
    /// True for the built-in rules (no `rules:` in `ripple.yml`).
    pub builtin: bool,
    pub evidence: Evidence,
}

/// The files that made a rule fire.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Evidence {
    /// Changed files that satisfied a `touches*` / `touches_global` /
    /// `untested` condition, or whose impact satisfied a `reaches*` /
    /// `crosses_boundary` condition. Sorted.
    pub changed: Vec<RelPath>,
    /// Impacted files that satisfied a `reaches*` / `crosses_boundary`
    /// condition, closest first, at most [`EVIDENCE_LIMIT`]; each with its explanation path.
    pub impacted: Vec<ImpactPath>,
    /// Total number of impacted files that satisfied the condition (may exceed `impacted.len()`).
    pub impacted_matching: usize,
    /// Numbers that satisfied threshold conditions (`impacted_files`, `changed_files`, `changed_lines`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub counts: BTreeMap<String, usize>,
}

/// Maximum impacted files listed as evidence per reason.
pub const EVIDENCE_LIMIT: usize = 20;

/// An impacted file and the shortest chain of dependencies leading back to a changed file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImpactPath {
    pub file: RelPath,
    pub depth: u32,
    /// `path[0]` is `file`, the last hop is the changed file. Each hop's edge
    /// fields describe how that file depends on the next one.
    pub path: Vec<Hop>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hop {
    pub file: RelPath,
    /// How `file` depends on the next hop; absent on the last hop (the changed file).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<EdgeKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<Confidence>,
    /// 1-based line in `file` where the reference is; absent when unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// Evidence text: constant, import specifier, URL, `controller#action`, ...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The method / action of `file` the reference sits in (absent: the file as a whole).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_scope: Option<String>,
    /// The method / action of the next hop that is referenced (absent: the file as a whole).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_scope: Option<String>,
    /// `file` inherits from / includes the next hop (superclass, mixin).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub inherit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChangedEntry {
    pub path: RelPath,
    pub status: FileStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<RelPath>,
    /// Added/deleted lines; absent when no diff is available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_line: Option<u32>,
    /// The methods / actions the diff is confined to, which is what the impact
    /// query started from. Absent = the whole file is treated as changed
    /// (no diff, a change outside every method, `--file-level`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
    pub language: Language,
    pub areas: Vec<String>,
    /// What kind of change it is (body / signature / added / removed method,
    /// class body, whole file).
    pub change: crate::changed::Change,
    /// Number of files that directly depend on this one (any confidence): its fan-in.
    pub dependents: usize,
    /// False when the file is unknown to the dependency graph (new, deleted,
    /// non-code); its impact is then unknown, not empty.
    pub in_graph: bool,
    pub is_test: bool,
    /// Matches `global_files`: static impact is not reliable for this file.
    pub is_global: bool,
    /// Complexity, size, churn, coverage and hotspot rank (non-test files
    /// that still exist; parts are absent when unknown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<crate::signals::Metrics>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AreaReached {
    pub area: String,
    pub tier: Risk,
    pub owners: Vec<String>,
    /// A changed file is in this area.
    pub touched: bool,
    /// Changed + impacted non-test files in this area.
    pub files: usize,
    /// 0 when touched directly.
    pub min_depth: u32,
    /// Explanation path of the closest file in the area (full reports only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example: Option<ImpactPath>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct QueryInfo {
    /// 0 = unlimited.
    pub max_depth: u32,
    pub min_confidence: Confidence,
    /// True when the query ignored method / action scopes (`--file-level`):
    /// every changed file counted as wholly changed.
    #[serde(default)]
    pub file_level: bool,
    /// Lists were not cut to the default budget.
    #[serde(default)]
    pub all: bool,
}

/// Microseconds spent per phase on the side that produced the report.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Timing {
    /// Waiting for / applying pending file changes before answering (daemon only).
    pub refresh_us: u64,
    /// Reverse BFS over the graph.
    pub query_us: u64,
    /// Rule evaluation and report assembly.
    pub evaluate_us: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct IndexInfo {
    /// Index generation the report was computed against; increases on every re-index.
    pub generation: u64,
    pub nodes: usize,
    pub edges: usize,
    /// File changes the daemon knew about but had not yet indexed when it
    /// answered (only when it served a snapshot while a rebuild was in
    /// progress and none of the requested files were affected). 0 = fully fresh.
    pub pending_changes: usize,
    /// Reserved for the daemon: it answered from an index it knows to be
    /// behind the work tree.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
}
