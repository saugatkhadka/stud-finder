//! ripple-ruby: Ruby/Rails dependency edges.
//!
//! Pipeline per build/update:
//! 1. **analyse** (parallel, per changed file): read, Rubydex local graph,
//!    keep constant references that are real uses, ruby-prism Rails facts,
//!    template facts. Cached per file.
//! 2. **merge + resolve**: feed local graphs into a long-lived Rubydex
//!    [`Graph`] (`consume_document_changes` / `delete_document`), then run
//!    the resolver, which is incremental over the pending work.
//! 3. **edges** (all files): resolve cached facts against the graph and the
//!    file list. See [`edges`] for why this step is not incremental.
//!
//! Fast path: when every changed file keeps its [`fingerprint`] (a
//! "body-only" edit: method bodies, comments, Rails DSL arguments...), step 2
//! is skipped and step 3 runs for the changed files only
//! ([`Extractor::update_rows`]). In a batch, files that keep their
//! fingerprint are never fed to Rubydex; only the others are. A batch whose
//! Rubydex merge would cost more than a fresh build is rebuilt instead.

mod actions;
mod analyse;
mod edges;
mod fingerprint;
mod inflector;
mod lex;
mod methods;
mod paths;
mod rails;
mod scoped;
mod shadow;
mod slim;
mod templates;
mod views;
#[cfg(feature = "mem-profile")]
pub mod memprof;

use analyse::{Analysed, FileEntry};
use inflector::Inflector;
use paths::SourceKind;
use rayon::prelude::*;
use ripple_core::{ChangeSet, Edge, EdgeKind, ExtractOutput, Extractor, Workspace};
use rubydex::model::declaration::{Declaration, Namespace};
use rubydex::model::graph::Graph;
use rubydex::model::ids::{DeclarationId, NameId};
use rubydex::model::name::NameRef;
use rubydex::resolution::Resolver;
use std::collections::{BTreeMap, HashSet};
use std::time::Instant;

const INFLECTIONS: &str = "config/initializers/inflections.rb";

#[derive(Default)]
pub struct RubyExtractor {
    state: Option<State>,
}

struct State {
    graph: Graph,
    files: BTreeMap<String, FileEntry>,
    /// [`shadow::misattached`] after the last build/update, as the baseline
    /// for detecting inconsistencies an update introduces.
    inconsistent: HashSet<NameId>,
    /// Method names, workspace-wide. Append-only: ids never leak into the
    /// output (edges carry the names), so history does not matter.
    names: scoped::Interner,
    /// `ruby.method_level` the cached facts were collected with.
    method_level: bool,
    /// Edge stats of the current files against the current graph, so a
    /// fast-path update can patch them (see [`State::row_edges`]).
    totals: edges::EdgeStats,
    kind_counts: [u64; 4],
    over: edges::OverLimit,
    /// The last resolve reached its fixed point (else the fast path is off).
    converged: bool,
    /// Decayed sums of Rubydex merge time and of structurally changed files
    /// over past slow-path updates, and the decayed resolve+repair time, to
    /// predict a batch's cost against `cold_us` (the last full build).
    merge_us_sum: f64,
    merge_docs_sum: f64,
    resolve_us_avg: Option<f64>,
    cold_us: u64,
}

/// Edge kinds Ruby emits, in stats order.
const KINDS: [EdgeKind; 4] = [EdgeKind::RubyConstant, EdgeKind::RubyRequire, EdgeKind::RailsInferred, EdgeKind::RailsView];

fn kind_counts(edges: &[Edge]) -> [u64; 4] {
    let mut out = [0u64; 4];
    for e in edges {
        if let Some(i) = KINDS.iter().position(|k| *k == e.kind) {
            out[i] += 1;
        }
    }
    out
}

