//! Immutable dependency graph in compressed-sparse-row form, plus the impact
//! query (reverse breadth-first search). Built once per index generation;
//! queries are allocation-light and run in microseconds to low milliseconds.

use crate::model::{Confidence, Edge, EdgeKind, RelPath};
use std::collections::HashMap;

pub type NodeId = u32;

/// Edge payload stored alongside the adjacency arrays.
#[derive(Debug, Clone, Copy)]
pub struct EdgeData {
    pub kind: EdgeKind,
    pub confidence: Confidence,
    pub line: u32,
    /// Index into `Graph::details`, or `u32::MAX` for none.
    detail: u32,
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

    #[inline]
    fn degree(&self, n: NodeId) -> usize {
        (self.offsets[n as usize + 1] - self.offsets[n as usize]) as usize
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
}

impl Default for QueryOpts {
    fn default() -> Self {
        Self { max_depth: 0, min_confidence: Confidence::Heuristic }
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
    /// Builds the graph. Duplicate `(from, to, kind)` edges are collapsed,
    /// keeping the highest confidence (then lowest line). Self-edges are dropped.
    /// `extra_nodes` lets files with no edges still be addressable.
    pub fn build<'a>(edges: impl IntoIterator<Item = &'a Edge>, extra_nodes: impl IntoIterator<Item = &'a str>) -> Self {
        let mut best: HashMap<(&str, &str, EdgeKind), &Edge> = HashMap::new();
        for e in edges {
            if e.from == e.to {
                continue;
            }
            best.entry((e.from.as_str(), e.to.as_str(), e.kind))
                .and_modify(|cur| {
                    if (e.confidence, std::cmp::Reverse(e.line)) > (cur.confidence, std::cmp::Reverse(cur.line)) {
                        *cur = e;
                    }
                })
                .or_insert(e);
        }
        let mut chosen: Vec<&Edge> = best.into_values().collect();
        chosen.sort_by(|a, b| (&a.from, &a.to, a.kind).cmp(&(&b.from, &b.to, b.kind)));

        let mut names: Vec<&str> = chosen.iter().flat_map(|e| [e.from.as_str(), e.to.as_str()]).chain(extra_nodes).collect();
        names.sort_unstable();
        names.dedup();
        let paths: Vec<RelPath> = names.iter().map(|s| s.to_string()).collect();
        let index: HashMap<RelPath, NodeId> = paths.iter().enumerate().map(|(i, p)| (p.clone(), i as NodeId)).collect();

        let mut details = Vec::new();
        let mut endpoints = Vec::with_capacity(chosen.len());
        let mut data = Vec::with_capacity(chosen.len());
        for e in &chosen {
            endpoints.push((index[&e.from], index[&e.to]));
            let detail = match &e.detail {
                Some(d) => {
                    details.push(d.clone());
                    (details.len() - 1) as u32
                }
                None => u32::MAX,
            };
            data.push(EdgeData { kind: e.kind, confidence: e.confidence, line: e.line, detail });
        }
        let n = paths.len();
        let forward = Csr::build(n, endpoints.iter().copied());
        let reverse = Csr::build(n, endpoints.iter().map(|&(f, t)| (t, f)));
        Self { paths, index, endpoints, data, details, forward, reverse }
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
        self.reverse.neighbours(node)
    }
    /// Files `node` directly depends on, with the connecting edge id.
    pub fn dependencies(&self, node: NodeId) -> impl Iterator<Item = (NodeId, u32)> + '_ {
        self.forward.neighbours(node)
    }
    pub fn fan_in(&self, node: NodeId) -> usize {
        self.reverse.degree(node)
    }
    pub fn fan_out(&self, node: NodeId) -> usize {
        self.forward.degree(node)
    }

    /// Everything that may be affected if `seeds` change: reverse BFS.
    pub fn impact(&self, seeds: &[NodeId], opts: QueryOpts) -> Impact {
        self.bfs(&self.reverse, seeds, opts)
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
        Impact { hits, position }
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
        let imp = g.impact(&[a], QueryOpts { max_depth: 1, min_confidence: Inferred });
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
    fn degrees() {
        let g = g();
        assert_eq!(g.fan_in(g.id("a").unwrap()), 2);
        assert_eq!(g.fan_out(g.id("b").unwrap()), 1);
    }
}
