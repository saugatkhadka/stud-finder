//! The contract between language extractors, cross-language linkers and the engine.

use crate::config::Config;
use crate::model::{Edge, Facts, RelPath, Warning};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// What an extractor is given. `files` is the full, sorted list of files the
/// engine considers part of the repo (tracked + untracked-not-ignored, minus
/// `config.ignore`). Extractors pick the ones they understand by extension.
#[derive(Debug, Clone)]
pub struct Workspace {
    /// Absolute path of the repo root.
    pub root: PathBuf,
    pub files: Vec<RelPath>,
    pub config: Config,
}

impl Workspace {
    pub fn abs(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

/// Files that changed on disk since the extractor last ran.
/// A newly added file appears in `modified`.
#[derive(Debug, Clone, Default)]
pub struct ChangeSet {
    pub modified: Vec<RelPath>,
    pub removed: Vec<RelPath>,
}

impl ChangeSet {
    pub fn is_empty(&self) -> bool {
        self.modified.is_empty() && self.removed.is_empty()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExtractOutput {
    /// The extractor's **complete** current edge set (not a delta). The engine
    /// replaces this extractor's previous edges wholesale. Must be
    /// deterministic: same inputs -> same set (order does not matter).
    pub edges: Vec<Edge>,
    pub facts: Facts,
    pub warnings: Vec<Warning>,
    /// Free-form counters for `ripple stats` (files_parsed, refs_resolved, ...).
    pub stats: BTreeMap<String, u64>,
}

/// A per-language analyser. One long-lived instance per workspace; it owns
/// whatever caches it needs so that `update` is cheap.
pub trait Extractor: Send {
    fn name(&self) -> &'static str;

    /// Cold start: analyse everything.
    fn build(&mut self, ws: &Workspace) -> anyhow::Result<ExtractOutput>;

    /// Warm path: only `changes` differ from the last `build`/`update`.
    /// MUST return exactly what a fresh `build` on the current tree would
    /// (this is tested by the engine), just faster. Default: rebuild.
    fn update(&mut self, ws: &Workspace, changes: &ChangeSet) -> anyhow::Result<ExtractOutput> {
        let _ = changes;
        self.build(ws)
    }
}

/// Produces edges that need facts from more than one language
/// (e.g. JS call site -> Rails route -> controller file). Runs after all
/// extractors, on every build and update.
pub trait Linker: Send {
    fn name(&self) -> &'static str;
    fn link(&mut self, ws: &Workspace, facts: &Facts, changes: Option<&ChangeSet>) -> anyhow::Result<ExtractOutput>;
}