impl RubyExtractor {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Default)]
struct Timings {
    analyse_us: u64,
    merge_us: u64,
    resolve_us: u64,
    shadow_us: u64,
    shadow_docs: usize,
    /// Files that kept / changed their fingerprint (update only).
    fast_files: usize,
    slow_files: usize,
    batch_rebuilt: bool,
}

fn micros(since: Instant) -> u64 {
    since.elapsed().as_micros() as u64
}

fn wanted_files(ws: &Workspace) -> BTreeMap<&str, SourceKind> {
    if !ws.config.ruby.enabled {
        return BTreeMap::new();
    }
    ws.files
        .iter()
        .filter_map(|f| paths::classify(f).map(|k| (f.as_str(), k)))
        .collect()
}

fn analyse_all(ws: &Workspace, todo: &[(&str, SourceKind)]) -> Vec<Analysed> {
    let roots = &ws.config.ruby.autoload_roots;
    let method_level = ws.config.ruby.method_level;
    todo.par_iter()
        .map(|(path, kind)| analyse::analyse(&ws.root, path, *kind, roots, method_level))
        .collect()
}

impl State {
    /// Applies analysed files to the Rubydex graph and the per-file cache.
    fn apply(&mut self, analysed: Vec<Analysed>) {
        for mut a in analysed {
            self.names.globalize(&mut a.entry.methods);
            match a.local_graph {
                Some(lg) => self.graph.consume_document_changes(lg),
                None if a.entry.kind == Some(SourceKind::Ruby) => {
                    // Unreadable/unindexable now: make sure stale definitions go away.
                    self.graph.delete_document(&a.path);
                }
                None => {}
            }
            self.files.insert(a.path, a.entry);
        }
    }

    fn inflector(&self, ws: &Workspace) -> Inflector {
        match self
            .files
            .contains_key(INFLECTIONS)
            .then(|| std::fs::read_to_string(ws.abs(INFLECTIONS)))
        {
            Some(Ok(src)) => Inflector::new().with_app_rules(&src),
            _ => Inflector::new(),
        }
    }

    fn builder<'a>(&'a self, ws: &'a Workspace) -> edges::EdgeBuilder<'a> {
        edges::EdgeBuilder::new(
            &self.graph,
            &self.files,
            &ws.files,
            ws.config.ruby.namespace_definition_limit,
            self.inflector(ws),
            ws.config.ruby.method_level,
        )
    }

    /// Every file's edges; resets the totals.
    fn all_edges(&mut self, ws: &Workspace) -> Vec<Edge> {
        let (edges, stats, over) = self.builder(ws).run();
        self.kind_counts = kind_counts(&edges);
        self.totals = stats;
        self.over = over;
        edges
    }

    /// Fast path: swaps in `body` (files whose fingerprint did not change)
    /// and returns their new edges, patching the totals by the difference
    /// between their old and new contributions. Sound because neither the
    /// graph nor anything another file's edges read changes.
    fn row_edges(&mut self, ws: &Workspace, body: Vec<Analysed>) -> (Vec<Edge>, Vec<String>) {
        let mut body: Vec<(String, FileEntry)> = body.into_iter().map(|a| (a.path, a.entry)).collect();
        body.sort_by(|a, b| a.0.cmp(&b.0));
        let ((old_edges, old_stats, old_over), (new_edges, new_stats, new_over)) = {
            let mut b = self.builder(ws);
            let old = b.run_entries(body.iter().filter_map(|(p, _)| self.files.get_key_value(p).map(|(k, e)| (k.as_str(), e))));
            let new = b.run_entries(body.iter().map(|(p, e)| (p.as_str(), e)));
            (old, new)
        };
        self.totals.add(&old_stats, -1);
        self.totals.add(&new_stats, 1);
        let (old_k, new_k) = (kind_counts(&old_edges), kind_counts(&new_edges));
        for i in 0..KINDS.len() {
            self.kind_counts[i] = self.kind_counts[i] + new_k[i] - old_k[i];
        }
        for (d, (n, _)) in old_over {
            if let Some(e) = self.over.get_mut(&d) {
                e.0 -= n;
            }
        }
        for (d, (n, c)) in new_over {
            self.over.entry(d).or_insert((0, c)).0 += n;
        }
        self.over.retain(|_, (n, _)| *n > 0);
        let paths: Vec<String> = body.iter().map(|(p, _)| p.clone()).collect();
        for (p, e) in body {
            self.files.insert(p, e);
        }
        (new_edges, paths)
    }

