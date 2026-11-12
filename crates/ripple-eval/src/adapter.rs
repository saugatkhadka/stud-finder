//! The only module that touches ripple's API. Everything else in this crate
//! works on node ids, paths and plain `(node, depth)` lists, so changes to
//! ripple's public surface stay contained here.
//!
//! Used from ripple:
//! * `ripple_cli::engine::{Engine, Pipeline, Snapshot}`: cold in-process index (no daemon);
//! * `ripple_cli::git::{parse_raw_numstat, parse_hunks}`: the CLI's own diff parsing;
//! * `ripple_policy::{scopes_for_hunks, Policy, Language, ...}`: hunk → method mapping, test globs;
//! * `ripple_core::{Graph, QueryOpts, Confidence}`: the impact queries.

use anyhow::{Context, Result, bail};
use ripple_cli::engine::{Engine, Pipeline, Snapshot};
pub use ripple_core::Confidence;
use ripple_core::model::ScopeRange;
use ripple_core::{Graph, QueryOpts};
use ripple_policy::{FileStatus, Hunk, Language, Policy, scopes_for_hunks};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Instant;

pub type NodeId = ripple_core::NodeId;

/// Query settings a graph predictor varies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Query {
    /// 0 = unlimited.
    pub max_depth: u32,
    pub min_confidence: Confidence,
    pub file_level: bool,
}

enum Source {
    Snapshot(Arc<Snapshot>),
    #[cfg_attr(not(test), allow(dead_code))]
    Synthetic(Graph),
}

/// One index generation (the repository at one commit) plus the policy's
/// test classification of every node.
pub struct Index {
    source: Source,
    tests: Vec<bool>,
    scopes: Arc<BTreeMap<String, Vec<ScopeRange>>>,
    pub build_ms: u64,
}

impl Index {
    /// Cold in-process build of the working tree at `root` (no daemon).
    pub fn build(root: &Path) -> Result<Self> {
        let t = Instant::now();
        let (_engine, snap) = Engine::open(root, Pipeline::standard()).with_context(|| format!("indexing {}", root.display()))?;
        let tests = (0..snap.graph.node_count() as NodeId).map(|n| snap.index.get(n).is_some_and(|c| c.test)).collect();
        Ok(Self { scopes: snap.scopes.clone(), tests, source: Source::Snapshot(snap), build_ms: t.elapsed().as_millis() as u64 })
    }

    /// A hand-made graph with default config (for tests).
    #[cfg(test)]
    pub fn synthetic(edges: &[ripple_core::Edge], extra_nodes: &[&str], scopes: BTreeMap<String, Vec<ScopeRange>>) -> Self {
        let graph = Graph::build(edges, extra_nodes.iter().copied());
        let policy = Policy::compile(ripple_core::Config::default()).expect("defaults compile");
        let tests = graph.paths().iter().map(|p| policy.is_test(p)).collect();
        Self { source: Source::Synthetic(graph), tests, scopes: Arc::new(scopes), build_ms: 0 }
    }

    fn graph(&self) -> &Graph {
        match &self.source {
            Source::Snapshot(s) => &s.graph,
            Source::Synthetic(g) => g,
        }
    }
    /// The underlying graph, for the `breaks` oracle's miss classification.
    pub fn core_graph(&self) -> &Graph {
        self.graph()
    }
    pub fn node_count(&self) -> usize {
        self.graph().node_count()
    }
    pub fn edge_count(&self) -> usize {
        self.graph().edge_count()
    }
    pub fn node(&self, path: &str) -> Option<NodeId> {
        self.graph().id(path)
    }
    pub fn path(&self, n: NodeId) -> &str {
        self.graph().path(n)
    }
    pub fn is_test(&self, n: NodeId) -> bool {
        self.tests[n as usize]
    }

    /// The method-level seed for a modified file, exactly as `ripple-policy`
    /// maps a diff: `None` = whole file.
    pub fn seed_scopes(&self, path: &str, status: Status, hunks: Option<&[Hunk]>) -> Option<Vec<String>> {
        let status = match status {
            Status::Modified => FileStatus::Modified,
            _ => return None,
        };
        scopes_for_hunks(status, hunks, self.scopes.get(path).map(Vec::as_slice).filter(|v| !v.is_empty()))
    }

