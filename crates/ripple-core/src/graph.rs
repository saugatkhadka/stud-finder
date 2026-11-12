//! Immutable dependency graph in compressed-sparse-row form, plus the impact
//! query (reverse breadth-first search). Built once per index generation;
//! queries are allocation-light and run in microseconds to low milliseconds.
//!
//! Every array is a [`Slab`]: owned when built in memory, or a window into
//! a mapped file when loaded with [`Graph::load`] (see [`crate::store`]).

use crate::mentions::{FileMentions, MentionTable, Mentions, NO_DEF, name_id};
use crate::metrics::{FileMetrics, MethodMetric, MetricsTable};
use crate::model::{Confidence, Edge, EdgeKind};
use crate::store::{Pod, SectionFile, SectionWriter, Slab, StrTable};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

pub type NodeId = u32;

/// Edge payload stored alongside the adjacency arrays.
#[derive(Debug, Clone, Copy)]
pub struct EdgeData {
    pub kind: EdgeKind,
    pub confidence: Confidence,
    pub line: u32,
    pub inherit: bool,
}

/// Every [`EdgeKind`], indexed by its discriminant (the stored byte).
const KINDS: [EdgeKind; 11] = [
    EdgeKind::RubyConstant,
    EdgeKind::RubyRequire,
    EdgeKind::RailsInferred,
    EdgeKind::RailsView,
    EdgeKind::RailsRoute,
    EdgeKind::JsImport,
    EdgeKind::JsDynamicImport,
    EdgeKind::ApiCall,
    EdgeKind::ViewMount,
    EdgeKind::GraphQl,
    EdgeKind::Asset,
];
const CONFIDENCES: [Confidence; 3] = [Confidence::Heuristic, Confidence::Inferred, Confidence::Exact];

/// The stored form of an edge's payload: plain integers, so it can live in
/// a mapped file. `kind`/`confidence` are enum discriminants (validated on
/// load), `inherit` is 0/1.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RawEdge {
    kind: u8,
    confidence: u8,
    inherit: u8,
    _pad: u8,
    line: u32,
    /// Index into `Graph::details`, or `u32::MAX` for none.
    detail: u32,
    /// Interned `Edge::from_scope` / `Edge::to_scope`, or `u32::MAX` for none.
    from_scope: u32,
    to_scope: u32,
}

// SAFETY: integers only, no padding (4 x u8 + 4 x u32, align 4).
unsafe impl Pod for RawEdge {}
const _: () = assert!(std::mem::size_of::<RawEdge>() == 20);

impl RawEdge {
    fn new(e: &Edge, detail: u32, from_scope: u32, to_scope: u32, inherit: bool) -> Self {
        Self { kind: e.kind as u8, confidence: e.confidence as u8, inherit: inherit as u8, _pad: 0, line: e.line, detail, from_scope, to_scope }
    }
    #[inline]
    fn kind(&self) -> EdgeKind {
        KINDS[self.kind as usize]
    }
    #[inline]
    fn confidence(&self) -> Confidence {
        CONFIDENCES[self.confidence as usize]
    }
    #[inline]
    fn inherit(&self) -> bool {
        self.inherit != 0
    }
    fn data(&self) -> EdgeData {
        EdgeData { kind: self.kind(), confidence: self.confidence(), line: self.line, inherit: self.inherit() }
    }
}

#[derive(Debug, Default, Clone)]
struct Csr {
    /// `offsets[n]..offsets[n+1]` indexes `targets`/`edge_ids` for node `n`.
    offsets: Slab<u32>,
    targets: Slab<NodeId>,
    edge_ids: Slab<u32>,
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
        Self { offsets: offsets.into(), targets: targets.into(), edge_ids: edge_ids.into() }
    }

    #[inline]
    fn neighbours(&self, n: NodeId) -> impl Iterator<Item = (NodeId, u32)> + '_ {
        let (a, b) = (self.offsets[n as usize] as usize, self.offsets[n as usize + 1] as usize);
        self.targets[a..b].iter().copied().zip(self.edge_ids[a..b].iter().copied())
    }

    fn degree(&self, n: NodeId) -> u32 {
        self.offsets[n as usize + 1] - self.offsets[n as usize]
    }

    /// Checks what `neighbours` relies on for `n` nodes and `m` edges.
    fn check(&self, n: usize, m: usize) -> Result<(), String> {
        let o = &self.offsets;
        if o.len() != n + 1 || o[0] != 0 || o[n] as usize != m || self.targets.len() != m || self.edge_ids.len() != m {
            return Err("adjacency arrays have the wrong size".into());
        }
        if o.windows(2).any(|w| w[0] > w[1]) || self.targets.iter().any(|&t| t as usize >= n) || self.edge_ids.iter().any(|&e| e as usize >= m) {
            return Err("adjacency out of range".into());
        }
        Ok(())
    }
}

/// Per-node method facts: owned tables, or the stored form (see
/// [`Graph::save`]): `index[n]` is the start of node `n`'s record in
/// `words`, or `u32::MAX` for unknown; a record is
/// `[flags, pairs, defs, implicit, self_dynamic, pair words.., defs.., implicit.., self_dynamic..]`.
#[derive(Debug, Clone)]
enum MentionStore {
    Owned(Vec<Option<Arc<FileMentions>>>),
    Stored { index: Slab<u32>, words: Slab<u32> },
}

impl Default for MentionStore {
    fn default() -> Self {
        Self::Owned(Vec::new())
    }
}

const M_OPAQUE: u32 = 1;
const M_CONTROLLER: u32 = 2;

impl MentionStore {
    fn get(&self, node: NodeId) -> Option<Mentions<'_>> {
        match self {
            Self::Owned(v) => v.get(node as usize).and_then(|m| m.as_deref()).map(FileMentions::view),
            Self::Stored { index, words } => {
                let at = *index.get(node as usize)?;
                (at != u32::MAX).then(|| record(words, at as usize))
            }
        }
    }

    fn encode(&self, n: usize) -> (Vec<u32>, Vec<u32>) {
        let mut index = Vec::with_capacity(n);
        let mut words = Vec::new();
        for node in 0..n as NodeId {
            match self.get(node) {
                None => index.push(u32::MAX),
                Some(m) => {
                    index.push(words.len() as u32);
                    words.push(u32::from(m.opaque) * M_OPAQUE | u32::from(m.controller) * M_CONTROLLER);
                    words.extend([m.pairs.len() as u32, m.defs.len() as u32, m.implicit.len() as u32, m.self_dynamic.len() as u32]);
                    for &(a, b) in m.pairs {
                        words.extend([a, b]);
                    }
                    words.extend_from_slice(m.defs);
                    words.extend_from_slice(m.implicit);
                    words.extend_from_slice(m.self_dynamic);
                }
            }
        }
        (index, words)
    }

    /// Checks every record lies inside `words` (what `record` relies on).
    fn check(index: &[u32], words: &[u32], n: usize) -> Result<(), String> {
        if index.len() != n {
            return Err("mention index has the wrong size".into());
        }
        for &at in index {
            if at == u32::MAX {
                continue;
            }
            let at = at as usize;
            let head = words.get(at..at + 5).ok_or("mention record out of range")?;
            let len = 5 + 2 * head[1] as usize + head[2] as usize + head[3] as usize + head[4] as usize;
            if at + len > words.len() {
                return Err("mention record out of range".into());
            }
        }
        Ok(())
    }
}