    /// The output for `edges` (all edges, or with `replaced` the rows of
    /// those files) and the current totals.
    fn output(&self, ws: &Workspace, t: Timings, files_analysed: usize, edges: Vec<Edge>, edges_us: u64, replaced: Option<Vec<String>>) -> ExtractOutput {
        let es = &self.totals;
        let (namespace_canonical, namespace_no_canonical) = edges::namespace_counts(&self.over);
        let method_level = ws.config.ruby.method_level;
        let start = Instant::now();
        let (scopes, mentions, method_stats) = if method_level {
            let o = scoped::compute(&self.files, &self.names);
            (o.scopes, Some(std::sync::Arc::new(o.mentions)), o.stats)
        } else {
            // `method_level` off: exactly the file-level output (no scope ranges).
            (BTreeMap::new(), None, BTreeMap::new())
        };
        let methods_us = micros(start);
        let start = Instant::now();
        let metrics = self.metrics_table();
        let metrics_us = micros(start);
        let rest = Instant::now();

        let mut stats = BTreeMap::new();
        let mut put = |k: &str, v: u64| {
            stats.insert(k.to_string(), v);
        };
        let ruby = self
            .files
            .values()
            .filter(|e| e.kind == Some(SourceKind::Ruby))
            .count();
        put("ruby_files_indexed", ruby as u64);
        put("ruby_templates_scanned", (self.files.len() - ruby) as u64);
        put("ruby_files_analysed_this_run", files_analysed as u64);
        put("ruby_refs_total", es.refs_total);
        put("ruby_refs_resolved", es.refs_resolved);
        put("ruby_refs_unresolved", es.refs_unresolved);
        put("ruby_refs_external", es.refs_external);
        put("ruby_refs_prefix_fallback", es.refs_prefix_fallback);
        put("ruby_refs_top_level_fallback", es.refs_top_level_fallback);
        put(
            "ruby_refs_namespace_skipped",
            self.files
                .values()
                .map(|e| u64::from(e.passthrough_refs))
                .sum(),
        );
        put(
            "ruby_refs_to_namespace_without_canonical",
            es.refs_to_namespace_without_canonical,
        );
        put("ruby_namespaces_over_limit_canonical", namespace_canonical);
        put("ruby_namespaces_over_limit_no_canonical", namespace_no_canonical);
        put("ruby_inferred_unresolved", es.inferred_unresolved);
        put("ruby_serializer_model_unresolved", es.serializer_model_unresolved);
        put("ruby_template_helper_calls", es.template_helper_calls);
        put("ruby_template_helper_ambiguous", es.template_helper_ambiguous);
        put(
            "ruby_assoc_polymorphic_skipped",
            self.files
                .values()
                .map(|e| u64::from(e.facts.assoc_polymorphic_skipped))
                .sum(),
        );
        put(
            "ruby_assoc_dynamic_skipped",
            self.files
                .values()
                .map(|e| u64::from(e.facts.assoc_dynamic_skipped))
                .sum(),
        );
        put("ruby_requires_unresolved", es.requires_unresolved);
        put("ruby_renders_unresolved", es.renders_unresolved);
        put(
            "ruby_template_consts_unresolved",
            es.template_consts_unresolved,
        );
        put(
            "ruby_parse_failures",
            self.files
                .values()
                .flat_map(|e| &e.warnings)
                .filter(|w| w.code == "ruby_parse_error")
                .count() as u64,
        );
        for (kind, n) in KINDS.into_iter().zip(self.kind_counts) {
            put(
                &format!(
                    "ruby_edges_{}",
                    serde_json::to_value(kind)
                        .ok()
                        .and_then(|v| v.as_str().map(String::from))
                        .unwrap_or_default()
                ),
                n,
            );
        }
        put("ruby_time_analyse_us", t.analyse_us);
        put("ruby_time_merge_us", t.merge_us);
        put("ruby_time_resolve_us", t.resolve_us);
        put("ruby_time_edges_us", edges_us);
        if method_level {
            put("ruby_time_methods_us", methods_us);
            put("ruby_time_metrics_us", metrics_us);
        }
        for (k, v) in method_stats {
            put(&k, v);
        }
        put("ruby_time_shadow_repair_us", t.shadow_us);
        put("ruby_shadow_docs_reindexed", t.shadow_docs as u64);
        put("ruby_fast_files_this_run", t.fast_files as u64);
        put("ruby_slow_files_this_run", t.slow_files as u64);
        put("ruby_slow_batch_rebuilt_this_run", u64::from(t.batch_rebuilt));
        let http_requests = self.http_requests();
        let warnings = self
            .files
            .values()
            .flat_map(|e| e.warnings.iter().cloned())
            .collect();
        stats.insert("ruby_time_output_rest_us".into(), micros(rest));

        ExtractOutput {
            edges,
            facts: ripple_core::Facts {
                scopes,
                mentions,
                metrics: metrics.into_iter().collect(),
                http_requests,
                ..Default::default()
            },
            warnings,
            stats,
            replaced,
        }
    }