    /// Reverse reachability (dependents), seeds excluded, as `(node, depth)`.
    pub fn impact(&self, seeds: &[(NodeId, Option<Vec<String>>)], q: Query) -> Vec<(NodeId, u32)> {
        let opts = QueryOpts { max_depth: q.max_depth, min_confidence: q.min_confidence, file_level: q.file_level };
        self.graph().impact_scoped(seeds, opts).impacted().map(|h| (h.node, h.depth)).collect()
    }

    /// Forward reachability (dependencies), seeds excluded. Not scope-aware.
    pub fn dependencies(&self, seeds: &[NodeId], q: Query) -> Vec<(NodeId, u32)> {
        let opts = QueryOpts { max_depth: q.max_depth, min_confidence: q.min_confidence, file_level: true };
        self.graph().dependencies_closure(seeds, opts).impacted().map(|h| (h.node, h.depth)).collect()
    }
}

/// The `ripple.yml` in `root` (or defaults), for classifying paths before any index exists.
pub struct Classifier(Policy);

impl Classifier {
    pub fn load(root: &Path) -> Result<Self> {
        Policy::load(root).map(Self).map_err(|e| anyhow::anyhow!("{e}"))
    }
    pub fn is_ignored(&self, path: &str) -> bool {
        self.0.is_ignored(path)
    }
}

/// Coarse language of a path ("ruby", "js", "template", "other").
pub fn language(path: &str) -> &'static str {
    Language::of(path).as_str()
}

/// Ruby, JS/TS or a template: what the extractors analyse.
pub fn is_source(path: &str) -> bool {
    Language::of(path).is_source()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Added,
    Modified,
    Deleted,
    Renamed,
}

/// One file changed by a commit.
#[derive(Debug, Clone)]
pub struct FileChange {
    /// Path at the commit (for deletions: the deleted path).
    pub path: String,
    /// Path at the parent, when the file existed there.
    pub parent_path: Option<String>,
    pub status: Status,
    /// Hunks of the reversed diff (commit → parent), i.e. in the parent's
    /// line numbers, which is what the index at the parent knows. Modified files only.
    pub hunks: Option<Vec<Hunk>>,
}

pub fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.quotepath=off"])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// Files changed from `parent` to `commit`, with renames detected as the CLI does.
pub fn commit_changes(repo: &Path, parent: &str, commit: &str) -> Result<Vec<FileChange>> {
    let raw = git(repo, &["diff", "-z", "--raw", "--numstat", "-M", "--no-ext-diff", parent, commit, "--"])?;
    let files = ripple_cli::git::parse_raw_numstat(&raw);
    // Reversed: the "new" side is the parent, whose line numbers the index has.
    let patch = git(repo, &["diff", "-U0", "--no-color", "-M", "--no-ext-diff", "--src-prefix=a/", "--dst-prefix=b/", commit, parent, "--"])?;
    let hunks: BTreeMap<String, Vec<Hunk>> = ripple_cli::git::parse_hunks(&patch).into_iter().collect();
    Ok(files
        .into_values()
        .map(|f| {
            let status = match f.status {
                FileStatus::Added => Status::Added,
                FileStatus::Modified => Status::Modified,
                FileStatus::Deleted => Status::Deleted,
                FileStatus::Renamed => Status::Renamed,
            };
            let parent_path = match status {
                Status::Added => None,
                Status::Renamed => f.old_path.clone(),
                _ => Some(f.path.clone()),
            };
            let hunks = if status == Status::Modified { hunks.get(&f.path).cloned() } else { None };
            FileChange { path: f.path, parent_path, status, hunks }
        })
        .collect())
}

/// For tests: build hunks without git.
#[cfg(test)]
pub fn hunk(start: u32, count: u32) -> Hunk {
    Hunk::new(start, count)
}
