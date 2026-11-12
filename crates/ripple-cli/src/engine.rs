//! The indexing engine: file listing -> extractors -> linkers -> graph,
//! published as immutable [`Snapshot`]s.
//!
//! The engine is single-owner (`&mut self`) and is driven by one indexer
//! thread in the daemon, or directly by one-shot commands. Readers only ever
//! see `Arc<Snapshot>`s, which are never mutated after publication.

use crate::edgefile::EdgeFileExtractor;
use crate::files::{self, Listing};
use anyhow::Result;
use ripple_core::model::ScopeRange;
use ripple_core::{ChangeSet, Edge, ExtractOutput, Extractor, Facts, Graph, Linker, RelPath, Warning, Workspace};
use ripple_policy::{CONFIG_FILE_NAMES, Policy, PolicyIndex};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Instant, SystemTime};

/// The extractors and linkers an engine runs.
pub struct Pipeline {
    pub extractors: Vec<Box<dyn Extractor>>,
    pub linkers: Vec<Box<dyn Linker>>,
}

impl Pipeline {
    /// The production pipeline. This is the only place the concrete
    /// language extractors and linkers are named.
    pub fn standard() -> Self {
        let mut extractors: Vec<Box<dyn Extractor>> = vec![Box::new(ripple_ruby::RubyExtractor::new()), Box::new(ripple_js::JsExtractor::new())];
        if let Some(e) = EdgeFileExtractor::from_env() {
            extractors.push(Box::new(e));
        }
        let mut linkers: Vec<Box<dyn Linker>> = vec![Box::new(ripple_boundary::BoundaryLinker::new())];
        // Debugging / benchmarking: `RIPPLE_EXTRACTORS=edge_file` runs only the named stages.
        if let Some(only) = std::env::var("RIPPLE_EXTRACTORS").ok().filter(|v| !v.is_empty()) {
            let keep: HashSet<&str> = only.split(',').map(str::trim).collect();
            extractors.retain(|e| keep.contains(e.name()));
            linkers.retain(|l| keep.contains(l.name()));
        }
        Self { extractors, linkers }
    }
}

/// When each path was last examined by an incremental pass (shared by the
/// engine and all its snapshots; written only after a pass is published).
pub type Seen = Arc<RwLock<HashMap<RelPath, SystemTime>>>;

/// One immutable index generation.
pub struct Snapshot {
    pub generation: u64,
    pub root: PathBuf,
    pub graph: Graph,
    pub policy: Arc<Policy>,
    /// Policy classification of every graph node.
    pub index: PolicyIndex,
    /// Extractor/linker/config warnings, sorted.
    pub warnings: Vec<Warning>,
    /// Per extractor/linker counters.
    pub stats: BTreeMap<String, BTreeMap<String, u64>>,
    /// Microseconds per phase of the pass that produced this generation.
    pub timings: BTreeMap<String, u64>,
    /// Workspace membership.
    pub files: Arc<HashSet<RelPath>>,
    /// When each path was last looked at by an incremental pass; paths not
    /// present were last looked at by the full build at `base_time`.
    pub seen: Seen,
    pub base_time: SystemTime,
    pub listing: &'static str,
    /// Deleted files whose former dependents are kept in the graph.
    pub tombstones: usize,
    pub built_at: SystemTime,
    /// Line ranges of the named scopes (methods, controller actions) per
    /// file, merged from every extractor's [`Facts::scopes`]. Lets a diff be
    /// turned into method-level seeds. Shared between generations until an
    /// extractor reports different ranges.
    pub scopes: Arc<BTreeMap<RelPath, Vec<ScopeRange>>>,
}

impl Snapshot {
    /// Scope ranges of `file`, sorted by start line; `None` when the
    /// extractors report none (the file then always counts as wholly changed).
    pub fn scopes_of(&self, file: &str) -> Option<&[ScopeRange]> {
        self.scopes.get(file).map(Vec::as_slice).filter(|v| !v.is_empty())
    }
}