    /// Size/complexity of every analysed file, with per-method complexity
    /// of the outermost `def`s; `None` without `ruby.method_level`.
    fn metrics_table(&self) -> Option<std::sync::Arc<ripple_core::MetricsTable>> {
        use ripple_core::metrics::{MethodMetric, sat16};
        if !self.method_level {
            return None;
        }
        let mut methods: Vec<MethodMetric> = Vec::new();
        let mut rows: Vec<(&str, ripple_core::FileMetrics, usize, usize)> = Vec::with_capacity(self.files.len());
        for (path, e) in &self.files {
            let Some(m) = e.metrics() else { continue };
            let from = methods.len();
            methods.extend(e.methods.defs.iter().map(|d| MethodMetric {
                start: d.start,
                span: sat16(d.end.saturating_sub(d.start)),
                complexity: d.complexity,
            }));
            rows.push((path, m, from, methods.len()));
        }
        let table = ripple_core::MetricsTable::build(rows.iter().map(|&(p, m, a, b)| (p, m, &methods[a..b])), true);
        Some(std::sync::Arc::new(table))
    }

    /// Every test's HTTP requests, in file order.
    fn http_requests(&self) -> Vec<ripple_core::JsCallSite> {
        let mut out = Vec::new();
        for (path, e) in &self.files {
            for r in &e.facts.requests {
                out.push(ripple_core::JsCallSite {
                    file: path.clone(),
                    line: r.line,
                    method: Some(r.verb.clone()),
                    url: r.url.clone(),
                    callee: r.callee.clone(),
                    route_helper: r.helper,
                    link: false,
                });
            }
        }
        out
    }

