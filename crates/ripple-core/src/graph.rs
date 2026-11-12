//! Immutable dependency graph in compressed-sparse-row form, plus the impact
//! query (reverse breadth-first search). Built once per index generation;
//! queries are allocation-light and run in microseconds to low milliseconds.

use crate::model::{Confidence, Edge, EdgeKind, RelPath};
use std::collections::{HashMap, HashSet, VecDeque};

pub type NodeId = u32;

/// Edge payload stored alongside the adjacency arrays.
#[derive(Debug, Clone, Copy)]
pub struct EdgeData {
    pub kind: EdgeKind,
    pub confidence: Confidence,
    pub line: u32,
    /// Index into `Graph::details`, or `u32::MAX` for none.
    detail: u32,
    /// Interned `Edge::from_scope` / `Edge::to_scope`, or `u32::MAX` for none.
    from_scope: u32,
    to_scope: u32,
    inherit: bool,
}

#[derive(Debug, Default)]
struct Csr {
    /// `offsets[n]..offsets[n+1]` indexes `targets`/`edge_ids` for node `n`.
    offsets: Vec<u32>,
    targets: Vec<NodeId>,
    edge_ids: Vec<u32>,
}

impl Csr {
    fn build(n: usize, pairs: impl Iterator<Item = (NodeId, NodeId)> + Clone) -> Self {
        let mut offsets = vec![0u32; n + 1];
        for (src, _) in pairs.clone() {
            offsets[src as usize + 1] += 1;
        }
        for i in 0..n {
            offsets[i + 1] += offsets[i];
        }
        let m = offsets[n] as usize;
        let mut targets = vec![0; m];
        let mut edge_ids = vec![0; m];
        let mut cursor = offsets.clone();
        for (edge_id, (src, dst)) in pairs.enumerate() {
            let slot = cursor[src as usize] as usize;
            targets[slot] = dst;
            edge_ids[slot] = edge_id as u32;
            cursor[src as usize] += 1;
        }
        Self { offsets, targets, edge_ids }
    }

    #[inline]
    fn neighbours(&self, n: NodeId) -> impl Iterator<Item = (NodeId, u32)> + '_ {
        let (a, b) = (self.offsets[n as usize] as usize, self.offsets[n as usize + 1] as usize);
        self.targets[a..b].iter().copied().zip(self.edge_ids[a..b].iter().copied())
    }
}

#[derive(Debug, Default)]
pub struct Graph {
    paths: Vec<RelPath>,
    index: HashMap<RelPath, NodeId>,
    /// (from, to) per edge id.
    endpoints: Vec<(NodeId, NodeId)>,
    data: Vec<EdgeData>,
    details: Vec<String>,
    scopes: Vec<String>,
    scope_index: HashMap<String, u32>,
    /// Number of distinct other files depending on each node / each node
    /// depends on (edges can repeat per scope, and scoped self-edges exist).
    distinct_in: Vec<u32>,
    distinct_out: Vec<u32>,
    /// from -> to ("depends on").
    forward: Csr,
    /// to -> from ("is depended on by").
    reverse: Csr,
}

#[derive(Debug, Clone, Copy)]
pub struct QueryOpts {
    /// 0 = unlimited.
    pub max_depth: u32,
    pub min_confidence: Confidence,
    /// Treat every file as one unit: ignore method/action scopes on edges and
    /// seeds. Gives the coarser file-level answer, a superset of the scoped one.
    pub file_level: bool,
}

impl Default for QueryOpts {
    fn default() -> Self {
        Self { max_depth: 0, min_confidence: Confidence::Heuristic, file_level: false }
    }
}

/// One impacted file. `via`/`edge` give the BFS tree parent, so the shortest
/// explanation path back to a changed file can be reconstructed.
#[derive(Debug, Clone, Copy)]
pub struct Hit {
    pub node: NodeId,
    /// 0 for the seeds (changed files) themselves.
    pub depth: u32,
    /// The node one hop closer to a seed; equals `node` for seeds.
    pub via: NodeId,
    /// Edge id connecting `node -> via`; `u32::MAX` for seeds.
    pub edge: u32,
}

/// Result of an impact query, in BFS order (seeds first, then by depth;
/// ties broken by node id so output is deterministic).
#[derive(Debug, Default)]
pub struct Impact {
    pub hits: Vec<Hit>,
    /// node id -> index into `hits`, or `u32::MAX`.
    position: Vec<u32>,
    /// Scope-aware search tree; `path` walks this so the explanation uses the
    /// reference that actually let the search through. Empty for forward queries.
    acts: Vec<Act>,
    /// hit index -> index into `acts` of the activation that first reached it.
    hit_act: Vec<u32>,
}