impl Snapshot {
    /// Files among `paths` whose on-disk state may differ from what this
    /// snapshot indexed: modified since last seen, created, or deleted.
    /// Costs one `stat` per path.
    pub fn stale_files(&self, paths: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        for p in paths {
            if p.is_empty() {
                continue;
            }
            let seen_at = self.seen.read().ok().and_then(|s| s.get(p).copied());
            let known = self.files.contains(p);
            let stale = match std::fs::metadata(self.root.join(p)) {
                Ok(m) if m.is_file() => {
                    let mtime = m.modified().unwrap_or(SystemTime::now());
                    match (known, seen_at) {
                        (true, at) => mtime >= at.unwrap_or(self.base_time),
                        // Unknown file: stale unless we already looked at it (and excluded it) after its last write.
                        (false, Some(at)) => mtime >= at,
                        (false, None) => true,
                    }
                }
                _ => known,
            };
            if stale {
                out.push(p.clone());
            }
        }
        out
    }
}

pub struct Engine {
    root: PathBuf,
    pipeline: Pipeline,
    ws: Workspace,
    files: Arc<HashSet<RelPath>>,
    seen: Seen,
    base_time: SystemTime,
    outputs: Vec<ExtractOutput>,
    link_outputs: Vec<ExtractOutput>,
    /// Extractors whose last run failed; they get a full `build` next time.
    failed: Vec<bool>,
    tombstones: BTreeMap<RelPath, Vec<Edge>>,
    policy: Arc<Policy>,
    config_warning: Option<Warning>,
    generation: u64,
    listing: Listing,
    current: Option<Arc<Snapshot>>,
    scopes: Arc<BTreeMap<RelPath, Vec<ScopeRange>>>,
    /// Files whose out-edges changed in some output since the published
    /// graph; `None` = unknown (the next publish builds the graph).
    dirty: Option<BTreeSet<RelPath>>,
}

/// Whether `edges` are grouped by `from` in increasing order.
fn sorted_by_from(edges: &[Edge]) -> bool {
    edges.windows(2).all(|w| w[0].from <= w[1].from)
}

/// The edges of each `from`, in order.
fn groups(edges: &[Edge]) -> HashMap<&str, Vec<&Edge>> {
    let mut out: HashMap<&str, Vec<&Edge>> = HashMap::new();
    for e in edges {
        out.entry(e.from.as_str()).or_default().push(e);
    }
    out
}

/// Files whose out-edges (as an ordered list) differ between two complete
/// outputs.
fn changed_rows(old: &[Edge], new: &[Edge], out: &mut BTreeSet<RelPath>) {
    if old == new {
        return;
    }
    if sorted_by_from(old) && sorted_by_from(new) {
        let (mut i, mut j) = (0, 0);
        while i < old.len() || j < new.len() {
            let from = match (old.get(i), new.get(j)) {
                (Some(a), Some(b)) => a.from.as_str().min(b.from.as_str()),
                (Some(a), None) => a.from.as_str(),
                (None, Some(b)) => b.from.as_str(),
                (None, None) => break,
            };
            let a = i + old[i..].iter().take_while(|e| e.from == from).count();
            let b = j + new[j..].iter().take_while(|e| e.from == from).count();
            if old[i..a] != new[j..b] {
                out.insert(from.to_string());
            }
            (i, j) = (a, b);
        }
        return;
    }
    let (a, b) = (groups(old), groups(new));
    for (from, v) in &a {
        if b.get(from) != Some(v) {
            out.insert(from.to_string());
        }
    }
    for from in b.keys() {
        if !a.contains_key(from) {
            out.insert(from.to_string());
        }
    }
}

/// Replaces the out-edges of `files` (sorted) in the complete edge set
/// `old` by `rows` (see [`ExtractOutput::replaced`]), keeping `from` order
/// when `old` has it, so the result is what a complete output would be.
fn splice(old: Vec<Edge>, files: &[RelPath], rows: Vec<Edge>) -> Vec<Edge> {
    let replaced = |f: &str| files.binary_search_by(|p| p.as_str().cmp(f)).is_ok();
    if !(sorted_by_from(&old) && sorted_by_from(&rows)) {
        let mut out: Vec<Edge> = old.into_iter().filter(|e| !replaced(&e.from)).collect();
        out.extend(rows);
        return out;
    }
    let mut out = Vec::with_capacity(old.len() + rows.len());
    let mut rows = rows.into_iter().peekable();
    for e in old {
        while rows.peek().is_some_and(|r| r.from < e.from) {
            out.extend(rows.next());
        }
        if !replaced(&e.from) {
            out.push(e);
        }
    }
    out.extend(rows);
    out
}