/// `(u32, u32)` is laid out as two consecutive `u32`s (checked at compile
/// time), so stored pair words can be viewed as tuples.
const _: () = assert!(
    std::mem::size_of::<(u32, u32)>() == 8
        && std::mem::align_of::<(u32, u32)>() == 4
        && std::mem::offset_of!((u32, u32), 0) == 0
        && std::mem::offset_of!((u32, u32), 1) == 4
);

fn record(words: &[u32], at: usize) -> Mentions<'_> {
    let h = &words[at..at + 5];
    let (np, nd, ni, ns) = (h[1] as usize, h[2] as usize, h[3] as usize, h[4] as usize);
    let mut o = at + 5;
    let pw = &words[o..o + 2 * np];
    // SAFETY: `(u32, u32)` is two consecutive u32s (asserted above), and
    // `pw` holds `2 * np` of them.
    let pairs: &[(u32, u32)] = unsafe { std::slice::from_raw_parts(pw.as_ptr() as *const (u32, u32), np) };
    o += 2 * np;
    let defs = &words[o..o + nd];
    o += nd;
    let implicit = &words[o..o + ni];
    o += ni;
    let self_dynamic = &words[o..o + ns];
    Mentions { pairs, defs, implicit, self_dynamic, opaque: h[0] & M_OPAQUE != 0, controller: h[0] & M_CONTROLLER != 0 }
}

#[derive(Debug, Default)]
pub struct Graph {
    /// Sorted (node ids follow path order), so [`Graph::id`] is a binary
    /// search and no path is stored twice. Shared with patched graphs.
    paths: Arc<StrTable>,
    /// (from, to) per edge id.
    endpoints: Slab<[NodeId; 2]>,
    data: Slab<RawEdge>,
    /// Distinct edge details.
    details: StrTable,
    scopes: Arc<StrTable>,
    /// [`name_id`] of each scope, to match `to_scope`s against method names.
    scope_ids: Slab<u32>,
    /// Number of distinct other files depending on each node / each node
    /// depends on (edges can repeat per scope).
    distinct_in: Slab<u32>,
    distinct_out: Slab<u32>,
    /// from -> to ("depends on").
    forward: Csr,
    /// to -> from ("is depended on by").
    reverse: Csr,
    /// Per node, the method names it defines and mentions (see
    /// [`Graph::set_mentions`]); `None` = unknown.
    mentions: MentionStore,
    unmentioned: Slab<u32>,
    /// Size/complexity tables (see [`Graph::set_metrics`]), shared with the
    /// extractors' outputs.
    metrics: Vec<Arc<MetricsTable>>,
    /// Details appended by [`Graph::patched`] since the last build (some
    /// may be unused); bounds the garbage in `details`.
    details_appended: u32,
}

/// Per-row dedup key, as in [`Graph::build`]: (from, to, kind, scopes, best
/// confidence first, line, edge index, inherits).
type Key = (NodeId, NodeId, EdgeKind, (u32, u32), std::cmp::Reverse<Confidence>, u32, u32, bool);

/// Sorts `keyed`, keeps the best duplicate (it inherits if any duplicate
/// does) and drops scoped twins that an unscoped edge covers. Both only
/// relate keys with the same `from`, so this can run per row.
fn dedup_keys(keyed: &mut Vec<Key>) {
    keyed.sort_unstable();
    keyed.dedup_by(|b, a| {
        let same = (a.0, a.1, a.2, a.3) == (b.0, b.1, b.2, b.3);
        if same {
            a.7 |= b.7;
        }
        same
    });
    // An edge with no scope on one side already covers every scope on that
    // side ("the whole file depends" / "depends on the whole file"), so a
    // more specific twin of the same from/to/kind adds nothing: drop it.
    // (An inheritance edge passes the affected scope on instead, so it
    // covers nothing.)
    let present: HashSet<(NodeId, NodeId, EdgeKind, u32, u32)> = keyed.iter().filter(|k| !k.7).map(|k| (k.0, k.1, k.2, k.3 .0, k.3 .1)).collect();
    const ANY: u32 = u32::MAX;
    keyed.retain(|k| {
        let (fs, ts) = k.3;
        !((fs != ANY && present.contains(&(k.0, k.1, k.2, ANY, ts)))
            || (ts != ANY && present.contains(&(k.0, k.1, k.2, fs, ANY)))
            || (fs != ANY && ts != ANY && present.contains(&(k.0, k.1, k.2, ANY, ANY))))
    });
}

/// `distinct_in` / `distinct_out` from endpoints sorted by (from, to).
fn distinct(n: usize, endpoints: &[[NodeId; 2]]) -> (Slab<u32>, Slab<u32>) {
    let mut distinct_in = vec![0u32; n];
    let mut distinct_out = vec![0u32; n];
    let mut last: Option<[NodeId; 2]> = None;
    // Duplicates are adjacent.
    for &[f, t] in endpoints {
        if f != t && last != Some([f, t]) {
            distinct_in[t as usize] += 1;
            distinct_out[f as usize] += 1;
        }
        last = Some([f, t]);
    }
    (distinct_in.into(), distinct_out.into())
}

/// Scope strings in the order [`Graph::build`] interns them: first
/// appearance in input order, `from_scope` before `to_scope`, inheritance
/// edges and self-edges skipped.
fn scope_order<'a>(edges: impl Iterator<Item = &'a Edge>) -> Vec<&'a str> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out = Vec::new();
    for e in edges {
        if e.inherit || e.from == e.to {
            continue;
        }
        for s in [&e.from_scope, &e.to_scope].into_iter().flatten() {
            if seen.insert(s.as_str()) {
                out.push(s.as_str());
            }
        }
    }
    out
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
    /// `u32::MAX` = whole file; see `impact_scoped` for the other values.
    scope: u32,
    parent: u32,
    edge: u32,
    depth: u32,
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
                out.push((act.node, act.edge));
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

/// Edge kinds through which a file uses another file's methods by name, so
/// the mention filter applies to them. `RubyRequire` is deliberately absent:
/// a file that only requires another (an entry point registering subcommands,
/// `config/application.rb`) runs it without naming its methods.
fn is_reference(kind: EdgeKind) -> bool {
    matches!(kind, EdgeKind::RubyConstant | EdgeKind::RailsInferred | EdgeKind::RailsView)
}