/// One (file, scope) reached by the scope-aware search.
#[derive(Debug, Clone, Copy)]
struct Act {
    node: NodeId,
    /// `u32::MAX` = whole file.
    scope: u32,
    parent: u32,
    edge: u32,
    depth: u32,
    /// Reached through an edge inside the same file (method calls method).
    internal: bool,
}

impl Impact {
    pub fn get(&self, node: NodeId) -> Option<&Hit> {
        match self.position.get(node as usize) {
            Some(&p) if p != u32::MAX => Some(&self.hits[p as usize]),
            _ => None,
        }
    }
    pub fn contains(&self, node: NodeId) -> bool {
        self.get(node).is_some()
    }
    /// Hits excluding the seeds.
    pub fn impacted(&self) -> impl Iterator<Item = &Hit> {
        self.hits.iter().filter(|h| h.depth > 0)
    }
    /// Chain of (node, edge-to-next) from `node` back to the seed it was reached from.
    /// The last element is the seed with edge `u32::MAX`.
    pub fn path(&self, node: NodeId) -> Vec<(NodeId, u32)> {
        let mut out = Vec::new();
        if !self.acts.is_empty() {
            let Some(&p) = self.position.get(node as usize).filter(|&&p| p != u32::MAX) else { return out };
            let mut a = self.hit_act[p as usize];
            loop {
                let act = self.acts[a as usize];
                // Hops inside one file are not shown: the path lists files.
                if !act.internal {
                    out.push((act.node, act.edge));
                }
                if act.edge == u32::MAX {
                    break;
                }
                a = act.parent;
            }
            return out;
        }
        let mut cur = node;
        while let Some(hit) = self.get(cur) {
            out.push((hit.node, hit.edge));
            if hit.depth == 0 {
                break;
            }
            cur = hit.via;
        }
        out
    }
}