fn load_policy(root: &Path) -> (Policy, Option<Warning>) {
    match Policy::load(root) {
        Ok(p) => (p, None),
        Err(e) => {
            let w = Warning::new("config_invalid", format!("{e}; using built-in defaults until it is fixed"));
            (Policy::compile(ripple_core::Config::default()).expect("defaults compile"), Some(w))
        }
    }
}

fn us(t: Instant) -> u64 {
    t.elapsed().as_micros() as u64
}

impl Engine {
    /// Cold start: lists files and runs every extractor and linker.
    pub fn open(root: &Path, pipeline: Pipeline) -> Result<(Self, Arc<Snapshot>)> {
        let root = root.to_path_buf();
        let (policy, config_warning) = load_policy(&root);
        let n_ex = pipeline.extractors.len();
        let n_ln = pipeline.linkers.len();
        let mut engine = Self {
            ws: Workspace { root: root.clone(), files: Vec::new(), config: policy.config.clone() },
            root,
            pipeline,
            files: Arc::default(),
            seen: Arc::default(),
            base_time: SystemTime::now(),
            outputs: vec![ExtractOutput::default(); n_ex],
            link_outputs: vec![ExtractOutput::default(); n_ln],
            failed: vec![false; n_ex],
            tombstones: BTreeMap::new(),
            policy: Arc::new(policy),
            config_warning,
            generation: 0,
            listing: Listing::Git,
            current: None,
            scopes: Arc::default(),
            dirty: None,
        };
        let snap = engine.full_build()?;
        Ok((engine, snap))
    }