    /// Repairs two Rubydex incremental gaps (see [`shadow`]): definitions
    /// attached to the wrong declaration, and names a new declaration may
    /// shadow. Every document using
    /// such a name is deleted and re-added in one batch, so Rubydex resolves
    /// it as a fresh build would. Fixing a misattachment creates the right
    /// declaration, which can in turn shadow names, hence rounds; a name is
    /// repaired at most once. Returns how many documents were re-indexed,
    /// or `None` when the repair cannot be done in place (the caller then
    /// rebuilds).
    fn repair_shadowing(
        &mut self,
        ws: &Workspace,
        changed: &[&str],
        decls_before: &HashSet<DeclarationId>,
    ) -> Option<usize> {
        const MAX_ROUNDS: usize = 4;
        let mut repaired: HashSet<NameId> = HashSet::new();
        let mut reindexed = 0;
        for _ in 0..MAX_ROUNDS {
            let new_decls: HashSet<DeclarationId> =
                shadow::constant_decls(&self.graph, changed.iter().copied())
                    .difference(decls_before)
                    .copied()
                    .collect();
            let mut names = shadow::shadowed(&self.graph, &new_decls);
            // Misattachments that predate this update are what a build produces too.
            let inconsistent = shadow::misattached(&self.graph);
            names.extend(inconsistent.difference(&self.inconsistent).copied());
            names.retain(|n| !repaired.contains(n));
            let docs = shadow::documents_using(&self.graph, &names);
            if docs.is_empty() {
                self.inconsistent = inconsistent;
                return Some(reindexed);
            }
            for doc in &docs {
                self.graph.delete_document(doc);
            }
            // A name still held elsewhere (e.g. as the nesting of another name) survives deletion.
            if names.iter().any(|n| self.graph.names().contains_key(n)) {
                return None;
            }
            let todo: Vec<(&str, SourceKind)> = docs
                .iter()
                .filter(|d| {
                    self.files
                        .get(*d)
                        .is_some_and(|e| e.kind == Some(SourceKind::Ruby))
                })
                .map(|d| (d.as_str(), SourceKind::Ruby))
                .collect();
            let analysed = analyse_all(ws, &todo);
            self.apply(analysed);
            self.resolve();
            reindexed += docs.len();
            repaired.extend(names);
        }
        None
    }

    /// Runs the resolver to a fixed point: at least two passes, then more
    /// while a pass still changes the number of resolved names or singleton
    /// classes (capped).
    ///
    /// Rubydex 0.4.3's `resolve()` ends with `handle_remaining_definitions`,
    /// which runs *after* its convergence loop and can create singleton
    /// classes (`def Foo.bar`, class-level ivars). References waiting on those
    /// only resolve on the *next* `resolve()` call (on GitLab `GC::Profiler`,
    /// on mastodon 12 names), so a single pass makes a fresh build differ
    /// from an update. Each extra pass retries every unresolved reference:
    /// about 60-70 ms on GitLab, 10 ms on mastodon.
    ///
    /// Sets `converged` when the fixed point was reached within the cap.
    fn resolve(&mut self) -> u64 {
        const MAX_PASSES: usize = 6;
        let start = Instant::now();
        let mut last = None;
        self.converged = false;
        for _ in 0..MAX_PASSES {
            Resolver::new(&mut self.graph).resolve();
            let now = self.progress_marker();
            if last == Some(now) {
                self.converged = true;
                break;
            }
            last = Some(now);
        }
        micros(start)
    }

    /// Whether updating Rubydex for `docs` structurally changed files is
    /// predicted to cost more than a fresh build. Merge time grows with the
    /// batch (Rubydex scans every declaration per changed document) and is
    /// predicted from past updates; before any, from the build time.
    fn batch_too_costly(&self, docs: usize) -> bool {
        if docs < 8 || self.cold_us == 0 {
            return false;
        }
        let per_doc = if self.merge_docs_sum > 0.0 { self.merge_us_sum / self.merge_docs_sum } else { self.cold_us as f64 / 150.0 };
        let predicted = per_doc * docs as f64 + self.resolve_us_avg.unwrap_or(0.0);
        predicted > 1.25 * self.cold_us as f64
    }

    fn record_cost(&mut self, docs: usize, merge_us: u64, resolve_us: u64) {
        if docs == 0 {
            return;
        }
        const DECAY: f64 = 0.7;
        self.merge_us_sum = DECAY * self.merge_us_sum + merge_us as f64;
        self.merge_docs_sum = DECAY * self.merge_docs_sum + docs as f64;
        let r = resolve_us as f64;
        self.resolve_us_avg = Some(self.resolve_us_avg.map_or(r, |a| DECAY * a + (1.0 - DECAY) * r));
    }

