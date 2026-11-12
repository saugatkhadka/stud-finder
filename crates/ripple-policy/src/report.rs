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
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Report {
    pub schema_version: u32,
    /// Overall risk: the maximum over fired rules, `low` when none fired.
    pub risk: Risk,
    /// One-line human summary.
    pub summary: String,
    /// Rules that fired, highest risk first, then by rule id.
    pub reasons: Vec<Reason>,
    /// Union of `require` over fired rules, sorted.
    pub require: Vec<String>,
    /// The changed files, sorted by path.
    pub changed: Vec<ChangedEntry>,
    pub impacted: ImpactedSummary,
    /// Test files that depend (transitively) on the change, plus changed test
    /// files (depth 0). Sorted by depth, then path.
    pub tests_to_run: Vec<ImpactedFile>,
    /// Areas containing a changed or impacted non-test file; highest tier first, then name.
    pub areas_reached: Vec<AreaReached>,
    /// Owners of every area in `areas_reached`, sorted and deduplicated.
    pub owners: Vec<String>,
    /// Sorted by code, then file.
    pub warnings: Vec<Warning>,
    /// The query settings actually used.
    pub query: QueryInfo,
    pub timing: Timing,
    pub index: IndexInfo,
}

impl Report {
    /// Number of tests to run, including any cut from `tests_to_run` by a `limit`.
    pub fn tests_total(&self) -> usize {
        let changed_tests = self.changed.iter().filter(|c| c.is_test).count();
        (self.impacted.tests + changed_tests).max(self.tests_to_run.len())
    }
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
    /// Number of files that directly depend on this one (any confidence).
    pub dependents: usize,
    /// False when the file is unknown to the dependency graph (new, deleted,
    /// non-code); its impact is then unknown, not empty.
    pub in_graph: bool,
    pub is_test: bool,
    /// Matches `global_files`: static impact is not reliable for this file.
    pub is_global: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ImpactedSummary {
    /// Impacted non-test files, excluding the changed files.
    pub total: usize,
    /// What `total` would be if every changed file were wholly changed and
    /// method / action scopes on dependencies were ignored (a superset).
    /// Equal to `total` in file-level mode.
    #[serde(default)]
    pub file_level_total: usize,
    /// Impacted test files (see `tests_to_run`).
    pub tests: usize,
    /// Deepest hop count reached.
    pub max_depth: u32,
    /// The following counts cover the `total` non-test files.
    pub by_depth: BTreeMap<u32, usize>,
    pub by_language: BTreeMap<Language, usize>,
    /// Kind of the edge connecting each file to its parent in the explanation path.
    pub by_edge_kind: BTreeMap<EdgeKind, usize>,
    /// Weakest confidence along each file's explanation path.
    pub by_min_confidence: BTreeMap<Confidence, usize>,
    /// Sorted by depth, then path. Truncated only when the request set a `limit`.
    pub files: Vec<ImpactedFile>,
    /// True when `files` (and `tests_to_run`) were cut short by a `limit`.
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImpactedFile {
    pub file: RelPath,
    /// Hops from the nearest changed file (0 only for changed test files in `tests_to_run`).
    pub depth: u32,
    /// The next file on the explanation path (one hop closer to the change).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<RelPath>,
    /// The changed file this one is traced back to.
    pub root: RelPath,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<EdgeKind>,
    /// Confidence of the edge to `via`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<Confidence>,
    /// Weakest confidence along the whole explanation path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_confidence: Option<Confidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Method / action of `file` that holds the reference to `via`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_scope: Option<String>,
    /// Method / action of `via` that is referenced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_scope: Option<String>,
    pub language: Language,
    pub areas: Vec<String>,
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
    /// Explanation path of the closest file in the area.
    pub example: ImpactPath,
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
}
