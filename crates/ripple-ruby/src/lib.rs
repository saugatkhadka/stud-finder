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

mod actions;
mod analyse;
mod edges;
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
use ripple_core::{ChangeSet, EdgeKind, ExtractOutput, Extractor, Workspace};
use rubydex::model::declaration::{Declaration, Namespace};
use rubydex::model::graph::Graph;
use rubydex::model::ids::{DeclarationId, NameId};
use rubydex::model::name::NameRef;
use rubydex::resolution::Resolver;
use std::collections::{BTreeMap, BTreeSet, HashSet};
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

    fn output(&self, ws: &Workspace, t: Timings, files_analysed: usize) -> ExtractOutput {
        let start = Instant::now();
        let inflector = match self
            .files
            .contains_key(INFLECTIONS)
            .then(|| std::fs::read_to_string(ws.abs(INFLECTIONS)))
        {
            Some(Ok(src)) => Inflector::new().with_app_rules(&src),
            _ => Inflector::new(),
        };
        let method_level = ws.config.ruby.method_level;
        let builder = edges::EdgeBuilder::new(
            &self.graph,
            &self.files,
            &ws.files,
            ws.config.ruby.namespace_definition_limit,
            inflector,
            method_level,
        );
        let (edges, es) = builder.run();
        let edges_us = micros(start);
        let start = Instant::now();
        let (scopes, mentions, method_stats) = if method_level {
            let o = scoped::compute(&self.files, &self.names);
            (o.scopes, Some(std::sync::Arc::new(o.mentions)), o.stats)
        } else {
            // `method_level` off: exactly the file-level output (no scope ranges).
            (BTreeMap::new(), None, BTreeMap::new())
        };
        let methods_us = micros(start);

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
        put(
            "ruby_namespaces_over_limit_canonical",
            es.namespace_canonical,
        );
        put(
            "ruby_namespaces_over_limit_no_canonical",
            es.namespace_no_canonical,
        );
        put("ruby_inferred_unresolved", es.inferred_unresolved);
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
        for kind in [
            EdgeKind::RubyConstant,
            EdgeKind::RubyRequire,
            EdgeKind::RailsInferred,
            EdgeKind::RailsView,
        ] {
            let n = edges.iter().filter(|e| e.kind == kind).count() as u64;
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
        }
        for (k, v) in method_stats {
            put(&k, v);
        }
        put("ruby_time_shadow_repair_us", t.shadow_us);
        put("ruby_shadow_docs_reindexed", t.shadow_docs as u64);

        ExtractOutput {
            edges,
            facts: ripple_core::Facts {
                scopes,
                mentions,
                ..Default::default()
            },
            warnings: self
                .files
                .values()
                .flat_map(|e| e.warnings.iter().cloned())
                .collect(),
            stats,
        }
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
    fn resolve(&mut self) -> u64 {
        const MAX_PASSES: usize = 6;
        let start = Instant::now();
        let mut last = None;
        for _ in 0..MAX_PASSES {
            Resolver::new(&mut self.graph).resolve();
            let now = self.progress_marker();
            if last == Some(now) {
                break;
            }
            last = Some(now);
        }
        micros(start)
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

        let start = Instant::now();
        let mut state = State {
            graph: Graph::new(),
            files: BTreeMap::new(),
            inconsistent: HashSet::new(),
            names: scoped::Interner::default(),
            method_level: ws.config.ruby.method_level,
        };
        state.apply(analysed);
        t.merge_us = micros(start);
        t.resolve_us = state.resolve();
        state.inconsistent = shadow::misattached(&state.graph);

        let out = state.output(ws, t, todo.len());
        self.state = Some(state);
        Ok(out)
    }

    /// Re-analyses changed files, updates the Rubydex graph in place and
    /// recomputes all edges. Besides `changes`, any wanted file missing from
    /// the cache is analysed and any cached file no longer wanted is dropped,
    /// so the result always matches a fresh `build` of `ws.files`.
    fn update(&mut self, ws: &Workspace, changes: &ChangeSet) -> anyhow::Result<ExtractOutput> {
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
            .chain(
                changes
                    .removed
                    .iter()
                    .filter(|p| state.files.contains_key(*p) && !wanted.contains_key(p.as_str())),
            )
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
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

        let start = Instant::now();
        let changed_ruby = || {
            todo.iter()
                .filter(|(_, k)| *k == SourceKind::Ruby)
                .map(|(p, _)| *p)
        };
        let before = shadow::constant_decls(&state.graph, changed_ruby());
        state.apply(analysed);
        t.merge_us = micros(start);
        t.resolve_us = state.resolve();

        let start = Instant::now();
        let changed: Vec<&str> = changed_ruby().collect();
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
        Ok(state.output(ws, t, todo.len()))
    }
}