    /// (resolved names, singleton classes).
    fn progress_marker(&self) -> (usize, usize) {
        let resolved = self
            .graph
            .names()
            .values()
            .filter(|n| matches!(n, NameRef::Resolved(_)))
            .count();
        let singletons = self
            .graph
            .declarations()
            .values()
            .filter(|d| matches!(d, Declaration::Namespace(Namespace::SingletonClass(_))))
            .count();
        (resolved, singletons)
    }
}

impl RubyExtractor {
    /// [`Extractor::update`] (`rows` false: all edges) or
    /// [`Extractor::update_rows`] (`rows` true: on the fast path, only the
    /// changed files' edges).
    fn update_impl(&mut self, ws: &Workspace, changes: &ChangeSet, rows: bool) -> anyhow::Result<ExtractOutput> {
        let Some(state) = self.state.as_mut().filter(|s| s.method_level == ws.config.ruby.method_level) else {
            return self.build(ws);
        };
        let wanted = wanted_files(ws);
        let mut t = Timings::default();

        let start = Instant::now();
        let stale: Vec<String> = state
            .files
            .keys()
            .filter(|p| !wanted.contains_key(p.as_str()))
            .cloned()
            .collect();
        for path in &stale {
            if let Some(entry) = state.files.remove(path)
                && entry.kind == Some(SourceKind::Ruby)
            {
                state.graph.delete_document(path);
            }
        }
        let todo: Vec<(&str, SourceKind)> = changes
            .modified
            .iter()
            .filter_map(|p| wanted.get_key_value(p.as_str()))
            .chain(
                wanted
                    .iter()
                    .filter(|(p, _)| !state.files.contains_key(**p)),
            )
            .map(|(p, k)| (*p, *k))
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            .collect();
        let analysed = analyse_all(ws, &todo);
        t.analyse_us = micros(start);

        // Same file set (so the same template index, helper map, Zeitwerk
        // keys...) and the inflections unchanged: a file that keeps its
        // fingerprint needs no Rubydex work.
        let same_set = stale.is_empty() && todo.iter().all(|(p, k)| state.files.get(*p).is_some_and(|e| e.kind == Some(*k)));
        let (body, structural): (Vec<Analysed>, Vec<Analysed>) = analysed.into_iter().partition(|a| {
            same_set && a.path != INFLECTIONS && state.files.get(&a.path).is_some_and(|e| e.fp == a.entry.fp)
        });
        t.fast_files = body.len();
        t.slow_files = structural.len();
        let mut body = body;
        for a in &mut body {
            state.names.globalize(&mut a.entry.methods);
        }

        if structural.is_empty() && state.converged {
            let start = Instant::now();
            let (edges, replaced) = if rows {
                let (edges, paths) = state.row_edges(ws, body);
                (edges, Some(paths))
            } else {
                for a in body {
                    state.files.insert(a.path, a.entry);
                }
                (state.all_edges(ws), None)
            };
            let edges_us = micros(start);
            return Ok(state.output(ws, t, todo.len(), edges, edges_us, replaced));
        }

        let structural_ruby: Vec<String> = structural
            .iter()
            .filter(|a| a.entry.kind == Some(SourceKind::Ruby))
            .map(|a| a.path.clone())
            .collect();
        if state.batch_too_costly(structural_ruby.len()) {
            let mut out = self.build(ws)?;
            out.stats.insert("ruby_slow_batch_rebuilt_this_run".into(), 1);
            out.stats.insert("ruby_fast_files_this_run".into(), t.fast_files as u64);
            out.stats.insert("ruby_slow_files_this_run".into(), t.slow_files as u64);
            return Ok(out);
        }

        let start = Instant::now();
        let before = shadow::constant_decls(&state.graph, structural_ruby.iter().map(String::as_str));
        // Files that kept their fingerprint keep their Rubydex document.
        for a in body {
            state.files.insert(a.path, a.entry);
        }
        state.apply(structural);
        t.merge_us = micros(start);
        t.resolve_us = state.resolve();

        let start = Instant::now();
        let changed: Vec<&str> = structural_ruby.iter().map(String::as_str).collect();
        match state.repair_shadowing(ws, &changed, &before) {
            Some(n) => t.shadow_docs = n,
            None => {
                // Could not repair in place: correctness first.
                let mut out = self.build(ws)?;
                out.stats.insert("ruby_update_fell_back_to_build".into(), 1);
                return Ok(out);
            }
        }
        t.shadow_us = micros(start);
        state.record_cost(structural_ruby.len(), t.merge_us, t.resolve_us + t.shadow_us);
        let start = Instant::now();
        let edges = state.all_edges(ws);
        let edges_us = micros(start);
        Ok(state.output(ws, t, todo.len(), edges, edges_us, None))
    }
}