    pub fn current(&self) -> Option<Arc<Snapshot>> {
        self.current.clone()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Every extractor and linker edge of the last pass (not the
    /// tombstones), in the order the graph is built from.
    pub fn output_edges(&self) -> impl Iterator<Item = &Edge> {
        self.outputs.iter().chain(&self.link_outputs).flat_map(|o| o.edges.iter())
    }

    /// The edges kept for the former dependents of deleted files.
    pub fn tombstone_edges(&self) -> impl Iterator<Item = &Edge> {
        self.tombstones.values().flatten()
    }

    /// The method-mention facts of the last pass, if an extractor reports them.
    pub fn mention_table(&self) -> Option<Arc<ripple_core::mentions::MentionTable>> {
        self.outputs.iter().find_map(|o| o.facts.mentions.clone())
    }

    /// Re-lists files, reloads the config and rebuilds everything.
    pub fn full_build(&mut self) -> Result<Arc<Snapshot>> {
        let start = Instant::now();
        let mut timings = BTreeMap::new();
        let (policy, warning) = load_policy(&self.root);
        self.policy = Arc::new(policy);
        self.config_warning = warning;
        self.ws.config = self.policy.config.clone();

        self.base_time = SystemTime::now();
        let t = Instant::now();
        let (files, listing) = files::list_files(&self.root, &self.policy)?;
        timings.insert("list_files_us".into(), us(t));
        self.listing = listing;
        self.files = Arc::new(files.iter().cloned().collect());
        self.ws.files = files;
        self.tombstones.retain(|p, _| !self.files.contains(p));

        self.dirty = None;
        self.run_extractors(None, &mut timings);
        self.run_linkers(None, &mut timings);
        let snap = self.publish(start, timings);
        if let Ok(mut seen) = self.seen.write() {
            seen.clear();
        }
        Ok(snap)
    }

    /// Turns raw paths reported by a watcher or a freshness check into a
    /// [`ChangeSet`] by looking at the file system: existing files are
    /// modified/added (unless ignored), missing ones removed; directories are
    /// expanded. Returns whether `ripple.yml` was among them.
    pub fn resolve_paths(&self, paths: &BTreeSet<RelPath>) -> (ChangeSet, bool) {
        let mut modified = BTreeSet::new();
        let mut removed = BTreeSet::new();
        let mut candidates = Vec::new();
        let mut config = false;
        let mut case = files::ExactCase::new(&self.root);
        for p in paths {
            if CONFIG_FILE_NAMES.contains(&p.as_str()) {
                config = true;
            }
            let abs = self.root.join(p);
            // On case-insensitive file systems `stat("Version")` finds `VERSION`;
            // only the exact spelling counts as the file existing.
            let meta = std::fs::metadata(&abs).ok().filter(|_| case.exists(p));
            match meta {
                Some(m) if m.is_file() => {
                    if self.files.contains(p) {
                        modified.insert(p.clone());
                    } else {
                        candidates.push(p.clone());
                    }
                }
                Some(m) if m.is_dir() => {
                    for f in files::walk_under(&self.root, &abs) {
                        if !self.files.contains(&f) {
                            candidates.push(f);
                        }
                    }
                    let prefix = format!("{p}/");
                    removed.extend(self.files.iter().filter(|f| f.starts_with(&prefix) && !case.exists(f)).cloned());
                }
                _ => {
                    if self.files.contains(p) {
                        removed.insert(p.clone());
                    } else {
                        // Possibly a deleted directory.
                        let prefix = format!("{p}/");
                        removed.extend(self.files.iter().filter(|f| f.starts_with(&prefix) && !case.exists(f)).cloned());
                    }
                }
            }
        }
        candidates.retain(|c| !self.policy.is_ignored(c) && !c.starts_with(".git/"));
        let ignored = files::git_ignored(&self.root, &candidates);
        modified.extend(candidates.into_iter().filter(|c| !ignored.contains(c)));
        (ChangeSet { modified: modified.into_iter().collect(), removed: removed.into_iter().collect() }, config)
    }

    /// Applies raw paths (see [`Self::resolve_paths`]). Returns the new
    /// snapshot, or the current one if nothing relevant changed.
    pub fn apply_paths(&mut self, paths: &BTreeSet<RelPath>) -> Result<Arc<Snapshot>> {
        let pass_start = SystemTime::now();
        let (changes, config_changed) = self.resolve_paths(paths);
        let snap = self.apply_resolved(&changes, config_changed)?;
        // Recorded only after publication, so a reader that finds a path
        // fresh by its `seen` time is guaranteed to find the new snapshot.
        if let Ok(mut seen) = self.seen.write() {
            for p in paths.iter().chain(&changes.modified).chain(&changes.removed) {
                seen.insert(p.clone(), pass_start);
            }
        }
        Ok(snap)
    }

    fn apply_resolved(&mut self, changes: &ChangeSet, config_changed: bool) -> Result<Arc<Snapshot>> {
        if config_changed {
            let (policy, warning) = load_policy(&self.root);
            let rebuild = warning.is_none() && Policy::extraction_changed(&self.policy.config, &policy.config);
            if warning.is_none() {
                self.policy = Arc::new(policy);
                self.ws.config = self.policy.config.clone();
            }
            self.config_warning = warning;
            if rebuild {
                return self.full_build();
            }
            if changes.is_empty() {
                // Same outputs: the graph only needs its node payloads again.
                self.dirty = Some(BTreeSet::new());
                return Ok(self.publish(Instant::now(), BTreeMap::new()));
            }
        }
        match &self.current {
            Some(cur) if changes.is_empty() => Ok(cur.clone()),
            _ => Ok(self.apply_changes(changes)),
        }
    }

    /// Incremental update: `update` on every extractor, then the linkers with
    /// `Some(changes)`, then a graph rebuild. Bumps the generation.
    pub fn apply_changes(&mut self, changes: &ChangeSet) -> Arc<Snapshot> {
        let start = Instant::now();
        let mut timings = BTreeMap::new();

        // The graph is patched unless the node set or the tombstones change.
        self.dirty = Some(BTreeSet::new());
        if !changes.removed.is_empty() || changes.modified.iter().any(|p| self.tombstones.contains_key(p)) {
            self.dirty = None;
        }
        // Keep the former dependents of deleted files reachable (see `tombstone`).
        if let Some(cur) = self.current.clone() {
            for p in &changes.removed {
                self.tombstone(&cur.graph, p, &changes.removed);
            }
        }
        for p in &changes.modified {
            self.tombstones.remove(p);
        }
        for list in self.tombstones.values_mut() {
            list.retain(|e| !changes.removed.contains(&e.from));
        }
        self.tombstones.retain(|_, v| !v.is_empty());

        let files = Arc::make_mut(&mut self.files);
        let mut membership_changed = false;
        for p in &changes.removed {
            membership_changed |= files.remove(p);
        }
        for p in &changes.modified {
            membership_changed |= files.insert(p.clone());
        }
        if membership_changed {
            let mut v: Vec<RelPath> = files.iter().cloned().collect();
            v.sort_unstable();
            self.ws.files = v;
            self.dirty = None;
        }

        self.run_extractors(Some(changes), &mut timings);
        self.run_linkers(Some(changes), &mut timings);
        self.publish(start, timings)
    }

    /// Records the edges pointing at `path` so its former dependents remain
    /// listed after it is deleted. Only possible when the deletion is observed
    /// by a running engine; a cold start cannot know them.
    fn tombstone(&mut self, graph: &Graph, path: &str, removed: &[RelPath]) {
        let Some(node) = graph.id(path) else { return };
        let edges: Vec<Edge> = graph
            .dependents(node)
            .filter(|(from, _)| !removed.iter().any(|r| r == graph.path(*from)))
            .map(|(from, e)| {
                let (_, _, d) = graph.edge(e);
                let detail = graph.edge_detail(e).map(|s| s.trim_start_matches("deleted: ").to_string()).unwrap_or_default();
                let mut edge = Edge::new(graph.path(from), path, d.kind, d.confidence).at(d.line);
                edge.detail = Some(format!("deleted: {detail}").trim_end().trim_end_matches(':').to_string());
                edge
            })
            .collect();
        if !edges.is_empty() {
            self.tombstones.insert(path.to_string(), edges);
        }
    }

    fn run_extractors(&mut self, changes: Option<&ChangeSet>, timings: &mut BTreeMap<String, u64>) {
        let ws = &self.ws;
        let failed = &self.failed;
        let results: Vec<(String, std::thread::Result<Result<ExtractOutput>>, u64)> = std::thread::scope(|s| {
            let handles: Vec<_> = self
                .pipeline
                .extractors
                .iter_mut()
                .enumerate()
                .map(|(i, ex)| {
                    let full = changes.is_none() || failed[i];
                    s.spawn(move || {
                        let t = Instant::now();
                        let name = ex.name().to_string();
                        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match (full, changes) {
                            (false, Some(c)) => ex.update_rows(ws, c),
                            _ => ex.build(ws),
                        }));
                        (name, r, us(t))
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or_else(|_| ("?".into(), Err(Box::new("thread panicked")), 0))).collect()
        });
        for (i, (name, r, t)) in results.into_iter().enumerate() {
            timings.insert(format!("extract_{name}_us"), t);
            match r {
                Ok(Ok(mut out)) => {
                    let old = std::mem::take(&mut self.outputs[i].edges);
                    match out.replaced.take() {
                        Some(files) => {
                            if let Some(d) = &mut self.dirty {
                                d.extend(files.iter().cloned());
                            }
                            out.edges = splice(old, &files, std::mem::take(&mut out.edges));
                        }
                        None => {
                            if let Some(d) = &mut self.dirty {
                                changed_rows(&old, &out.edges, d);
                            }
                        }
                    }
                    self.outputs[i] = out;
                    self.failed[i] = false;
                }
                Ok(Err(e)) => {
                    self.failed[i] = true;
                    self.outputs[i].warnings = vec![Warning::new("extractor_failed", format!("{name}: {e:#}; keeping its previous edges"))];
                }
                Err(_) => {
                    self.failed[i] = true;
                    self.outputs[i].warnings = vec![Warning::new("extractor_panicked", format!("{name} panicked; keeping its previous edges"))];
                }
            }
        }
    }

    fn run_linkers(&mut self, changes: Option<&ChangeSet>, timings: &mut BTreeMap<String, u64>) {
        let t = Instant::now();
        let mut facts = Facts::default();
        for o in &self.outputs {
            facts.merge(o.facts.clone());
        }
        // Extractor outputs are complete (not deltas), so the merged map is the
        // current truth; a failed extractor keeps its previous output and facts.
        let mut scopes = std::mem::take(&mut facts.scopes);
        for ranges in scopes.values_mut() {
            ranges.sort_by_key(|r| (r.start, r.end));
        }
        scopes.retain(|_, v| !v.is_empty());
        if *self.scopes != scopes {
            self.scopes = Arc::new(scopes.clone());
        }
        facts.scopes = scopes;
        timings.insert("merge_facts_us".into(), us(t));
        for (i, linker) in self.pipeline.linkers.iter_mut().enumerate() {
            let t = Instant::now();
            let name = linker.name().to_string();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| linker.link(&self.ws, &facts, changes)));
            timings.insert(format!("link_{name}_us"), us(t));
            match r {
                Ok(Ok(out)) => {
                    if let Some(d) = &mut self.dirty {
                        changed_rows(&self.link_outputs[i].edges, &out.edges, d);
                    }
                    self.link_outputs[i] = out
                }
                Ok(Err(e)) => self.link_outputs[i].warnings = vec![Warning::new("linker_failed", format!("{name}: {e:#}; keeping its previous edges"))],
                Err(_) => self.link_outputs[i].warnings = vec![Warning::new("linker_panicked", format!("{name} panicked; keeping its previous edges"))],
            }
        }
    }

    /// The current graph with the rows of the dirty files replaced (see
    /// [`Graph::patched`]), or `None` when it must be built.
    fn patch_graph(&self) -> Option<Graph> {
        if std::env::var_os("RIPPLE_NO_GRAPH_PATCH").is_some() {
            return None;
        }
        let dirty = self.dirty.as_ref()?;
        let cur = self.current.as_ref()?;
        let outputs: Vec<&ExtractOutput> = self.outputs.iter().chain(&self.link_outputs).collect();
        let mut rows: Vec<(&str, Vec<&Edge>)> = dirty.iter().map(|f| (f.as_str(), Vec::new())).collect();
        let index: HashMap<&str, usize> = dirty.iter().enumerate().map(|(i, f)| (f.as_str(), i)).collect();
        for o in &outputs {
            if sorted_by_from(&o.edges) {
                for (from, row) in rows.iter_mut() {
                    let a = o.edges.partition_point(|e| e.from.as_str() < *from);
                    row.extend(o.edges[a..].iter().take_while(|e| e.from == *from));
                }
            } else {
                for e in &o.edges {
                    if let Some(&i) = index.get(e.from.as_str()) {
                        rows[i].1.push(e);
                    }
                }
            }
        }
        for e in self.tombstones.values().flatten() {
            if let Some(&i) = index.get(e.from.as_str()) {
                rows[i].1.push(e);
            }
        }
        let all = outputs.iter().flat_map(|o| o.edges.iter()).chain(self.tombstones.values().flatten());
        cur.graph.patched(&rows, all, |p| self.files.contains(p))
    }

    fn publish(&mut self, start: Instant, mut timings: BTreeMap<String, u64>) -> Arc<Snapshot> {
        let t = Instant::now();
        let mut graph = match self.patch_graph() {
            Some(g) => {
                timings.insert("graph_patch_us".into(), us(t));
                g
            }
            None => {
                let edges = self.outputs.iter().chain(&self.link_outputs).flat_map(|o| o.edges.iter()).chain(self.tombstones.values().flatten());
                let g = Graph::build(edges, self.ws.files.iter().map(String::as_str));
                timings.insert("graph_build_us".into(), us(t));
                g
            }
        };
        self.dirty = None;
        if let Some(m) = self.outputs.iter().find_map(|o| o.facts.mentions.as_ref()) {
            graph.set_mentions(m);
        }
        // Size/complexity tables (shared Arcs): `snap.graph.metrics(node)`.
        graph.set_metrics(self.outputs.iter().flat_map(|o| o.facts.metrics.iter().cloned()));
        timings.insert("graph_us".into(), us(t));
        let t = Instant::now();
        let index = PolicyIndex::build(&self.policy, &graph);
        timings.insert("classify_us".into(), us(t));

        let mut warnings: Vec<Warning> = self.outputs.iter().chain(&self.link_outputs).flat_map(|o| o.warnings.iter().cloned()).collect();
        warnings.extend(self.config_warning.clone());
        warnings.sort_by(|a, b| (&a.code, &a.file, &a.message).cmp(&(&b.code, &b.file, &b.message)));
        let mut stats = BTreeMap::new();
        let names = self.pipeline.extractors.iter().map(|e| e.name()).chain(self.pipeline.linkers.iter().map(|l| l.name()));
        for (name, out) in names.zip(self.outputs.iter().chain(&self.link_outputs)) {
            let mut s = out.stats.clone();
            s.insert("edges".into(), out.edges.len() as u64);
            s.insert("warnings".into(), out.warnings.len() as u64);
            stats.insert(name.to_string(), s);
        }
        self.generation += 1;
        timings.insert("total_us".into(), us(start));
        let snap = Arc::new(Snapshot {
            generation: self.generation,
            root: self.root.clone(),
            graph,
            policy: self.policy.clone(),
            index,
            warnings,
            stats,
            timings,
            files: self.files.clone(),
            seen: self.seen.clone(),
            base_time: self.base_time,
            listing: self.listing.as_str(),
            tombstones: self.tombstones.len(),
            built_at: SystemTime::now(),
            scopes: self.scopes.clone(),
        });
        self.current = Some(snap.clone());
        snap
    }
}
