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
    /// Co-change histories built strictly from commits older than the one
    /// being measured (at its parent), one per configuration (see `main`).
    pub history: &'a [ripple_history::History],
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

/// Co-change partners of the seeds (ripple-history), at most `limit` non-test
/// graph nodes, seeds excluded. `graph_depth1`: the graph's direct
/// neighbours in both directions (file-level) plus up to `limit` partners
/// that are not among them: what an agent would be shown as hop 1 plus
/// "usually changes with this, not explained by the graph".
pub struct CoChange {
    pub name: String,
    /// Index into `Ctx::history`.
    pub history: usize,
    pub query: ripple_history::PartnerQuery,
    pub graph_depth1: bool,
}

/// The co-change list for `seeds` as node ids; tests, non-nodes and
/// `known` skipped before the limit.
pub fn cochange(ctx: &Ctx, history: usize, query: &ripple_history::PartnerQuery, seeds: &[Seed], known: &std::collections::HashSet<NodeId>) -> Vec<NodeId> {
    let Some(h) = ctx.history.get(history) else { return Vec::new() };
    let paths: Vec<&str> = seeds.iter().map(|s| ctx.index.path(s.node)).collect();
    let skip = |p: &str| ctx.index.node(p).is_none_or(|n| ctx.index.is_test(n) || known.contains(&n));
    h.partners_with(&paths, query, skip).iter().filter_map(|p| ctx.index.node(&p.path)).collect()
}

impl Predictor for CoChange {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_graph(&self) -> bool {
        false
    }
    fn predict(&self, ctx: &Ctx, seeds: &[Seed], _max_depth: u32) -> Prediction {
        let mut p = if self.graph_depth1 {
            GraphPredictor { name: "", min_confidence: Confidence::Heuristic, file_level: true }.predict(ctx, seeds, 1)
        } else {
            Prediction::default()
        };
        let have: std::collections::HashSet<NodeId> = p.dependents.iter().chain(&p.dependencies).map(|&(n, _)| n).collect();
        p.dependents.extend(cochange(ctx, self.history, &self.query, seeds, &have).into_iter().map(|n| (n, 1)));
        p
    }
}

/// The query `ripple.yml` defaults ask for (`history:`), without its limit.
pub fn default_query() -> ripple_history::PartnerQuery {
    ripple_history::PartnerQuery::from_config(&ripple_core::config::HistoryConfig::default())
}

/// The co-change rows of the standard tables: the default query at K = 5, 10, 20, alone and with graph depth 1.
pub fn cochange_variants() -> Vec<Box<dyn Predictor>> {
    let mut v: Vec<Box<dyn Predictor>> = Vec::new();
    for graph_depth1 in [false, true] {
        for k in crate::metrics::KS {
            let query = ripple_history::PartnerQuery { limit: k, ..default_query() };
            let name = if graph_depth1 { format!("graph1+cochange@{k}") } else { format!("cochange@{k}") };
            v.push(Box::new(CoChange { name, history: 0, query, graph_depth1 }));
        }
    }
    v
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
