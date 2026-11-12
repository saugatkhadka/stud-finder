//! Per-commit measurements of one predictor against what the commit really
//! changed. Pure functions of (index, truth, predictor): no git, no I/O.

use crate::adapter::{Index, NodeId, language};
use crate::predict::{Ctx, Predictor, Seed};
use serde::Serialize;
use std::collections::HashMap;

/// Depths measured: 1..=D.
pub const D: usize = 3;

/// What the commit changed, restricted to files the index at the parent knows.
pub struct Truth {
    /// Non-test changed files that are graph nodes, with the methods the diff touched.
    pub targets: Vec<Seed>,
    /// Changed test files that are graph nodes.
    pub tests: Vec<NodeId>,
}

/// Leave-one-out: was `file` predicted from the other changed files?
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LooCase {
    pub file: String,
    pub bucket: &'static str,
    /// Depth at which `file` is a dependent of the seeds (`None` = not within D).
    pub rev: Option<u32>,
    /// Depth at which `file` is a dependency of the seeds.
    pub fwd: Option<u32>,
    /// Non-test dependents of the seeds within depth 1..=D (seeds excluded).
    pub size_rev: [u32; D],
    /// Non-test files within depth 1..=D in either direction.
    pub size_union: [u32; D],
    /// Non-test graph nodes that are not seeds: what a random guess draws from.
    pub pool: u32,
}