impl Graph {
    /// Builds the graph. Duplicate `(from, to, kind, scopes)` edges are collapsed,
    /// keeping the highest confidence (then lowest line); the result inherits
    /// if any duplicate does (an inheritance edge has no scopes). Self-edges
    /// are dropped. `extra_nodes` lets files with no edges still be addressable.
    pub fn build<'a>(edges: impl IntoIterator<Item = &'a Edge>, extra_nodes: impl IntoIterator<Item = &'a str>) -> Self {
        // Intern paths first so dedup and ordering work on integers, not strings:
        // this runs on every index generation and dominates update latency.
        let edges: Vec<&Edge> = edges.into_iter().filter(|e| e.from != e.to).collect();
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
        let paths = Arc::new(StrTable::from_strs(order.iter().map(|&i| names[i as usize])));

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
        let scoped: Vec<(u32, u32)> = edges.iter().map(|e| if e.inherit { (u32::MAX, u32::MAX) } else { (scope(&e.from_scope), scope(&e.to_scope)) }).collect();
        // (from, to, kind, scopes, best confidence first, line, edge index, inherits)
        let mut keyed: Vec<Key> = raw
            .iter()
            .enumerate()
            .map(|(i, &(f, t))| (remap[f as usize], remap[t as usize], edges[i].kind, scoped[i], std::cmp::Reverse(edges[i].confidence), edges[i].line, i as u32, edges[i].inherit))
            .collect();
        dedup_keys(&mut keyed);

