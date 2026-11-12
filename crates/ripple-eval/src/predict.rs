//! Predictors: one function from (index at the parent, seeds, the commit's
//! diff) to a predicted set. Adding a variant = implementing [`Predictor`]
//! and adding it to [`standard`].

use crate::adapter::{Confidence, FileChange, Index, NodeId, Query};

/// A changed file used as a query seed. `scopes` are the methods the commit
/// touched (from its real diff hunks), `None` = whole file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seed {
    pub node: NodeId,
    pub scopes: Option<Vec<String>>,
}

/// What a predictor sees besides the seeds.
pub struct Ctx<'a> {
    /// The repository at the commit's parent.
    pub index: &'a Index,
    /// Every file the commit changed (paths, status, hunks at the parent).
    /// Unused by today's predictors; there for diff-aware ones (e.g. "direct
    /// dependents that mention a changed method").
    #[allow(dead_code)]
    pub diff: &'a [FileChange],
}

/// Predicted files as `(node, depth)`, seeds excluded, each node at most once
/// per list. `dependents` answer "what may break" (reverse reachability);
/// `dependencies` the mirror question "what does the change rely on".
/// Predictors without a notion of direction or depth put everything in
/// `dependents` at depth 1.
#[derive(Debug, Default, Clone)]
pub struct Prediction {
    pub dependents: Vec<(NodeId, u32)>,
    pub dependencies: Vec<(NodeId, u32)>,
}

pub trait Predictor {
    fn name(&self) -> &str;
    /// `max_depth` 0 = unlimited.
    fn predict(&self, ctx: &Ctx, seeds: &[Seed], max_depth: u32) -> Prediction;
    /// False for baselines: they are not shown in the tests table.
    fn is_graph(&self) -> bool {
        true
    }
}

/// ripple's graph, both directions.
pub struct GraphPredictor {
    pub name: &'static str,
    pub min_confidence: Confidence,
    /// true: whole-file seeds, edge scopes ignored. false: seeds narrowed to
    /// the methods the diff touched, edge scopes followed.
    pub file_level: bool,
}

impl Predictor for GraphPredictor {
    fn name(&self) -> &str {
        self.name
    }
    fn predict(&self, ctx: &Ctx, seeds: &[Seed], max_depth: u32) -> Prediction {
        let q = Query { max_depth, min_confidence: self.min_confidence, file_level: self.file_level };
        let scoped: Vec<(NodeId, Option<Vec<String>>)> = seeds.iter().map(|s| (s.node, if self.file_level { None } else { s.scopes.clone() })).collect();
        let nodes: Vec<NodeId> = seeds.iter().map(|s| s.node).collect();
        Prediction { dependents: ctx.index.impact(&scoped, q), dependencies: ctx.index.dependencies(&nodes, q) }
    }
}

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

/// Baseline: every file in the same directory as a seed.
pub struct SameDir;

impl Predictor for SameDir {
    fn name(&self) -> &str {
        "same_dir"
    }
    fn is_graph(&self) -> bool {
        false
    }
    fn predict(&self, ctx: &Ctx, seeds: &[Seed], _max_depth: u32) -> Prediction {
        let dirs: std::collections::BTreeSet<&str> = seeds.iter().map(|s| dir_of(ctx.index.path(s.node))).collect();
        let seed_nodes: std::collections::HashSet<NodeId> = seeds.iter().map(|s| s.node).collect();
        let dependents = (0..ctx.index.node_count() as NodeId).filter(|n| !seed_nodes.contains(n) && dirs.contains(dir_of(ctx.index.path(*n)))).map(|n| (n, 1)).collect();
        Prediction { dependents, dependencies: Vec::new() }
    }
}

/// The variants run side by side. The first one is the reference for the
/// miss classification and the random baseline.
pub fn standard() -> Vec<Box<dyn Predictor>> {
    vec![
        Box::new(GraphPredictor { name: "file", min_confidence: Confidence::Heuristic, file_level: true }),
        Box::new(GraphPredictor { name: "method", min_confidence: Confidence::Heuristic, file_level: false }),
        Box::new(GraphPredictor { name: "file_inferred", min_confidence: Confidence::Inferred, file_level: true }),
        Box::new(GraphPredictor { name: "file_exact", min_confidence: Confidence::Exact, file_level: true }),
        Box::new(SameDir),
    ]
}