impl Extractor for RubyExtractor {
    fn name(&self) -> &'static str {
        "ruby"
    }

    fn build(&mut self, ws: &Workspace) -> anyhow::Result<ExtractOutput> {
        let todo: Vec<(&str, SourceKind)> = wanted_files(ws).into_iter().collect();
        let mut t = Timings::default();
        let start = Instant::now();
        let analysed = analyse_all(ws, &todo);
        t.analyse_us = micros(start);
        let t_analyse_us = t.analyse_us;

        let start = Instant::now();
        let mut state = State {
            graph: Graph::new(),
            files: BTreeMap::new(),
            inconsistent: HashSet::new(),
            names: scoped::Interner::default(),
            method_level: ws.config.ruby.method_level,
            totals: Default::default(),
            kind_counts: [0; 4],
            over: Default::default(),
            converged: false,
            merge_us_sum: 0.0,
            merge_docs_sum: 0.0,
            resolve_us_avg: None,
            cold_us: 0,
        };
        let build_start = start;
        state.apply(analysed);
        // Inserting in key order leaves B-tree leaves half full, and a
        // `FileEntry` is 512 bytes: a bulk rebuild packs them (GitLab: -15 MB).
        state.files = std::mem::take(&mut state.files).into_iter().collect();
        t.merge_us = micros(start);
        t.resolve_us = state.resolve();
        state.inconsistent = shadow::misattached(&state.graph);
        let start = Instant::now();
        let edges = state.all_edges(ws);
        let edges_us = micros(start);
        let out = state.output(ws, t, todo.len(), edges, edges_us, None);
        state.cold_us = micros(build_start) + t_analyse_us;
        // Cost history carries over a rebuild of the same workspace.
        if let Some(old) = self.state.take() {
            state.merge_us_sum = old.merge_us_sum;
            state.merge_docs_sum = old.merge_docs_sum;
            state.resolve_us_avg = old.resolve_us_avg;
        }
        self.state = Some(state);
        Ok(out)
    }

    /// Re-analyses changed files, updates the Rubydex graph in place (unless
    /// every changed file kept its fingerprint) and recomputes all edges.
    /// Besides `changes`, any wanted file missing from the cache is analysed
    /// and any cached file no longer wanted is dropped, so the result always
    /// matches a fresh `build` of `ws.files`.
    fn update(&mut self, ws: &Workspace, changes: &ChangeSet) -> anyhow::Result<ExtractOutput> {
        self.update_impl(ws, changes, false)
    }

    /// [`Extractor::update`], but on the fast path only the changed files'
    /// edges are computed and returned.
    fn update_rows(&mut self, ws: &Workspace, changes: &ChangeSet) -> anyhow::Result<ExtractOutput> {
        self.update_impl(ws, changes, true)
    }
}