        let mut detail_ids: HashMap<&str, u32> = HashMap::new();
        let mut detail_list: Vec<&str> = Vec::new();
        let mut endpoints = Vec::with_capacity(keyed.len());
        let mut data = Vec::with_capacity(keyed.len());
        for k in &keyed {
            let e = edges[k.6 as usize];
            endpoints.push([k.0, k.1]);
            let detail = match &e.detail {
                Some(d) => *detail_ids.entry(d.as_str()).or_insert_with(|| {
                    detail_list.push(d);
                    (detail_list.len() - 1) as u32
                }),
                None => u32::MAX,
            };
            data.push(RawEdge::new(e, detail, k.3 .0, k.3 .1, k.7));
        }
        let n = paths.len();
        let forward = Csr::build(n, endpoints.iter().map(|&[f, t]| (f, t)));
        let reverse = Csr::build(n, endpoints.iter().map(|&[f, t]| (t, f)));
        // `endpoints` is sorted by (from, to, ..).
        let (distinct_in, distinct_out) = distinct(n, &endpoints);
        drop(scope);
        let scope_ids = scopes.iter().map(|s| name_id(s)).collect();
        let scopes = Arc::new(StrTable::from_strs(scopes.iter().map(String::as_str)));
        let mentions = MentionStore::Owned(vec![None; n]);
        let details = StrTable::from_strs(detail_list);
        drop(detail_ids);
        Self {
            paths,
            endpoints: endpoints.into(),
            data: data.into(),
            details,
            scopes,
            scope_ids,
            distinct_in,
            distinct_out,
            forward,
            reverse,
            mentions,
            unmentioned: Slab::default(),
            metrics: Vec::new(),
            details_appended: 0,
        }
    }

    /// The graph [`Graph::build`] returns for `all` (the complete new input,
    /// in the same order) with the same `extra_nodes`, computed from `self`
    /// when only the out-edges of the files in `rows` changed: each row is
    /// `(from, every edge of all with that from, in input order)`. Edge ids,
    /// scope ids, adjacency order and counts equal a build's; detail ids may
    /// differ (they are not observable). Mentions and metrics are not
    /// carried over.
    ///
    /// `None` when the result would have a different node set (an endpoint
    /// that is not a node, or a node no longer touched by any edge for
    /// which `listed` is false), a different scope table, or too many
    /// appended details: the caller then builds.
    pub fn patched<'a>(&self, rows: &[(&'a str, Vec<&'a Edge>)], all: impl Iterator<Item = &'a Edge>, listed: impl Fn(&str) -> bool) -> Option<Graph> {
        // Scope ids depend on first appearance anywhere in the input.
        let order = scope_order(all);
        if order.len() != self.scopes.len() || order.iter().zip(self.scopes.iter()).any(|(a, b)| *a != b) {
            return None;
        }
        let scope_ix: HashMap<&str, u32> = self.scopes.iter().enumerate().map(|(i, s)| (s, i as u32)).collect();
        let scope = |s: &Option<String>| s.as_deref().map_or(Some(u32::MAX), |s| scope_ix.get(s).copied());

        let old_details = self.details.len() as u32;
        let mut appended: Vec<&str> = Vec::new();
        let mut new_details: HashMap<&str, u32> = HashMap::new();
        let mut rows_out: Vec<(NodeId, Vec<[NodeId; 2]>, Vec<RawEdge>)> = Vec::with_capacity(rows.len());
        // Nodes that may have lost their last edge.
        let mut touched: Vec<NodeId> = Vec::new();
        for (from, edges) in rows {
            let Some(f) = self.id(from) else {
                // Not a node and no edges now: nothing to patch.
                if edges.iter().all(|e| e.from == e.to) {
                    continue;
                }
                return None;
            };
            touched.push(f);
            touched.extend(self.forward.neighbours(f).map(|(t, _)| t));
            let edges: Vec<&Edge> = edges.iter().copied().filter(|e| e.from != e.to).collect();
            let mut keyed: Vec<Key> = Vec::with_capacity(edges.len());
            for (i, e) in edges.iter().enumerate() {
                debug_assert_eq!(e.from, *from);
                let t = self.id(&e.to)?;
                let sc = if e.inherit { (u32::MAX, u32::MAX) } else { (scope(&e.from_scope)?, scope(&e.to_scope)?) };
                keyed.push((f, t, e.kind, sc, std::cmp::Reverse(e.confidence), e.line, i as u32, e.inherit));
            }
            dedup_keys(&mut keyed);
            let mut ends = Vec::with_capacity(keyed.len());
            let mut data = Vec::with_capacity(keyed.len());
            for k in &keyed {
                let e = edges[k.6 as usize];
                ends.push([k.0, k.1]);
                let detail = match &e.detail {
                    Some(d) => *new_details.entry(d.as_str()).or_insert_with(|| {
                        appended.push(d);
                        old_details + (appended.len() - 1) as u32
                    }),
                    None => u32::MAX,
                };
                data.push(RawEdge::new(e, detail, k.3 .0, k.3 .1, k.7));
            }
            rows_out.push((f, ends, data));
        }
        let details_appended = self.details_appended + new_details.len() as u32;
        if details_appended as usize > (self.details.len() / 4).max(1 << 12) {
            return None;
        }
        rows_out.sort_unstable_by_key(|r| r.0);
        if rows_out.windows(2).any(|w| w[0].0 == w[1].0) {
            return None;
        }

        // Splice the rows into the from-major edge arrays.
        let n = self.paths.len();
        let mut endpoints = Vec::with_capacity(self.endpoints.len() + 64);
        let mut data = Vec::with_capacity(self.data.len() + 64);
        let mut at = 0usize;
        for (f, ends, d) in rows_out {
            let (a, b) = (self.forward.offsets[f as usize] as usize, self.forward.offsets[f as usize + 1] as usize);
            endpoints.extend_from_slice(&self.endpoints[at..a]);
            data.extend_from_slice(&self.data[at..a]);
            endpoints.extend(ends);
            data.extend(d);
            at = b;
        }
        endpoints.extend_from_slice(&self.endpoints[at..]);
        data.extend_from_slice(&self.data[at..]);

        let forward = Csr::build(n, endpoints.iter().map(|&[f, t]| (f, t)));
        let reverse = Csr::build(n, endpoints.iter().map(|&[f, t]| (t, f)));
        for t in touched {
            if forward.degree(t) == 0 && reverse.degree(t) == 0 && !listed(self.paths.get(t as usize)) {
                return None;
            }
        }
        let (distinct_in, distinct_out) = distinct(n, &endpoints);
        let details = if appended.is_empty() { self.details.clone() } else { StrTable::from_strs(self.details.iter().chain(appended)) };
        Some(Self {
            paths: self.paths.clone(),
            endpoints: endpoints.into(),
            data: data.into(),
            details,
            scopes: self.scopes.clone(),
            scope_ids: self.scope_ids.clone(),
            distinct_in,
            distinct_out,
            forward,
            reverse,
            mentions: MentionStore::Owned(vec![None; n]),
            unmentioned: Slab::default(),
            metrics: Vec::new(),
            details_appended,
        })
    }

    /// Saves the graph, its mentions and its metrics tables as sections
    /// `base..base + 100` (see [`crate::store`]).
    pub fn save(&self, w: &mut SectionWriter, base: u32) {
        self.paths.save(w, base, base + 1);
        w.put(base + 2, &self.endpoints);
        w.put(base + 3, &self.data);
        self.details.save(w, base + 4, base + 5);
        self.scopes.save(w, base + 6, base + 7);
        w.put(base + 8, &self.scope_ids);
        w.put(base + 9, &self.distinct_in);
        w.put(base + 10, &self.distinct_out);
        for (i, c) in [&self.forward, &self.reverse].into_iter().enumerate() {
            let b = base + 11 + 3 * i as u32;
            w.put(b, &c.offsets);
            w.put(b + 1, &c.targets);
            w.put(b + 2, &c.edge_ids);
        }
        match &self.mentions {
            MentionStore::Stored { index, words } => {
                w.put(base + 17, index);
                w.put(base + 18, words);
            }
            m @ MentionStore::Owned(_) => {
                let (index, words) = m.encode(self.node_count());
                w.put(base + 17, &index);
                w.put(base + 18, &words);
            }
        }
        w.put(base + 19, &self.unmentioned);
        w.put::<u32>(base + 20, &[self.metrics.len() as u32, self.details_appended]);
        for (i, t) in self.metrics.iter().enumerate() {
            t.save(w, base + 30 + 4 * i as u32);
        }
    }

    /// A graph saved by [`Graph::save`], its arrays pointing into the
    /// mapped file. Checks everything the accessors index with (so a
    /// malformed file is an error, never a crash or worse).
    pub fn load(f: &SectionFile, base: u32) -> Result<Self, String> {
        let paths = StrTable::load(f, base, base + 1)?;
        let n = paths.len();
        if (1..n).any(|i| paths.get(i - 1) >= paths.get(i)) {
            return Err("graph paths are not sorted".into());
        }
        let endpoints: Slab<[NodeId; 2]> = f.slab(base + 2)?;
        let data: Slab<RawEdge> = f.slab(base + 3)?;
        let details = StrTable::load(f, base + 4, base + 5)?;
        let scopes = StrTable::load(f, base + 6, base + 7)?;
        let m = endpoints.len();
        if data.len() != m || endpoints.iter().any(|&[a, b]| a as usize >= n || b as usize >= n) {
            return Err("graph edges out of range".into());
        }
        let (nd, ns) = (details.len() as u32, scopes.len() as u32);
        let ok = |i: u32, max: u32| i == u32::MAX || i < max;
        if data.iter().any(|d| {
            d.kind as usize >= KINDS.len() || d.confidence as usize >= CONFIDENCES.len() || d.inherit > 1 || !ok(d.detail, nd) || !ok(d.from_scope, ns) || !ok(d.to_scope, ns)
        }) {
            return Err("graph edge data out of range".into());
        }
        let scope_ids: Slab<u32> = f.slab(base + 8)?;
        let distinct_in: Slab<u32> = f.slab(base + 9)?;
        let distinct_out: Slab<u32> = f.slab(base + 10)?;
        if scope_ids.len() != scopes.len() || distinct_in.len() != n || distinct_out.len() != n {
            return Err("graph arrays have the wrong size".into());
        }
        let csr = |b: u32| -> Result<Csr, String> {
            let c = Csr { offsets: f.slab(b)?, targets: f.slab(b + 1)?, edge_ids: f.slab(b + 2)? };
            c.check(n, m)?;
            Ok(c)
        };
        let (forward, reverse) = (csr(base + 11)?, csr(base + 14)?);
        let (index, words): (Slab<u32>, Slab<u32>) = (f.slab(base + 17)?, f.slab(base + 18)?);
        MentionStore::check(&index, &words, n)?;
        let unmentioned: Slab<u32> = f.slab(base + 19)?;
        let meta: Slab<u32> = f.slab(base + 20)?;
        let [tables, details_appended] = <[u32; 2]>::try_from(&*meta).map_err(|_| "graph metadata is malformed")?;
        let metrics = (0..tables).map(|i| MetricsTable::load(f, base + 30 + 4 * i).map(Arc::new)).collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            paths: Arc::new(paths),
            endpoints,
            data,
            details,
            scopes: Arc::new(scopes),
            scope_ids,
            distinct_in,
            distinct_out,
            forward,
            reverse,
            mentions: MentionStore::Stored { index, words },
            unmentioned,
            metrics,
            details_appended,
        })
    }

    /// Heap bytes held by the graph's own arrays (0 for a loaded graph;
    /// mentions shared with extractors are not counted).
    pub fn heap_bytes(&self) -> usize {
        let csr = |c: &Csr| c.offsets.heap_bytes() + c.targets.heap_bytes() + c.edge_ids.heap_bytes();
        self.paths.heap_bytes()
            + self.endpoints.heap_bytes()
            + self.data.heap_bytes()
            + self.details.heap_bytes()
            + self.scopes.heap_bytes()
            + self.scope_ids.heap_bytes()
            + self.distinct_in.heap_bytes()
            + self.distinct_out.heap_bytes()
            + csr(&self.forward)
            + csr(&self.reverse)
            + self.unmentioned.heap_bytes()
            + match &self.mentions {
                MentionStore::Owned(v) => v.len() * 8,
                MentionStore::Stored { index, words } => index.heap_bytes() + words.heap_bytes(),
            }
    }

    /// Every edge in id order as an [`Edge`] (detail resolved), for
    /// comparing graphs in tests.
    pub fn edges_for_test(&self) -> Vec<Edge> {
        (0..self.edge_count() as u32)
            .map(|i| {
                let (f, t, d) = self.edge(i);
                let mut e = Edge::new(self.path(f), self.path(t), d.kind, d.confidence).at(d.line);
                e.detail = self.edge_detail(i).map(String::from);
                let (fs, ts) = self.edge_scopes(i);
                e.from_scope = fs.map(String::from);
                e.to_scope = ts.map(String::from);
                e.inherit = d.inherit;
                e
            })
            .collect()
    }

    /// (forward, reverse) adjacency as (node, [(neighbour, edge id)]) for
    /// comparing graphs in tests.
    pub fn adjacency_for_test(&self) -> (Vec<Vec<(NodeId, u32)>>, Vec<Vec<(NodeId, u32)>>) {
        let n = self.node_count() as NodeId;
        ((0..n).map(|i| self.forward.neighbours(i).collect()).collect(), (0..n).map(|i| self.reverse.neighbours(i).collect()).collect())
    }

    /// Attaches the per-file method mentions that let method-level queries
    /// narrow a seed's direct dependents (see [`Graph::impact_scoped`]).
    /// Files of the table that are not nodes are ignored.
    pub fn set_mentions(&mut self, table: &MentionTable) {
        let mut v = vec![None; self.node_count()];
        for (path, m) in &table.files {
            if let Some(n) = self.id(path) {
                v[n as usize] = Some(m.clone());
            }
        }
        self.mentions = MentionStore::Owned(v);
        self.unmentioned = table.unmentioned.to_vec().into();
    }

    /// Attaches the extractors' size/complexity tables (replacing any
    /// attached before). The tables are shared, not copied; a lookup hashes
    /// the node's path and binary-searches each table.
    pub fn set_metrics(&mut self, tables: impl IntoIterator<Item = Arc<MetricsTable>>) {
        self.metrics = tables.into_iter().collect();
    }

    /// Size and complexity of `node`'s file; `None` when no extractor
    /// analysed it (not code, ignored, or Ruby with `ruby.method_level` off).
    pub fn metrics(&self, node: NodeId) -> Option<FileMetrics> {
        ((node as usize) < self.node_count()).then(|| self.metrics_of(self.path(node)))?
    }

    /// [`Graph::metrics`] by path (the file need not be a node).
    pub fn metrics_of(&self, path: &str) -> Option<FileMetrics> {
        crate::metrics::lookup(&self.metrics, path).map(|(f, _)| f)
    }

    /// Per-method metrics of `node`'s file, sorted by start line; empty when
    /// unknown (JS/TS, templates, and files without methods).
    pub fn method_metrics(&self, node: NodeId) -> &[MethodMetric] {
        if node as usize >= self.node_count() {
            return &[];
        }
        crate::metrics::lookup(&self.metrics, self.path(node)).map_or(&[], |(_, m)| m)
    }

    /// The method of `node`'s file whose lines contain `line` (1-based), e.g.
    /// the `start` of a [`crate::model::ScopeRange`].
    pub fn method_at(&self, node: NodeId, line: u32) -> Option<MethodMetric> {
        let ms = self.method_metrics(node);
        let i = ms.partition_point(|m| m.start <= line);
        ms[..i].last().copied().filter(|m| line <= m.end())
    }

    /// The attached size/complexity tables.
    pub fn metrics_tables(&self) -> &[Arc<MetricsTable>] {
        &self.metrics
    }

    /// The method facts of `node`, if known.
    pub fn mentions(&self, node: NodeId) -> Option<Mentions<'_>> {
        self.mentions.get(node)
    }

    pub fn node_count(&self) -> usize {
        self.paths.len()
    }
    pub fn edge_count(&self) -> usize {
        self.endpoints.len()
    }
    pub fn id(&self, path: &str) -> Option<NodeId> {
        self.paths.binary_search(path).ok().map(|i| i as NodeId)
    }
    pub fn path(&self, node: NodeId) -> &str {
        self.paths.get(node as usize)
    }
    /// Every node's path, in node id order (sorted).
    pub fn paths(&self) -> impl ExactSizeIterator<Item = &str> + Clone + '_ {
        self.paths.iter()
    }
    pub fn edge(&self, edge: u32) -> (NodeId, NodeId, EdgeData) {
        let [f, t] = self.endpoints[edge as usize];
        (f, t, self.data[edge as usize].data())
    }
    pub fn edge_detail(&self, edge: u32) -> Option<&str> {
        match self.data[edge as usize].detail {
            u32::MAX => None,
            i => Some(self.details.get(i as usize)),
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
        let get = |i: u32| (i != u32::MAX).then(|| self.scopes.get(i as usize));
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
        self.data[edge as usize].inherit()
    }

    /// Everything that may be affected if `seeds` change entirely: reverse BFS.
    pub fn impact(&self, seeds: &[NodeId], opts: QueryOpts) -> Impact {
        let seeds: Vec<(NodeId, Option<Vec<String>>)> = seeds.iter().map(|&s| (s, None)).collect();
        self.impact_scoped(&seeds, opts)
    }

    /// Everything that may be affected by a change to `seeds`, where a seed
    /// may be limited to some of its scopes (`Some(["refund!", "charge!"])` =
    /// only those methods / controller actions changed; `None` = the whole
    /// file changed).
    ///
    /// Each reached file carries the part of it that is affected: all of it,
    /// one named scope (a controller action), or a seed's name set. An edge is
    /// followed from `to` when it has no `to_scope`, or `to` is wholly
    /// affected, or its `to_scope` is the affected scope (one of the names, for
    /// a name set); the dependent then becomes affected in the edge's
    /// `from_scope` (whole file if none). Inheritance edges pass the affected
    /// part on unchanged.
    ///
    /// **Mention filter.** For a seed limited to names `M` whose method facts
    /// are known (see [`Graph::set_mentions`]), `M` first grows to a fixed
    /// point with every method of the seed and of its ancestors (inheritance
    /// edges, transitively) that mentions a name in `M` or dispatches on
    /// `self` by a computed name. The seed counts as wholly changed when a
    /// class body there mentions a name in `M` (callback, validation,
    /// `delegate`, DSL symbol) or dispatches on `self` by a computed name, or
    /// when a name in `M` is a method that is invoked implicitly or never
    /// mentioned anywhere. Otherwise a direct dependent reached through a
    /// Ruby reference edge (no `to_scope`, not inheritance) is kept only if it
    /// mentions a name in `M` or its mentions are unknown. Beyond that first
    /// hop, everything is file-level (or action-level) as above.
    pub fn impact_scoped(&self, seeds: &[(NodeId, Option<Vec<String>>)], opts: QueryOpts) -> Impact {
        const ALL: u32 = u32::MAX;
        /// "Nothing another file can depend on changed": leads nowhere.
        const NOTHING: u32 = u32::MAX - 1;
        /// `SET + 2k + f`: the names `sets[k]` of a seed; `f` = 1 when the
        /// mention filter applies (the seed itself), 0 when it does not
        /// (heirs, seeds without method facts).
        const SET: u32 = 1 << 31;
        let set_of = |scope: u32| -> Option<(usize, bool)> { (SET..NOTHING).contains(&scope).then(|| (((scope - SET) >> 1) as usize, (scope - SET) & 1 == 1)) };
        let mut position = vec![u32::MAX; self.node_count()];
        let mut whole = vec![false; self.node_count()];
        let mut partial: HashSet<(NodeId, u32)> = HashSet::new();
        let mut found: Vec<(Hit, u32)> = Vec::new();
        let mut acts: Vec<Act> = Vec::new();
        let mut queue: VecDeque<u32> = VecDeque::new();
        let mut sets: Vec<Vec<u32>> = Vec::new();

        // Merge per node: `None` (whole file) wins over any names.
        let mut merged: std::collections::BTreeMap<NodeId, Option<Vec<&str>>> = std::collections::BTreeMap::new();
        for (node, scopes) in seeds {
            let names = scopes.as_ref().filter(|_| !opts.file_level).filter(|v| !v.iter().any(|n| n == crate::model::SCOPE_BODY));
            match (merged.entry(*node).or_insert_with(|| Some(Vec::new())), names) {
                (slot, None) => *slot = None,
                (Some(have), Some(names)) => have.extend(names.iter().map(String::as_str)),
                (None, Some(_)) => {}
            }
        }
        for (node, names) in merged {
            position[node as usize] = 0;
            found.push((Hit { node, depth: 0, via: node, edge: u32::MAX }, acts.len() as u32));
            let scope = match names {
                None => ALL,
                Some(v) if v.is_empty() => NOTHING,
                Some(v) => {
                    let mut ids: Vec<u32> = v.iter().map(|n| name_id(n)).collect();
                    ids.sort_unstable();
                    ids.dedup();
                    match self.mentions(node).map(|_| self.grow(node, ids.clone())) {
                        Some(None) => ALL,
                        Some(Some(grown)) => {
                            sets.push(grown);
                            SET + 2 * (sets.len() as u32 - 1) + 1
                        }
                        None => {
                            sets.push(ids);
                            SET + 2 * (sets.len() as u32 - 1)
                        }
                    }
                }
            };
            if scope == ALL {
                whole[node as usize] = true;
            } else {
                partial.insert((node, scope));
            }
            queue.push_back(acts.len() as u32);
            acts.push(Act { node, scope, parent: u32::MAX, edge: u32::MAX, depth: 0 });
        }

        while let Some(i) = queue.pop_front() {
            let cur = acts[i as usize];
            if cur.scope == NOTHING {
                continue;
            }
            let set = set_of(cur.scope);
            for (next, edge) in self.reverse.neighbours(cur.node) {
                let d = &self.data[edge as usize];
                if d.confidence() < opts.min_confidence || whole[next as usize] || next == cur.node {
                    continue;
                }
                let scope = if opts.file_level || cur.scope == ALL {
                    if opts.file_level || d.inherit() { ALL } else { d.from_scope }
                } else if d.inherit() {
                    // An heir has the same methods; its own dependents are not filtered.
                    match set {
                        Some((k, _)) => SET + 2 * k as u32,
                        None => cur.scope,
                    }
                } else if d.to_scope != ALL {
                    let hit = match set {
                        Some((k, _)) => sets[k].binary_search(&self.scope_ids[d.to_scope as usize]).is_ok(),
                        None => d.to_scope == cur.scope,
                    };
                    if !hit {
                        continue;
                    }
                    d.from_scope
                } else {
                    if let Some((k, true)) = set
                        && is_reference(d.kind())
                        && !self.mentions(next).is_none_or(|m| m.opaque || m.mentions_any(&sets[k]))
                    {
                        continue;
                    }
                    d.from_scope
                };
                let depth = cur.depth + 1;
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
                acts.push(Act { node: next, scope, parent: i, edge, depth });
                queue.push_back(act);
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

    /// The names a change to methods `names` (sorted [`name_id`]s) of `node`
    /// affects, grown to a fixed point over `node` and its ancestors, or
    /// `None` when the whole file must count as changed (see
    /// [`Graph::impact_scoped`]). An ancestor without method facts makes it
    /// whole-file.
    fn grow(&self, node: NodeId, mut names: Vec<u32>) -> Option<Vec<u32>> {
        // The seed and its ancestors.
        let mut files: Vec<NodeId> = vec![node];
        let mut i = 0;
        while i < files.len() {
            for (to, e) in self.forward.neighbours(files[i]) {
                if self.data[e as usize].inherit() && !files.contains(&to) {
                    files.push(to);
                }
            }
            i += 1;
        }
        let tables: Vec<Mentions<'_>> = files.iter().map(|&f| self.mentions(f)).collect::<Option<_>>()?;
        if tables.iter().any(|t| t.self_dynamic.last() == Some(&NO_DEF)) {
            return None;
        }
        loop {
            let mut add: Vec<u32> = Vec::new();
            for t in &tables {
                for &(name, def) in t.pairs.iter() {
                    if names.binary_search(&name).is_ok() {
                        if def == NO_DEF {
                            return None;
                        }
                        add.push(t.defs[def as usize]);
                    }
                }
                add.extend(t.self_dynamic.iter().map(|&d| t.defs[d as usize]));
            }
            add.retain(|n| names.binary_search(n).is_err());
            if add.is_empty() {
                break;
            }
            names.extend(add);
            names.sort_unstable();
            names.dedup();
        }
        for t in &tables {
            for (i, d) in t.defs.iter().enumerate() {
                if names.binary_search(d).is_ok()
                    && (t.implicit.binary_search(&(i as u32)).is_ok() || (!t.controller && self.unmentioned.binary_search(d).is_ok()))
                {
                    return None;
                }
            }
        }
        Some(names)
    }

    /// Everything `seeds` rely on: forward BFS. Useful as agent context
    /// ("what do I need to read to understand this file").
    pub fn dependencies_closure(&self, seeds: &[NodeId], opts: QueryOpts) -> Impact {
        self.bfs(&self.forward, seeds, opts)
    }

    fn bfs(&self, csr: &Csr, seeds: &[NodeId], opts: QueryOpts) -> Impact {
        let mut position = vec![u32::MAX; self.node_count()];
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
                    if position[next as usize] != u32::MAX || self.data[edge as usize].confidence() < opts.min_confidence {
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

    /// `FileMentions` from (def names, (name, def or "-") pairs).
    fn fm(defs: &[&str], pairs: &[(&str, &str)]) -> FileMentions {
        let mut p: Vec<(u32, u32)> = pairs
            .iter()
            .map(|&(n, d)| (name_id(n), if d == "-" { NO_DEF } else { defs.iter().position(|x| *x == d).unwrap() as u32 }))
            .collect();
        p.sort_unstable();
        let implicit: Box<[u32]> = defs.iter().enumerate().filter(|(_, d)| **d == "to_s").map(|(i, _)| i as u32).collect();
        FileMentions { pairs: p.into(), defs: defs.iter().map(|d| name_id(d)).collect(), implicit, ..Default::default() }
    }

    fn table(files: Vec<(&str, FileMentions)>, unmentioned: &[&str]) -> MentionTable {
        let mut u: Vec<u32> = unmentioned.iter().map(|n| name_id(n)).collect();
        u.sort_unstable();
        MentionTable { files: files.into_iter().map(|(p, m)| (p.to_string(), Arc::new(m))).collect(), unmentioned: u.into() }
    }

    #[test]
    fn mention_filter() {
        // model: refund! calls charge!; `before_save :audit` at class level; to_s
        // (implicit) calls fmt; conv is never mentioned anywhere. It includes
        // concern, whose helper calls model's tax (template method).
        // svc calls refund!, job charge!, report helper; sub < model; sub_user
        // uses sub; caller uses svc; dyn dispatches dynamically; tmpl has no
        // facts; page reaches model through GraphQL.
        let edges = vec![
            Edge::new("svc", "model", RubyConstant, Exact),
            Edge::new("job", "model", RailsInferred, Inferred),
            Edge::new("report", "model", RubyConstant, Exact),
            Edge::new("lazy", "model", RubyRequire, Exact),
            Edge::new("sub", "model", RubyConstant, Exact).inherit(),
            Edge::new("sub_user", "sub", RubyConstant, Exact),
            Edge::new("caller", "svc", RubyConstant, Exact),
            Edge::new("model", "concern", RubyConstant, Exact).inherit(),
            Edge::new("dyn", "model", RubyConstant, Exact),
            Edge::new("tmpl", "model", RubyConstant, Exact),
            Edge::new("page", "model", GraphQl, Inferred),
        ];
        let mut g = Graph::build(&edges, []);
        let mut opaque = fm(&[], &[]);
        opaque.opaque = true;
        g.set_mentions(&table(
            vec![
                ("model", fm(&["refund!", "charge!", "audit", "tax", "to_s", "conv"], &[("charge!", "refund!"), ("audit", "-"), ("fmt", "to_s")])),
                ("concern", fm(&["helper"], &[("tax", "helper")])),
                ("svc", fm(&["run"], &[("refund!", "run")])),
                ("job", fm(&["perform"], &[("charge!", "perform")])),
                ("report", fm(&[], &[("helper", "-"), ("where", "-")])),
                ("lazy", fm(&[], &[])),
                ("sub_user", fm(&[], &[])),
                ("caller", fm(&[], &[])),
                ("dyn", opaque),
                ("nowhere", fm(&[], &[])),
            ],
            &["conv"],
        ));
        let id = |n: &str| g.id(n).unwrap();
        let run = |scopes: Option<&[&str]>, depth: u32| {
            let seed = (id("model"), scopes.map(|s| s.iter().map(|x| x.to_string()).collect()));
            let imp = g.impact_scoped(&[seed], QueryOpts { max_depth: depth, ..Default::default() });
            names(&g, &imp).into_iter().skip(1).map(|(n, d)| format!("{n}:{d}")).collect::<Vec<_>>().join(" ")
        };
        let unfiltered = "dyn:1 lazy:1 page:1 sub:1 tmpl:1";
        // charge! grows to refund! (same file): svc and job; beyond hop 1 file-level.
        assert_eq!(run(Some(&["charge!"]), 1), "dyn:1 job:1 lazy:1 page:1 sub:1 svc:1 tmpl:1");
        assert_eq!(run(Some(&["charge!"]), 0), "dyn:1 job:1 lazy:1 page:1 sub:1 svc:1 tmpl:1 caller:2 sub_user:2");
        assert_eq!(run(Some(&["refund!"]), 1), "dyn:1 lazy:1 page:1 sub:1 svc:1 tmpl:1");
        // tax grows to the concern's helper, which report mentions.
        assert_eq!(run(Some(&["tax"]), 1), "dyn:1 lazy:1 page:1 report:1 sub:1 tmpl:1");
        // Class body (callback), implicit (to_s calls fmt), never mentioned: whole file.
        let whole = "dyn:1 job:1 lazy:1 page:1 report:1 sub:1 svc:1 tmpl:1";
        assert_eq!(run(Some(&["audit"]), 1), whole);
        assert_eq!(run(Some(&["fmt"]), 1), whole);
        assert_eq!(run(Some(&["conv"]), 1), whole);
        assert_eq!(run(None, 1), whole);
        assert_eq!(run(Some(&[crate::model::SCOPE_BODY]), 1), whole);
        // A name nothing mentions reaches only what is not filtered; no change reaches nothing.
        assert_eq!(run(Some(&["zzz"]), 1), unfiltered);
        assert_eq!(run(Some(&[]), 0), "");
        let imp = g.impact_scoped(&[(id("model"), Some(vec!["zzz".into()]))], QueryOpts { file_level: true, ..Default::default() });
        assert_eq!(imp.hits.len(), 11, "file-level mode ignores scopes and mentions");
        // A seed without facts is not filtered.
        let imp = g.impact_scoped(&[(id("sub"), Some(vec!["zzz".into()]))], QueryOpts::default());
        assert_eq!(names(&g, &imp).len(), 2);
        // An ancestor without facts makes the change whole-file.
        let mut g2 = Graph::build(&edges, []);
        g2.set_mentions(&table(vec![("model", fm(&["refund!"], &[])), ("svc", fm(&[], &[]))], &[]));
        let imp = g2.impact_scoped(&[(g2.id("model").unwrap(), Some(vec!["refund!".into()]))], QueryOpts { max_depth: 1, ..Default::default() });
        assert_eq!(names(&g2, &imp).len(), 9);
        assert!(g.dependents(id("model")).all(|(n, _)| n != id("model")));
    }

    #[test]
    fn mention_filter_keeps_action_scopes() {
        // ctrl#index changed. Routes name actions; specs mention them; sub < ctrl.
        let edges = vec![
            Edge::new("routes", "ctrl", RailsRoute, Inferred).to_scope("index"),
            Edge::new("routes2", "ctrl", RailsRoute, Inferred).to_scope("show"),
            Edge::new("spec", "ctrl", RubyConstant, Exact),
            Edge::new("spec2", "ctrl", RubyConstant, Exact),
            Edge::new("sub", "ctrl", RubyConstant, Exact).inherit(),
            Edge::new("routes3", "sub", RailsRoute, Inferred).to_scope("index"),
            Edge::new("routes4", "sub", RailsRoute, Inferred).to_scope("show"),
            Edge::new("sub_spec", "sub", RubyConstant, Exact),
        ];
        let mut g = Graph::build(&edges, []);
        g.set_mentions(&table(
            vec![
                ("ctrl", fm(&["index", "show"], &[])),
                ("spec", fm(&[], &[("index", "-")])),
                ("spec2", fm(&[], &[("show", "-")])),
                ("sub", fm(&[], &[])),
                ("sub_spec", fm(&[], &[])),
            ],
            &[],
        ));
        let imp = g.impact_scoped(&[(g.id("ctrl").unwrap(), Some(vec!["index".into()]))], QueryOpts::default());
        let got: Vec<String> = names(&g, &imp).into_iter().skip(1).map(|(n, d)| format!("{n}:{d}")).collect();
        // The heir passes the action on; its own dependents are not mention-filtered.
        assert_eq!(got, ["routes:1", "spec:1", "sub:1", "routes3:2", "sub_spec:2"]);
        // Inheritance edges merge into a plain duplicate.
        let g = Graph::build(&[Edge::new("a", "b", RubyConstant, Exact), Edge::new("a", "b", RubyConstant, Exact).inherit()], []);
        assert_eq!((g.edge_count(), g.edge_inherits(0)), (1, true));
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

    #[test]
    fn metrics_by_node_path_and_line() {
        let mut g = g();
        let fm = FileMetrics { lines: 30, complexity: 7, methods: 2, max_method: 4 };
        let ms = [MethodMetric { start: 2, span: 3, complexity: 4 }, MethodMetric { start: 10, span: 0, complexity: 2 }];
        let js = FileMetrics { lines: 5, complexity: 1, methods: 0, max_method: 0 };
        g.set_metrics([
            Arc::new(MetricsTable::build([("a", fm, &ms[..])], true)),
            Arc::new(MetricsTable::build([("b", js, &[][..])], false)),
        ]);
        let (a, b, c) = (g.id("a").unwrap(), g.id("b").unwrap(), g.id("c").unwrap());
        assert_eq!(g.metrics(a), Some(fm));
        assert_eq!(g.metrics(b), Some(js));
        assert_eq!(g.metrics(c), None);
        assert_eq!(g.metrics_of("a"), Some(fm));
        assert_eq!(g.method_metrics(a), &ms);
        assert!(g.method_metrics(b).is_empty());
        assert_eq!(g.method_at(a, 1), None);
        assert_eq!(g.method_at(a, 2).map(|m| m.complexity), Some(4));
        assert_eq!(g.method_at(a, 5).map(|m| m.complexity), Some(4));
        assert_eq!(g.method_at(a, 6), None);
        assert_eq!(g.method_at(a, 10).map(|m| m.complexity), Some(2));
    }

    #[test]
    fn kinds_table_matches_discriminants() {
        for (i, k) in KINDS.iter().enumerate() {
            assert_eq!(*k as usize, i);
        }
        for (i, c) in CONFIDENCES.iter().enumerate() {
            assert_eq!(*c as usize, i);
        }
    }

    /// Saves `g` and maps it back.
    pub(crate) fn reload(g: &Graph) -> Graph {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!("ripple-graph-{}-{}.bin", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let mut w = SectionWriter::new();
        g.save(&mut w, 1000);
        w.write_atomic(1, &path).unwrap();
        let f = SectionFile::open(&path, 1).unwrap();
        let _ = std::fs::remove_file(&path);
        let out = Graph::load(&f, 1000).unwrap();
        assert_eq!(out.heap_bytes(), 0);
        out
    }

    /// Everything observable about two graphs is the same, including every
    /// single-seed impact (whole file and per scope / mentioned name).
    pub(crate) fn assert_same(a: &Graph, b: &Graph) {
        assert_eq!(a.paths().collect::<Vec<_>>(), b.paths().collect::<Vec<_>>());
        assert_eq!(a.edges_for_test(), b.edges_for_test());
        assert_eq!(a.adjacency_for_test(), b.adjacency_for_test());
        for n in 0..a.node_count() as NodeId {
            assert_eq!(a.mentions(n), b.mentions(n));
            assert_eq!(a.metrics(n), b.metrics(n));
            assert_eq!(a.method_metrics(n), b.method_metrics(n));
            assert_eq!((a.fan_in(n), a.fan_out(n)), (b.fan_in(n), b.fan_out(n)));
            for (e, f) in a.dependents(n).zip(b.dependents(n)) {
                assert_eq!(e, f);
                assert_eq!(a.edge_detail(e.1), b.edge_detail(f.1));
                assert_eq!(a.edge_scopes(e.1), b.edge_scopes(f.1));
                assert_eq!(format!("{:?}", a.edge(e.1)), format!("{:?}", b.edge(f.1)));
            }
            let mut names: Vec<String> = (0..a.edge_count() as u32).filter_map(|e| a.edge_scopes(e).1.map(String::from)).collect();
            names.extend(["zzz".to_string(), "refund!".into(), "charge!".into(), "tax".into()]);
            let mut seeds: Vec<Option<Vec<String>>> = vec![None];
            seeds.extend(names.into_iter().map(|s| Some(vec![s])));
            for scopes in seeds {
                for opts in [QueryOpts::default(), QueryOpts { max_depth: 1, ..Default::default() }, QueryOpts { file_level: true, ..Default::default() }] {
                    let (x, y) = (a.impact_scoped(&[(n, scopes.clone())], opts), b.impact_scoped(&[(n, scopes.clone())], opts));
                    assert_eq!(format!("{:?}", x.hits), format!("{:?}", y.hits));
                    for h in &x.hits {
                        assert_eq!(x.path(h.node), y.path(h.node));
                    }
                }
            }
            assert_eq!(format!("{:?}", a.dependencies_closure(&[n], QueryOpts::default()).hits), format!("{:?}", b.dependencies_closure(&[n], QueryOpts::default()).hits));
        }
    }

    #[test]
    fn saved_graphs_answer_the_same() {
        let mut g = g();
        assert_same(&g, &reload(&g));
        let fm1 = FileMetrics { lines: 30, complexity: 7, methods: 2, max_method: 4 };
        let ms = [MethodMetric { start: 2, span: 3, complexity: 4 }, MethodMetric { start: 10, span: 0, complexity: 2 }];
        g.set_metrics([
            Arc::new(MetricsTable::build([("a", fm1, &ms[..])], true)),
            Arc::new(MetricsTable::build([("b", FileMetrics::default(), &[][..])], false)),
        ]);
        g.set_mentions(&table(vec![("a", fm(&["x", "to_s"], &[("y", "x"), ("z", "-")])), ("c", fm(&[], &[]))], &["x"]));
        let r = reload(&g);
        assert_same(&g, &r);
        // A reloaded graph saves and patches like the original.
        assert_same(&r, &reload(&r));
        let edges = vec![
            Edge::new("b", "a", RubyConstant, Exact).at(3).detail("A"),
            Edge::new("c", "b", RubyConstant, Exact).detail("new detail"),
            Edge::new("d", "a", ApiCall, Heuristic),
            Edge::new("e", "c", JsImport, Exact),
            Edge::new("b", "a", RubyConstant, Inferred).at(1),
        ];
        let rows = vec![("c", vec![&edges[1]])];
        let p1 = g.patched(&rows, edges.iter(), |_| true).unwrap();
        let p2 = r.patched(&rows, edges.iter(), |_| true).unwrap();
        assert_same(&p1, &p2);
        assert_eq!(p2.edge_detail(p2.dependents(p2.id("b").unwrap()).next().unwrap().1), Some("new detail"));
    }

    #[test]
    fn corrupt_graph_sections_are_rejected() {
        let g = g();
        let path = std::env::temp_dir().join(format!("ripple-graph-bad-{}.bin", std::process::id()));
        // An edge pointing past the node table.
        let mut w = SectionWriter::new();
        g.save(&mut w, 0);
        let mut w2 = SectionWriter::new();
        // Rebuild with a bad endpoints section (sections are looked up by id, the later wins).
        let f = {
            w.write_atomic(1, &path).unwrap();
            SectionFile::open(&path, 1).unwrap()
        };
        for id in 0..60 {
            if id == 2 {
                w2.put::<[u32; 2]>(2, &[[0, 999]; 4]);
            } else if let Ok(b) = f.bytes(id) {
                w2.put_bytes(id, b);
            }
        }
        w2.write_atomic(1, &path).unwrap();
        let f = SectionFile::open(&path, 1).unwrap();
        assert!(Graph::load(&f, 0).unwrap_err().contains("out of range"));
        let _ = std::fs::remove_file(&path);
    }
}