impl Graph {
    /// Builds the graph. Duplicate `(from, to, kind, scopes)` edges are collapsed,
    /// keeping the highest confidence (then lowest line). Self-edges are dropped
    /// unless they connect two different scopes of the file (method calls method).
    /// `extra_nodes` lets files with no edges still be addressable.
    pub fn build<'a>(edges: impl IntoIterator<Item = &'a Edge>, extra_nodes: impl IntoIterator<Item = &'a str>) -> Self {
        // Intern paths first so dedup and ordering work on integers, not strings:
        // this runs on every index generation and dominates update latency.
        let edges: Vec<&Edge> = edges.into_iter().filter(|e| e.from != e.to || (!e.inherit && e.to_scope.is_some() && e.from_scope != e.to_scope)).collect();
        let mut seen: HashMap<&str, u32> = HashMap::with_capacity(edges.len() / 4);
        let mut names: Vec<&str> = Vec::new();
        let mut intern = |s: &'a str| -> u32 {
            *seen.entry(s).or_insert_with(|| {
                names.push(s);
                (names.len() - 1) as u32
            })
        };
        let raw: Vec<(u32, u32)> = edges.iter().map(|e| (intern(&e.from), intern(&e.to))).collect();
        for extra in extra_nodes {
            intern(extra);
        }
        // Node ids follow sorted path order so they are stable across runs.
        let mut order: Vec<u32> = (0..names.len() as u32).collect();
        order.sort_unstable_by_key(|&i| names[i as usize]);
        let mut remap = vec![0 as NodeId; names.len()];
        for (new, &old) in order.iter().enumerate() {
            remap[old as usize] = new as NodeId;
        }
        let paths: Vec<RelPath> = order.iter().map(|&i| names[i as usize].to_string()).collect();
        let index: HashMap<RelPath, NodeId> = paths.iter().enumerate().map(|(i, p)| (p.clone(), i as NodeId)).collect();

        // Sort so the best duplicate (highest confidence, then lowest line) comes first, then dedup.
        let mut scope_ids: HashMap<&str, u32> = HashMap::new();
        let mut scopes: Vec<String> = Vec::new();
        let mut scope = |s: &'a Option<String>| -> u32 {
            match s {
                None => u32::MAX,
                Some(s) => *scope_ids.entry(s.as_str()).or_insert_with(|| {
                    scopes.push(s.clone());
                    (scopes.len() - 1) as u32
                }),
            }
        };
        let scoped: Vec<(u32, u32, bool)> = edges.iter().map(|e| if e.inherit { (u32::MAX, u32::MAX, true) } else { (scope(&e.from_scope), scope(&e.to_scope), false) }).collect();
        let mut keyed: Vec<(NodeId, NodeId, EdgeKind, (u32, u32, bool), std::cmp::Reverse<Confidence>, u32, u32)> = raw
            .iter()
            .enumerate()
            .map(|(i, &(f, t))| (remap[f as usize], remap[t as usize], edges[i].kind, scoped[i], std::cmp::Reverse(edges[i].confidence), edges[i].line, i as u32))
            .collect();
        keyed.sort_unstable();
        keyed.dedup_by_key(|k| (k.0, k.1, k.2, k.3));
        // An edge with no scope on one side already covers every scope on that
        // side ("the whole file depends" / "depends on the whole file"), so a
        // more specific twin of the same from/to/kind adds nothing: drop it.
        let present: HashSet<(NodeId, NodeId, EdgeKind, u32, u32)> = keyed.iter().filter(|k| !k.3 .2).map(|k| (k.0, k.1, k.2, k.3 .0, k.3 .1)).collect();
        const ANY: u32 = u32::MAX;
        keyed.retain(|k| {
            let (fs, ts, inherit) = k.3;
            inherit
                || !((fs != ANY && present.contains(&(k.0, k.1, k.2, ANY, ts)))
                    || (ts != ANY && present.contains(&(k.0, k.1, k.2, fs, ANY)))
                    || (fs != ANY && ts != ANY && present.contains(&(k.0, k.1, k.2, ANY, ANY))))
        });

        let mut details = Vec::new();
        let mut endpoints = Vec::with_capacity(keyed.len());
        let mut data = Vec::with_capacity(keyed.len());
        for k in &keyed {
            let e = edges[k.6 as usize];
            endpoints.push((k.0, k.1));
            let detail = match &e.detail {
                Some(d) => {
                    details.push(d.clone());
                    (details.len() - 1) as u32
                }
                None => u32::MAX,
            };
            data.push(EdgeData { kind: e.kind, confidence: e.confidence, line: e.line, detail, from_scope: k.3 .0, to_scope: k.3 .1, inherit: k.3 .2 });
        }
        let n = paths.len();
        let forward = Csr::build(n, endpoints.iter().copied());
        let reverse = Csr::build(n, endpoints.iter().map(|&(f, t)| (t, f)));
        let mut distinct_in = vec![0u32; n];
        let mut distinct_out = vec![0u32; n];
        let mut last: Option<(NodeId, NodeId)> = None;
        // `endpoints` is sorted by (from, to, ..), so duplicates are adjacent.
        for &(f, t) in &endpoints {
            if f != t && last != Some((f, t)) {
                distinct_in[t as usize] += 1;
                distinct_out[f as usize] += 1;
            }
            last = Some((f, t));
        }
        drop(scope);
        let scope_index = scopes.iter().enumerate().map(|(i, s)| (s.clone(), i as u32)).collect();
        Self { paths, index, endpoints, data, details, scopes, scope_index, distinct_in, distinct_out, forward, reverse }
    }

    pub fn node_count(&self) -> usize {
        self.paths.len()
    }
    pub fn edge_count(&self) -> usize {
        self.endpoints.len()
    }
    pub fn id(&self, path: &str) -> Option<NodeId> {
        self.index.get(path).copied()
    }
    pub fn path(&self, node: NodeId) -> &str {
        &self.paths[node as usize]
    }
    pub fn paths(&self) -> &[RelPath] {
        &self.paths
    }
    pub fn edge(&self, edge: u32) -> (NodeId, NodeId, EdgeData) {
        let (f, t) = self.endpoints[edge as usize];
        (f, t, self.data[edge as usize])
    }
    pub fn edge_detail(&self, edge: u32) -> Option<&str> {
        match self.data[edge as usize].detail {
            u32::MAX => None,
            i => Some(&self.details[i as usize]),
        }
    }
    /// Files that directly depend on `node`, with the connecting edge id.
    pub fn dependents(&self, node: NodeId) -> impl Iterator<Item = (NodeId, u32)> + '_ {
        self.reverse.neighbours(node).filter(move |&(n, _)| n != node)
    }
    /// Files `node` directly depends on, with the connecting edge id.
    pub fn dependencies(&self, node: NodeId) -> impl Iterator<Item = (NodeId, u32)> + '_ {
        self.forward.neighbours(node).filter(move |&(n, _)| n != node)
    }
    /// `(from_scope, to_scope)` of an edge; see [`Edge::from_scope`].
    pub fn edge_scopes(&self, edge: u32) -> (Option<&str>, Option<&str>) {
        let d = &self.data[edge as usize];
        let get = |i: u32| (i != u32::MAX).then(|| self.scopes[i as usize].as_str());
        (get(d.from_scope), get(d.to_scope))
    }
    /// Number of distinct files that directly depend on `node`.
    pub fn fan_in(&self, node: NodeId) -> usize {
        self.distinct_in[node as usize] as usize
    }
    /// Number of distinct files `node` directly depends on.
    pub fn fan_out(&self, node: NodeId) -> usize {
        self.distinct_out[node as usize] as usize
    }
    /// True when the edge is an inheritance edge; see [`Edge::inherit`].
    pub fn edge_inherits(&self, edge: u32) -> bool {
        self.data[edge as usize].inherit
    }

    /// Everything that may be affected if `seeds` change entirely: reverse BFS.
    pub fn impact(&self, seeds: &[NodeId], opts: QueryOpts) -> Impact {
        let seeds: Vec<(NodeId, Option<Vec<String>>)> = seeds.iter().map(|&s| (s, None)).collect();
        self.impact_scoped(&seeds, opts)
    }

    /// Everything that may be affected by a change to `seeds`, where a seed
    /// may be limited to some of its scopes (`Some(["refund!", "charge!"])` =
    /// only those methods changed; `None` = the whole file changed).
    ///
    /// Each reached file carries the set of its scopes that are affected, or
    /// "all of it". An edge is followed from `to` when it has no `to_scope`,
    /// or `to` is wholly affected, or its `to_scope` is one of the affected
    /// scopes; the dependent then becomes affected in the edge's `from_scope`
    /// (whole file if none). Inheritance edges pass the affected scope on
    /// unchanged. Edges inside one file (method calls method) cost no depth.
    pub fn impact_scoped(&self, seeds: &[(NodeId, Option<Vec<String>>)], opts: QueryOpts) -> Impact {
        const ALL: u32 = u32::MAX;
        /// A named scope no edge refers to: only whole-file and inheritance
        /// edges lead on from it.
        const NOWHERE: u32 = u32::MAX - 1;
        /// "Nothing another file can depend on changed": leads nowhere.
        const NOTHING: u32 = u32::MAX - 2;
        let mut position = vec![u32::MAX; self.paths.len()];
        let mut whole = vec![false; self.paths.len()];
        let mut partial: HashSet<(NodeId, u32)> = HashSet::new();
        let mut found: Vec<(Hit, u32)> = Vec::new();
        let mut acts: Vec<Act> = Vec::new();
        let mut queue: VecDeque<u32> = VecDeque::new();

        let mut merged: std::collections::BTreeMap<NodeId, Option<Vec<u32>>> = std::collections::BTreeMap::new();
        for (node, scopes) in seeds {
            let ids = scopes.as_ref().filter(|_| !opts.file_level).and_then(|names| {
                names.iter().map(|n| if n == crate::model::SCOPE_BODY { None } else { Some(self.scope_index.get(n).copied().unwrap_or(NOWHERE)) }).collect::<Option<Vec<u32>>>()
            });
            match (merged.entry(*node).or_insert_with(|| Some(Vec::new())), ids) {
                (slot, None) => *slot = None,
                (Some(have), Some(ids)) => have.extend(ids),
                (None, Some(_)) => {}
            }
        }
        for (node, scopes) in merged {
            position[node as usize] = 0;
            found.push((Hit { node, depth: 0, via: node, edge: u32::MAX }, acts.len() as u32));
            let mut scopes = match scopes {
                None => vec![ALL],
                Some(v) if v.is_empty() => vec![NOTHING],
                Some(v) => v,
            };
            scopes.sort_unstable();
            scopes.dedup();
            for scope in scopes {
                if scope == ALL {
                    whole[node as usize] = true;
                } else {
                    partial.insert((node, scope));
                }
                queue.push_back(acts.len() as u32);
                acts.push(Act { node, scope, parent: u32::MAX, edge: u32::MAX, depth: 0, internal: false });
            }
        }

        // 0-1 breadth-first search: same-file hops go to the front of the queue.
        while let Some(i) = queue.pop_front() {
            let cur = acts[i as usize];
            if cur.scope == NOTHING {
                continue;
            }
            for (next, edge) in self.reverse.neighbours(cur.node) {
                let d = &self.data[edge as usize];
                if d.confidence < opts.min_confidence || whole[next as usize] {
                    continue;
                }
                let scope = if opts.file_level {
                    ALL
                } else if d.inherit {
                    cur.scope
                } else if d.to_scope != ALL && cur.scope != ALL && d.to_scope != cur.scope {
                    continue;
                } else {
                    d.from_scope
                };
                let internal = next == cur.node;
                let depth = cur.depth + u32::from(!internal);
                if opts.max_depth != 0 && depth > opts.max_depth {
                    continue;
                }
                // A whole-file activation re-walks edges an earlier partial
                // one already followed; `whole`/`partial` make that a no-op.
                if scope == ALL {
                    whole[next as usize] = true;
                } else if !partial.insert((next, scope)) {
                    continue;
                }
                let act = acts.len() as u32;
                acts.push(Act { node: next, scope, parent: i, edge, depth, internal });
                if internal {
                    queue.push_front(act);
                } else {
                    queue.push_back(act);
                }
                if position[next as usize] == u32::MAX {
                    position[next as usize] = 0;
                    found.push((Hit { node: next, depth, via: cur.node, edge }, act));
                }
            }
        }
        // Seeds first, then by depth; ties by node id so output is deterministic.
        found.sort_unstable_by_key(|(h, _)| (h.depth, h.node));
        let mut hits = Vec::with_capacity(found.len());
        let mut hit_act = Vec::with_capacity(found.len());
        for (h, act) in found {
            position[h.node as usize] = hits.len() as u32;
            hits.push(h);
            hit_act.push(act);
        }
        Impact { hits, position, acts, hit_act }
    }

    /// Everything `seeds` rely on: forward BFS. Useful as agent context
    /// ("what do I need to read to understand this file").
    pub fn dependencies_closure(&self, seeds: &[NodeId], opts: QueryOpts) -> Impact {
        self.bfs(&self.forward, seeds, opts)
    }

    fn bfs(&self, csr: &Csr, seeds: &[NodeId], opts: QueryOpts) -> Impact {
        let mut position = vec![u32::MAX; self.paths.len()];
        let mut hits: Vec<Hit> = Vec::new();
        let mut sorted_seeds = seeds.to_vec();
        sorted_seeds.sort_unstable();
        sorted_seeds.dedup();
        for s in sorted_seeds {
            position[s as usize] = hits.len() as u32;
            hits.push(Hit { node: s, depth: 0, via: s, edge: u32::MAX });
        }
        let mut level_start = 0;
        let mut depth = 0;
        while level_start < hits.len() {
            if opts.max_depth != 0 && depth >= opts.max_depth {
                break;
            }
            let level_end = hits.len();
            for i in level_start..level_end {
                let cur = hits[i].node;
                for (next, edge) in csr.neighbours(cur) {
                    if position[next as usize] != u32::MAX || self.data[edge as usize].confidence < opts.min_confidence {
                        continue;
                    }
                    position[next as usize] = hits.len() as u32;
                    hits.push(Hit { node: next, depth: depth + 1, via: cur, edge });
                }
            }
            // Deterministic order within a level regardless of adjacency layout.
            hits[level_end..].sort_unstable_by_key(|h| h.node);
            for (i, h) in hits.iter().enumerate().skip(level_end) {
                position[h.node as usize] = i as u32;
            }
            level_start = level_end;
            depth += 1;
        }
        Impact { hits, position, acts: Vec::new(), hit_act: Vec::new() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Confidence::*, EdgeKind::*};

    fn g() -> Graph {
        // c -> b -> a ; d -> a (heuristic) ; e -> c ; a -> a (self, dropped)
        let edges = vec![
            Edge::new("b", "a", RubyConstant, Exact).at(3).detail("A"),
            Edge::new("c", "b", RubyConstant, Exact),
            Edge::new("d", "a", ApiCall, Heuristic),
            Edge::new("e", "c", JsImport, Exact),
            Edge::new("a", "a", RubyConstant, Exact),
            Edge::new("b", "a", RubyConstant, Inferred).at(1),
        ];
        Graph::build(&edges, ["lonely"])
    }

    fn names(g: &Graph, imp: &Impact) -> Vec<(String, u32)> {
        imp.hits.iter().map(|h| (g.path(h.node).to_string(), h.depth)).collect()
    }

    #[test]
    fn reverse_reachability_with_depths() {
        let g = g();
        assert_eq!(g.edge_count(), 4);
        assert!(g.id("lonely").is_some());
        let imp = g.impact(&[g.id("a").unwrap()], QueryOpts::default());
        assert_eq!(
            names(&g, &imp),
            [("a", 0), ("b", 1), ("d", 1), ("c", 2), ("e", 3)].map(|(s, d)| (s.to_string(), d))
        );
    }

    #[test]
    fn depth_and_confidence_limits() {
        let g = g();
        let a = g.id("a").unwrap();
        let imp = g.impact(&[a], QueryOpts { max_depth: 1, min_confidence: Inferred, file_level: false });
        assert_eq!(names(&g, &imp), [("a", 0), ("b", 1)].map(|(s, d)| (s.to_string(), d)));
    }

    #[test]
    fn path_explains_why() {
        let g = g();
        let imp = g.impact(&[g.id("a").unwrap()], QueryOpts::default());
        let path: Vec<&str> = imp.path(g.id("e").unwrap()).iter().map(|&(n, _)| g.path(n)).collect();
        assert_eq!(path, ["e", "c", "b", "a"]);
        let (_, edge) = imp.path(g.id("b").unwrap())[0];
        assert_eq!(g.edge_detail(edge), Some("A"));
        assert_eq!(g.edge(edge).2.line, 3, "exact edge wins over inferred duplicate");
    }

    #[test]
    fn scopes_limit_what_passes_through_a_controller() {
        // ctrl#index uses model; ctrl#show uses other. page_i calls index, page_s calls show,
        // page_any calls the controller with no action known.
        let edges = vec![
            Edge::new("ctrl", "model", RubyConstant, Exact).from_scope("index").at(5),
            Edge::new("ctrl", "other", RubyConstant, Exact).from_scope("show").at(9),
            Edge::new("ctrl", "base", RubyConstant, Exact),
            Edge::new("page_i", "ctrl", ApiCall, Inferred).to_scope("index"),
            Edge::new("page_s", "ctrl", ApiCall, Inferred).to_scope("show"),
            Edge::new("page_any", "ctrl", ApiCall, Inferred),
            Edge::new("importer", "page_s", JsImport, Exact),
        ];
        let g = Graph::build(&edges, []);
        let run = |seed: &str| {
            let imp = g.impact(&[g.id(seed).unwrap()], QueryOpts::default());
            names(&g, &imp).into_iter().skip(1).collect::<Vec<_>>()
        };
        let s = |v: &[(&str, u32)]| v.iter().map(|&(a, b)| (a.to_string(), b)).collect::<Vec<_>>();
        assert_eq!(run("model"), s(&[("ctrl", 1), ("page_any", 2), ("page_i", 2)]));
        assert_eq!(run("other"), s(&[("ctrl", 1), ("page_any", 2), ("page_s", 2), ("importer", 3)]));
        // Class-level reference and a change to the controller itself reach every action.
        assert_eq!(run("base").len(), 5);
        assert_eq!(run("ctrl"), s(&[("page_any", 1), ("page_i", 1), ("page_s", 1), ("importer", 2)]));
        assert_eq!(g.fan_in(g.id("ctrl").unwrap()), 3);
        // Both changed at once: ctrl is reached twice at depth 1, with different actions.
        let imp = g.impact(&[g.id("model").unwrap(), g.id("other").unwrap()], QueryOpts::default());
        let path: Vec<&str> = imp.path(g.id("page_s").unwrap()).iter().map(|&(n, _)| g.path(n)).collect();
        assert_eq!(path, ["page_s", "ctrl", "other"], "explanation goes through the reference inside #show");
        assert_eq!(g.edge_scopes(imp.path(g.id("page_s").unwrap())[1].1), (Some("show"), None));
    }

    #[test]
    fn method_level_scopes() {
        // model: refund! calls charge! (same file); callback at class level mentions audit.
        // svc#run calls refund!; job#perform calls charge!; report only uses the constant.
        // sub < model; sub_user calls refund! on sub. caller depends on all of svc.
        let edges = vec![
            Edge::new("model", "model", RubyConstant, Exact).from_scope("refund!").to_scope("charge!"),
            Edge::new("model", "model", RubyConstant, Exact).to_scope("audit"),
            Edge::new("svc", "model", RubyConstant, Exact).from_scope("run").to_scope("refund!"),
            Edge::new("svc", "model", RubyConstant, Exact).from_scope("other").to_scope(crate::model::SCOPE_BODY),
            Edge::new("job", "model", RubyConstant, Exact).from_scope("perform").to_scope("charge!"),
            Edge::new("report", "model", RubyConstant, Exact).to_scope(crate::model::SCOPE_BODY),
            Edge::new("sub", "model", RubyConstant, Exact).inherit(),
            Edge::new("sub_user", "sub", RubyConstant, Exact).to_scope("refund!"),
            Edge::new("caller", "svc", RubyConstant, Exact).to_scope("run"),
            Edge::new("caller2", "svc", RubyConstant, Exact).to_scope("other"),
        ];
        let g = Graph::build(&edges, []);
        let id = |n: &str| g.id(n).unwrap();
        let run = |scopes: Option<&[&str]>, depth: u32| {
            let seed = (id("model"), scopes.map(|s| s.iter().map(|x| x.to_string()).collect()));
            let imp = g.impact_scoped(&[seed], QueryOpts { max_depth: depth, ..Default::default() });
            names(&g, &imp).into_iter().skip(1).map(|(n, d)| format!("{n}:{d}")).collect::<Vec<_>>().join(" ")
        };
        // charge! changed: job directly; refund! via the same-file call (no extra depth), so svc#run
        // and its caller; the subclass inherits both. report and caller2 are untouched.
        assert_eq!(run(Some(&["charge!"]), 0), "job:1 sub:1 svc:1 caller:2 sub_user:2");
        assert_eq!(run(Some(&["charge!"]), 1), "job:1 sub:1 svc:1");
        assert_eq!(run(Some(&["refund!"]), 0), "sub:1 svc:1 caller:2 sub_user:2");
        // audit is wired up at class level: the whole model is affected.
        assert_eq!(run(Some(&["audit"]), 0), "job:1 report:1 sub:1 svc:1 caller:2 caller2:2 sub_user:2");
        assert_eq!(run(None, 0), run(Some(&["audit"]), 0));
        assert_eq!(run(Some(&[crate::model::SCOPE_BODY]), 0), run(None, 0));
        // A method nobody mentions, and "no method changed" (comment-only diff), reach nothing.
        // A method no edge names still affects whole-file dependents and heirs.
        assert_eq!(run(Some(&["unknown"]), 0), "sub:1");
        assert_eq!(run(Some(&[]), 0), "");
        let imp = g.impact_scoped(&[(id("model"), Some(vec![]))], QueryOpts { file_level: true, ..Default::default() });
        assert_eq!(imp.hits.len(), 8, "file-level mode ignores scopes");
        // Same-file hops are not listed in the explanation.
        let imp = g.impact_scoped(&[(id("model"), Some(vec!["charge!".into()]))], QueryOpts::default());
        let path: Vec<&str> = imp.path(id("caller")).iter().map(|&(n, _)| g.path(n)).collect();
        assert_eq!(path, ["caller", "svc", "model"]);
        assert_eq!((g.fan_in(id("model")), g.fan_out(id("svc"))), (4, 1));
        assert!(g.dependents(id("model")).all(|(n, _)| n != id("model")));
    }

    #[test]
    fn unscoped_edge_absorbs_scoped_duplicates() {
        let edges = vec![
            Edge::new("ctrl", "model", RubyConstant, Exact).from_scope("index"),
            Edge::new("ctrl", "model", RubyConstant, Exact),
        ];
        assert_eq!(Graph::build(&edges, []).edge_count(), 1);
        let edges = vec![
            Edge::new("a", "b", RubyConstant, Exact).from_scope("x").to_scope("m"),
            Edge::new("a", "b", RubyConstant, Exact).from_scope("x"),
            Edge::new("a", "b", RubyConstant, Exact).from_scope("y").to_scope("m"),
        ];
        assert_eq!(Graph::build(&edges, []).edge_count(), 2);
    }

    #[test]
    fn degrees() {
        let g = g();
        assert_eq!(g.fan_in(g.id("a").unwrap()), 2);
        assert_eq!(g.fan_out(g.id("b").unwrap()), 1);
    }
}