impl LooCase {
    pub fn union(&self) -> Option<u32> {
        match (self.rev, self.fwd) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// One changed file alone as the seed: how big is its impact and how much of
/// the rest of the commit does it contain?
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SingleCase {
    pub file: String,
    /// Non-test dependents within depth 1..=D.
    pub size: [u32; D],
    /// How many of the other non-test changed files are among them.
    pub hits: [u32; D],
    pub others: u32,
}

/// Changed test files vs. the tests that reach the changed sources.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct TestsCase {
    pub changed: u32,
    pub reached_d3: u32,
    pub reached_all: u32,
    /// Test files among the dependents within depth 3 / at any depth.
    pub size_d3: u32,
    pub size_all: u32,
    /// Changed test files not among the tests reaching the change within depth 3.
    pub missed_d3: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VariantResult {
    pub variant: String,
    pub loo: Vec<LooCase>,
    pub single: Vec<SingleCase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tests: Option<TestsCase>,
}

/// Co-change list sizes reported.
pub const KS: [usize; 3] = [5, 10, 20];
const KMAX: usize = 20;

/// One co-change configuration measured in the gain analysis.
pub struct GainConfig {
    pub name: String,
    /// Index into `Ctx::history`.
    pub history: usize,
    /// Its `limit` is ignored (lists are taken up to 20).
    pub query: ripple_history::PartnerQuery,
}

/// A leave-one-out target seen by the graph and by co-change.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GainCase {
    pub file: String,
    pub bucket: &'static str,
    /// Depth at which the file-level graph finds it, either direction (`None` = not within D).
    pub graph: Option<u32>,
    /// 0-based position in the co-change list of the other files: raw,
    /// excluding the graph's depth ≤ 1 set, excluding its depth ≤ 3 set.
    pub rank: [Option<u8>; 3],
    /// Length of those lists (each capped at 20).
    pub len: [u8; 3],
    /// How many of the first K (5, 10, 20) entries of the raw list are in
    /// the graph's depth ≤ 1 set.
    pub raw_in_d1: [u8; 3],
}

/// One changed file alone as the seed: its raw co-change list.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GainSingle {
    pub len: u8,
    /// Positions in the list holding another non-test changed file of the commit.
    pub hits: Vec<u8>,
    pub others: u32,
    /// The same for the list without the seed's graph depth ≤ 1 set
    /// (either direction): what would be shown next to the graph.
    pub len_ex1: u8,
    pub hits_ex1: Vec<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GainResult {
    pub config: String,
    pub loo: Vec<GainCase>,
    pub single: Vec<GainSingle>,
}

/// Graph-vs-history comparison for every configuration on one commit.
pub fn gain(ctx: &Ctx, truth: &Truth, configs: &[GainConfig]) -> Vec<GainResult> {
    use crate::predict::{GraphPredictor, cochange};
    use std::collections::HashSet;
    let graph = GraphPredictor { name: "file", min_confidence: crate::adapter::Confidence::Heuristic, file_level: true };
    let targets = &truth.targets;
    let mut out: Vec<GainResult> = configs.iter().map(|c| GainResult { config: c.name.clone(), loo: Vec::new(), single: Vec::new() }).collect();
    let none = HashSet::new();
    if targets.len() >= 2 {
        for (i, f) in targets.iter().enumerate() {
            let seeds: Vec<Seed> = targets.iter().enumerate().filter(|&(j, _)| j != i).map(|(_, s)| s.clone()).collect();
            let p = graph.predict(ctx, &seeds, D as u32);
            let mut depth: HashMap<NodeId, u32> = p.dependents.iter().copied().collect();
            for &(n, d) in &p.dependencies {
                let e = depth.entry(n).or_insert(d);
                *e = (*e).min(d);
            }
            let d1: HashSet<NodeId> = depth.iter().filter(|e| *e.1 <= 1).map(|e| *e.0).collect();
            let d3: HashSet<NodeId> = depth.keys().copied().collect();
            let path = ctx.index.path(f.node).to_string();
            for (c, r) in configs.iter().zip(out.iter_mut()) {
                let q = ripple_history::PartnerQuery { limit: KMAX, ..c.query };
                let lists = [cochange(ctx, c.history, &q, &seeds, &none), cochange(ctx, c.history, &q, &seeds, &d1), cochange(ctx, c.history, &q, &seeds, &d3)];
                r.loo.push(GainCase {
                    file: path.clone(),
                    bucket: bucket(&path),
                    graph: depth.get(&f.node).copied(),
                    rank: std::array::from_fn(|k| lists[k].iter().position(|&n| n == f.node).map(|x| x as u8)),
                    len: std::array::from_fn(|k| lists[k].len() as u8),
                    raw_in_d1: std::array::from_fn(|k| lists[0].iter().take(KS[k]).filter(|n| d1.contains(n)).count() as u8),
                });
            }
        }
    }
    for (i, f) in targets.iter().enumerate() {
        let one = std::slice::from_ref(f);
        let p = graph.predict(ctx, one, 1);
        let d1: HashSet<NodeId> = p.dependents.iter().chain(&p.dependencies).map(|&(n, _)| n).collect();
        let hits_of = |list: &[NodeId]| -> Vec<u8> { list.iter().enumerate().filter(|(_, n)| targets.iter().enumerate().any(|(j, t)| j != i && t.node == **n)).map(|(x, _)| x as u8).collect() };
        for (c, r) in configs.iter().zip(out.iter_mut()) {
            let q = ripple_history::PartnerQuery { limit: KMAX, ..c.query };
            let list = cochange(ctx, c.history, &q, one, &none);
            let ex1 = cochange(ctx, c.history, &q, one, &d1);
            r.single.push(GainSingle { len: list.len() as u8, hits: hits_of(&list), others: targets.len() as u32 - 1, len_ex1: ex1.len() as u8, hits_ex1: hits_of(&ex1) });
        }
    }
    out
}

/// Where a file lives, for the miss classification.
pub fn bucket(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    let ext = lower.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    if lower.starts_with("db/migrate/") || lower.starts_with("db/schema") || lower.starts_with("db/structure") || lower.contains("/db/migrate/") {
        "db/migrate|schema"
    } else if lower.starts_with("config/locales/") || lower.contains("/config/locales/") {
        "config/locales"
    } else if lower.starts_with("config/") {
        "config/other"
    } else if matches!(ext, "yml" | "yaml" | "json") {
        "yml/json"
    } else if language(path) == "template" || lower.starts_with("app/views/") || matches!(ext, "html" | "hbs" | "mustache") {
        "views/templates"
    } else if language(path) == "js" {
        "js/ts"
    } else if language(path) == "ruby" {
        "ruby"
    } else if matches!(ext, "md" | "markdown" | "txt" | "rdoc" | "adoc") || lower.starts_with("doc/") || lower.starts_with("docs/") {
        "docs"
    } else {
        "other"
    }
}

pub const BUCKETS: [&str; 9] = ["db/migrate|schema", "config/locales", "config/other", "yml/json", "views/templates", "js/ts", "ruby", "docs", "other"];

fn sizes(index: &Index, depths: &HashMap<NodeId, u32>) -> [u32; D] {
    let mut out = [0u32; D];
    for (&n, &d) in depths {
        if index.is_test(n) {
            continue;
        }
        for (i, slot) in out.iter_mut().enumerate() {
            if d as usize <= i + 1 {
                *slot += 1;
            }
        }
    }
    out
}

/// Runs every measurement of one predictor on one commit.
pub fn measure(ctx: &Ctx, truth: &Truth, pred: &dyn Predictor) -> VariantResult {
    let index = ctx.index;
    let nontest = (0..index.node_count() as NodeId).filter(|&n| !index.is_test(n)).count() as u32;
    let targets = &truth.targets;

    // Leave-one-out.
    let mut loo = Vec::new();
    if targets.len() >= 2 {
        for (i, f) in targets.iter().enumerate() {
            let seeds: Vec<Seed> = targets.iter().enumerate().filter(|&(j, _)| j != i).map(|(_, s)| s.clone()).collect();
            let p = pred.predict(ctx, &seeds, D as u32);
            let rev: HashMap<NodeId, u32> = p.dependents.iter().copied().collect();
            let mut both = rev.clone();
            for &(n, d) in &p.dependencies {
                let e = both.entry(n).or_insert(d);
                *e = (*e).min(d);
            }
            let path = index.path(f.node).to_string();
            loo.push(LooCase {
                bucket: bucket(&path),
                file: path,
                rev: rev.get(&f.node).copied(),
                fwd: p.dependencies.iter().find(|(n, _)| *n == f.node).map(|&(_, d)| d),
                size_rev: sizes(index, &rev),
                size_union: sizes(index, &both),
                pool: nontest - seeds.len() as u32,
            });
        }
    }

    // Single seed.
    let mut single = Vec::new();
    for (i, f) in targets.iter().enumerate() {
        let p = pred.predict(ctx, std::slice::from_ref(f), D as u32);
        let rev: HashMap<NodeId, u32> = p.dependents.iter().copied().collect();
        let mut hits = [0u32; D];
        for (j, o) in targets.iter().enumerate() {
            if j == i {
                continue;
            }
            if let Some(&d) = rev.get(&o.node) {
                for (k, slot) in hits.iter_mut().enumerate() {
                    if d as usize <= k + 1 {
                        *slot += 1;
                    }
                }
            }
        }
        single.push(SingleCase { file: index.path(f.node).to_string(), size: sizes(index, &rev), hits, others: targets.len() as u32 - 1 });
    }

    // Tests reaching the change.
    let tests = (pred.is_graph() && !truth.tests.is_empty() && !targets.is_empty()).then(|| {
        let reached = |depth: u32| -> Vec<NodeId> { pred.predict(ctx, targets, depth).dependents.into_iter().map(|(n, _)| n).filter(|&n| index.is_test(n)).collect() };
        let (d3, all) = (reached(3), reached(0));
        let count = |set: &[NodeId]| truth.tests.iter().filter(|t| set.contains(t)).count() as u32;
        let missed_d3 = truth.tests.iter().filter(|t| !d3.contains(t)).map(|&t| index.path(t).to_string()).collect();
        TestsCase { changed: truth.tests.len() as u32, reached_d3: count(&d3), reached_all: count(&all), size_d3: d3.len() as u32, size_all: all.len() as u32, missed_d3 }
    });

    VariantResult { variant: pred.name().to_string(), loo, single, tests }
}

/// Nearest-rank percentile of an unsorted list (0 for empty).
pub fn percentile(values: &[u32], p: f64) -> u32 {
    if values.is_empty() {
        return 0;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let rank = ((p / 100.0) * v.len() as f64).ceil().max(1.0) as usize;
    v[rank.min(v.len()) - 1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{Confidence, FileChange, Status, hunk};
    use crate::predict::{GraphPredictor, SameDir};
    use ripple_core::model::ScopeRange;
    use ripple_core::{Confidence::*, Edge, EdgeKind::*};
    use std::collections::BTreeMap;

    /// ```text
    /// app/c.rb -> app/b.rb -> app/a.rb        (exact)
    /// app/d.rb -> app/a.rb                    (heuristic)
    /// lib/e.rb -> app/c.rb                    (exact)
    /// spec/a_spec.rb -> app/a.rb ; spec/c_spec.rb -> app/c.rb
    /// svc.rb#run -> app/a.rb#charge           (method-level)
    /// lonely: config/x.yml
    /// ```
    fn index() -> Index {
        let edges = vec![
            Edge::new("app/b.rb", "app/a.rb", RubyConstant, Exact),
            Edge::new("app/c.rb", "app/b.rb", RubyConstant, Exact),
            Edge::new("app/d.rb", "app/a.rb", ApiCall, Heuristic),
            Edge::new("lib/e.rb", "app/c.rb", RubyConstant, Exact),
            Edge::new("spec/a_spec.rb", "app/a.rb", RubyConstant, Exact),
            Edge::new("spec/c_spec.rb", "app/c.rb", RubyConstant, Exact),
            Edge::new("svc.rb", "app/a.rb", RubyConstant, Exact).from_scope("run").to_scope("charge"),
        ];
        let mut scopes = BTreeMap::new();
        scopes.insert("app/a.rb".to_string(), vec![ScopeRange { name: "charge".into(), start: 2, end: 5 }, ScopeRange { name: "refund".into(), start: 7, end: 9 }]);
        Index::synthetic(&edges, &["config/x.yml"], scopes)
    }

    fn seed(ix: &Index, p: &str) -> Seed {
        Seed { node: ix.node(p).unwrap(), scopes: None }
    }

    fn file() -> GraphPredictor {
        GraphPredictor { name: "file", min_confidence: Confidence::Heuristic, file_level: true }
    }

    #[test]
    fn leave_one_out_both_directions() {
        let ix = index();
        let ctx = Ctx { index: &ix, diff: &[], history: &[] };
        // Commit changed a, c and the yml. Spec of a also changed.
        let truth = Truth { targets: ["app/a.rb", "app/c.rb", "config/x.yml"].iter().map(|p| seed(&ix, p)).collect(), tests: vec![ix.node("spec/a_spec.rb").unwrap()] };
        let r = measure(&ctx, &truth, &file());
        let by: HashMap<&str, &LooCase> = r.loo.iter().map(|c| (c.file.as_str(), c)).collect();
        // c depends on a at depth 2 (c -> b -> a): a dependent of {a, yml}.
        assert_eq!((by["app/c.rb"].rev, by["app/c.rb"].fwd), (Some(2), None));
        // a is a dependency of c at depth 2, not a dependent.
        assert_eq!((by["app/a.rb"].rev, by["app/a.rb"].fwd, by["app/a.rb"].union()), (None, Some(2), Some(2)));
        // The yml is invisible.
        assert_eq!(by["config/x.yml"].union(), None);
        assert_eq!(by["config/x.yml"].bucket, "config/other");
        // Seeds {a, yml}: dependents b, d, svc (1), c (2), e (3); spec files never count.
        assert_eq!(by["app/c.rb"].size_rev, [3, 4, 5]);
        // Seeds {c, yml}: dependents e (1); dependencies b (1), a (2).
        assert_eq!(by["app/a.rb"].size_union, [2, 3, 3]);
        // 9 nodes, 2 of them specs, 2 seeds.
        assert_eq!(by["app/a.rb"].pool, 5);
        // Single seed a: b, d, svc, c, e; of the others only c (depth 2).
        let a = r.single.iter().find(|s| s.file == "app/a.rb").unwrap();
        assert_eq!((a.size, a.hits, a.others), ([3, 4, 5], [0, 1, 1], 2));
        // Tests: a_spec and c_spec both reach; a_spec was changed.
        assert_eq!(r.tests, Some(TestsCase { changed: 1, reached_d3: 1, reached_all: 1, size_d3: 2, size_all: 2, missed_d3: vec![] }));
    }

    #[test]
    fn confidence_and_method_seeds_narrow_the_prediction() {
        let ix = index();
        let ctx = Ctx { index: &ix, diff: &[], history: &[] };
        let single = |p: &dyn Predictor, s: Seed| measure(&ctx, &Truth { targets: vec![s], tests: vec![] }, p).single[0].size;
        let exact = GraphPredictor { name: "x", min_confidence: Confidence::Exact, file_level: true };
        assert_eq!(single(&file(), seed(&ix, "app/a.rb")), [3, 4, 5]);
        assert_eq!(single(&exact, seed(&ix, "app/a.rb")), [2, 3, 4], "d is heuristic");
        // A diff inside `refund` (lines 7..9 of a): only svc's `charge` edge is scoped, so it drops.
        let change = FileChange { path: "app/a.rb".into(), parent_path: Some("app/a.rb".into()), status: Status::Modified, hunks: Some(vec![hunk(8, 1)]) };
        let scopes = ix.seed_scopes("app/a.rb", change.status, change.hunks.as_deref());
        assert_eq!(scopes, Some(vec!["refund".to_string()]));
        let method = GraphPredictor { name: "m", min_confidence: Confidence::Heuristic, file_level: false };
        let s = Seed { node: ix.node("app/a.rb").unwrap(), scopes };
        assert_eq!(single(&method, s.clone()), [2, 3, 4]);
        assert_eq!(single(&file(), s), [3, 4, 5], "file-level ignores the scopes");
        // The def line itself is whole-file.
        assert_eq!(ix.seed_scopes("app/a.rb", Status::Modified, Some(&[hunk(7, 1)])), None);
    }

    #[test]
    fn same_dir_baseline() {
        let ix = index();
        let ctx = Ctx { index: &ix, diff: &[], history: &[] };
        let truth = Truth { targets: ["app/a.rb", "lib/e.rb"].iter().map(|p| seed(&ix, p)).collect(), tests: vec![] };
        let r = measure(&ctx, &truth, &SameDir);
        let by: HashMap<&str, &LooCase> = r.loo.iter().map(|c| (c.file.as_str(), c)).collect();
        assert_eq!(by["app/a.rb"].rev, None, "only lib/ is predicted");
        assert_eq!((by["lib/e.rb"].rev, by["lib/e.rb"].size_rev), (None, [3, 3, 3]), "app/b, c, d");
        assert!(r.tests.is_none());
    }

    /// History: config/x.yml changes with app/a.rb (3 of 3), app/b.rb with
    /// app/a.rb once, spec/a_spec.rb with app/a.rb (a test: never listed).
    fn history() -> ripple_history::History {
        let o = ripple_history::Options { min_together: 1, ..Default::default() };
        let c: Vec<(&str, i64, &[&str])> = vec![
            ("c5", 5, &["app/a.rb", "config/x.yml", "spec/a_spec.rb"]),
            ("c4", 4, &["app/a.rb", "config/x.yml", "spec/a_spec.rb"]),
            ("c3", 3, &["app/a.rb", "config/x.yml", "app/b.rb"]),
            ("c2", 2, &["lib/e.rb"]),
            ("c1", 1, &["app/d.rb"]),
            ("c0", 0, &["app/c.rb"]),
        ];
        ripple_history::History::from_commits("p", &o, &c)
    }

    #[test]
    fn cochange_predictors_and_gain() {
        use crate::predict::{CoChange, cochange_variants};
        let ix = index();
        let h = [history()];
        let ctx = Ctx { index: &ix, diff: &[], history: &h };
        let truth = Truth { targets: ["app/a.rb", "config/x.yml"].iter().map(|p| seed(&ix, p)).collect(), tests: vec![] };
        // Alone, unfiltered: x.yml is predicted from a.rb and vice versa; the spec never is.
        let q = ripple_history::PartnerQuery { limit: 5, min_together: 1, min_confidence: 0.0, min_lift: 0.0, ..Default::default() };
        let alone = CoChange { name: "c".into(), history: 0, query: q, graph_depth1: false };
        let r = measure(&ctx, &truth, &alone);
        let by: HashMap<&str, &LooCase> = r.loo.iter().map(|c| (c.file.as_str(), c)).collect();
        assert_eq!((by["config/x.yml"].rev, by["app/a.rb"].rev), (Some(1), Some(1)));
        // From a.rb: x.yml (3/3) then b.rb (1/3).
        assert_eq!(by["config/x.yml"].size_rev, [2, 2, 2]);
        // With graph depth 1: b, d, svc (dependents of a) + x.yml; b is not counted twice.
        let both = CoChange { name: "g".into(), history: 0, query: q, graph_depth1: true };
        let r = measure(&ctx, &truth, &both);
        let x = r.loo.iter().find(|c| c.file == "config/x.yml").unwrap();
        assert_eq!((x.union(), x.size_union[0]), (Some(1), 4));
        assert!(r.tests.is_none());
        // Gain: the graph misses x.yml at any depth; history recovers it at rank 0 in every list.
        let g = gain(&ctx, &truth, &[GainConfig { name: "u".into(), history: 0, query: q }]);
        let x = g[0].loo.iter().find(|c| c.file == "config/x.yml").unwrap();
        assert_eq!((x.graph, x.rank, x.len), (None, [Some(0); 3], [2, 1, 1]), "b.rb is a graph neighbour of a.rb");
        assert_eq!(x.raw_in_d1, [1, 1, 1]);
        let a = g[0].loo.iter().find(|c| c.file == "app/a.rb").unwrap();
        assert_eq!((a.graph, a.rank[0]), (None, Some(0)));
        assert_eq!(g[0].single.iter().map(|s| (s.len, s.hits.clone())).collect::<Vec<_>>(), [(2, vec![0]), (2, vec![0])]);
        // Next to the graph: a.rb's list loses b.rb (a direct dependent).
        assert_eq!((g[0].single[0].len_ex1, g[0].single[0].hits_ex1.clone()), (1, vec![0]));
        // Default filters (t≥3, conf≥0.3, lift≥2): a.rb -> x.yml passes (3/3, base rate 3/6).
        assert_eq!(cochange_variants().len(), 6);
        let d = &cochange_variants()[0];
        let r = measure(&ctx, &truth, d.as_ref());
        assert_eq!(r.loo.iter().find(|c| c.file == "config/x.yml").unwrap().rev, Some(1));
        // No history: empty predictions, never a panic.
        let ctx = Ctx { index: &ix, diff: &[], history: &[] };
        assert!(measure(&ctx, &truth, d.as_ref()).loo.iter().all(|c| c.rev.is_none()));
    }

    #[test]
    fn buckets_and_percentiles() {
        assert_eq!(bucket("db/migrate/2024_x.rb"), "db/migrate|schema");
        assert_eq!(bucket("db/schema.rb"), "db/migrate|schema");
        assert_eq!(bucket("config/locales/en.yml"), "config/locales");
        assert_eq!(bucket("config/routes.rb"), "config/other");
        assert_eq!(bucket("app/javascript/x.json"), "yml/json");
        assert_eq!(bucket("app/views/a/show.html.erb"), "views/templates");
        assert_eq!(bucket("app/javascript/a.tsx"), "js/ts");
        assert_eq!(bucket("app/models/a.rb"), "ruby");
        assert_eq!(bucket("docs/README.md"), "docs");
        assert_eq!(bucket("app/assets/x.scss"), "other");
        assert_eq!(percentile(&[5, 1, 3, 2, 4], 50.0), 3);
        assert_eq!(percentile(&[5, 1, 3, 2, 4], 90.0), 5);
        assert_eq!(percentile(&[], 50.0), 0);
    }
}
