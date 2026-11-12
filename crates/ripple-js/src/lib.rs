//! JavaScript / TypeScript extractor: the module graph, plus HTTP call sites for
//! the Ruby boundary linker.
//!
//! Pipeline (see the module docs for each stage):
//! 1. `parse` — oxc parse of every JS/TS/Vue file, in parallel, cached by content hash
//!    (`refs::Parsed`: module references + call sites, independent of the file set).
//! 2. `resolve` — specifier → repo file, over a virtual file system built from
//!    `ws.files`, with tsconfig paths, aliases and workspace packages.
//! 3. `edges` — references + resolution → deduplicated edges, warnings, counters.
//!
//! `update` re-parses only changed files and re-resolves only what a change can
//! affect: the changed files themselves, or *everything* when the set of files
//! changed (an added/removed file can alter any specifier's resolution) or a
//! config file (`tsconfig*`, `jsconfig*`, `package.json`) changed.
//!
//! Known over-approximation: edges are file-level, so a barrel file
//! (`index.ts` re-exporting a directory) makes every importer of the barrel
//! depend on every re-exported file's changes. Not solved here.

mod callsites;
mod edges;
mod linkctx;
mod parse;
mod paths;
mod refs;
mod resolve;
mod tsconfig;
mod vfs;
mod vue;
mod workspaces;
#[cfg(feature = "mem-profile")]
pub mod memprof;

use anyhow::Result;
use callsites::CallSiteConfig;
use rayon::prelude::*;
use refs::{Parsed, Ref, RefKind};
use resolve::ModuleResolver;
use ripple_core::{ChangeSet, ExtractOutput, Extractor, Warning, Workspace};
use std::collections::{BTreeMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Instant;

pub use callsites::CallSiteConfig as JsCallSiteConfig;

/// oxc and our visitors recurse on AST depth; give the workers room so a
/// pathological file cannot overflow a small default stack.
const WORKER_STACK_BYTES: usize = 64 * 1024 * 1024;

#[derive(Default)]
pub struct JsExtractor {
    pool: Option<Arc<rayon::ThreadPool>>,
    state: Option<State>,
}

struct State {
    /// Everything `parse` and `resolve` depend on besides file contents.
    fingerprint: String,
    js: ripple_core::config::JsConfig,
    calls_cfg: CallSiteConfig,
    resolver: ModuleResolver,
    config_deps: HashSet<String>,
    parsed: BTreeMap<String, Parsed>,
    results: BTreeMap<String, edges::FileResult>,
}

impl JsExtractor {
    pub fn new() -> Self {
        Self::default()
    }

    fn pool(&mut self) -> Result<Arc<rayon::ThreadPool>> {
        if self.pool.is_none() {
            let pool = rayon::ThreadPoolBuilder::new().stack_size(WORKER_STACK_BYTES).thread_name(|i| format!("ripple-js-{i}")).build()?;
            self.pool = Some(Arc::new(pool));
        }
        Ok(Arc::clone(self.pool.as_ref().expect("pool initialised above")))
    }
}

fn fingerprint(ws: &Workspace) -> String {
    let b = &ws.config.boundary;
    format!("{}\u{0}{}\u{0}{:?}\u{0}{:?}", ws.root.display(), serde_json::to_string(&ws.config.js).unwrap_or_default(), b.http_callees, b.url_prefixes)
}

fn call_cfg(ws: &Workspace) -> CallSiteConfig {
    CallSiteConfig { callees: ws.config.boundary.http_callees.clone(), prefixes: ws.config.boundary.url_prefixes.clone() }
}

/// Reads and parses one file; never panics and never fails (problems go in `Parsed`).
fn parse_file(root: &Path, rel: &str, cfg: &CallSiteConfig) -> Parsed {
    let bytes = match std::fs::read(root.join(rel)) {
        Ok(b) => b,
        Err(e) => return Parsed { error: Some(e.to_string()), unreadable: true, ..Parsed::default() },
    };
    let hash = xxhash_rust::xxh3::xxh3_64(&bytes);
    let text = String::from_utf8_lossy(&bytes);
    let attempt = std::panic::catch_unwind(AssertUnwindSafe(|| parse_text(rel, &text, cfg)));
    match attempt {
        Ok(mut parsed) => {
            parsed.hash = hash;
            parsed
        }
        Err(_) => Parsed { hash, error: Some("internal parser failure".into()), ..Parsed::default() },
    }
}

fn parse_text(rel: &str, text: &str, cfg: &CallSiteConfig) -> Parsed {
    let mut collected = parse::Collected::default();
    let mut error = None;
    if rel.ends_with(".vue") {
        for block in vue::script_blocks(text) {
            if let Some(src) = &block.src {
                collected.refs.push(Ref { line: block.line_offset + 1, kind: RefKind::Import { specifier: src.clone(), dynamic: false, type_only: false } });
            }
            let e = parse::parse_block(block.content, block.source_type, block.line_offset, rel, cfg, &mut collected);
            error = error.or(e);
        }
    } else if let Some(st) = parse::source_type_for(rel) {
        error = parse::parse_block(text, st, 0, rel, cfg, &mut collected);
    }
    Parsed { hash: 0, refs: collected.refs, calls: collected.calls, unclassified: collected.unclassified, error, unreadable: false }
}

impl State {
    fn assemble(&self, files_parsed: usize, phase_us: &[(&str, u128)]) -> ExtractOutput {
        let mut out = ExtractOutput::default();
        let mut counts = edges::Counts::default();
        let mut parse_errors = 0u64;
        let mut unclassified = 0u64;
        for (code, msg) in &self.resolver.problems {
            out.warnings.push(Warning::new(code, msg.clone()));
        }
        for (file, parsed) in &self.parsed {
            if let Some(err) = &parsed.error {
                let code = if parsed.unreadable { "js_read_error" } else { "js_parse_error" };
                parse_errors += 1;
                out.warnings.push(Warning::new(code, format!("{file}: {err}")).file(file));
            }
            out.facts.js_call_sites.extend(parsed.calls.iter().cloned());
            unclassified += parsed.unclassified.iter().filter(|u| u.is_some()).count() as u64;
            if let Some(res) = self.results.get(file) {
                out.edges.extend(res.edges.iter().cloned());
                out.warnings.extend(res.warnings.iter().cloned());
                counts.add(&res.counts);
            }
        }
        let s = &mut out.stats;
        let mut put = |k: &str, v: u64| {
            s.insert(k.to_string(), v);
        };
        put("js_files", self.parsed.len() as u64);
        put("js_files_parsed", files_parsed as u64);
        put("js_parse_errors", parse_errors);
        put("js_imports_total", counts.imports);
        put("js_imports_resolved", counts.resolved);
        put("js_imports_external", counts.external);
        put("js_imports_unresolved", counts.unresolved);
        put("js_relative_total", counts.relative);
        put("js_relative_resolved", counts.relative_resolved);
        put("js_alias_total", counts.alias);
        put("js_alias_resolved", counts.alias_resolved);
        put("js_workspace_total", counts.workspace);
        put("js_workspace_resolved", counts.workspace_resolved);
        put("js_globs", counts.globs);
        put("js_glob_matches", counts.glob_matches);
        put("js_call_sites", out.facts.js_call_sites.len() as u64);
        put("js_call_sites_link", out.facts.js_call_sites.iter().filter(|c| c.link).count() as u64);
        put("js_call_sites_unclassified", unclassified);
        put("js_edges", out.edges.len() as u64);
        put("js_workspace_packages", self.resolver.workspaces.packages.len() as u64);
        put("js_tsconfigs", self.resolver.config_count() as u64);
        for (name, us) in phase_us {
            put(&format!("js_us_{name}"), *us as u64);
        }
        out
    }

    fn refresh_config_deps(&mut self) {
        self.config_deps = self.resolver.config_deps().cloned().collect();
    }
}

fn micros(t: Instant) -> u128 {
    t.elapsed().as_micros()
}

impl JsExtractor {
    /// Call sites from the last build/update whose URL use could not be
    /// classified as navigation or data (they keep `link = false`), with a
    /// short description of the context (`arg:useState`, `prop:endpoint`,
    /// `var:return`, ...). For diagnostics.
    pub fn unclassified_call_sites(&self) -> Vec<(&ripple_core::JsCallSite, &str)> {
        let Some(state) = &self.state else { return Vec::new() };
        state.parsed.values().flat_map(|p| p.calls.iter().zip(&p.unclassified).filter_map(|(c, u)| u.as_deref().map(|u| (c, u)))).collect()
    }
}

impl Extractor for JsExtractor {
    fn name(&self) -> &'static str {
        "js"
    }

    fn build(&mut self, ws: &Workspace) -> Result<ExtractOutput> {
        let total = Instant::now();
        self.state = None;
        let pool = self.pool()?;
        let calls_cfg = call_cfg(ws);

        let t = Instant::now();
        let files = Arc::new(RwLock::new(vfs::FileSet::from_files(&ws.files)));
        let resolver = ModuleResolver::new(ws.root.clone(), files, &ws.config.js);
        let config_us = micros(t);

        let t = Instant::now();
        let code: Vec<&String> = ws.files.iter().filter(|f| paths::is_code(f)).collect();
        let parsed: BTreeMap<String, Parsed> =
            pool.install(|| code.par_iter().map(|f| ((*f).clone(), parse_file(&ws.root, f, &calls_cfg))).collect::<Vec<_>>()).into_iter().collect();
        let parse_us = micros(t);

        let t = Instant::now();
        let results = resolve_all(&pool, &resolver, &parsed);
        let resolve_us = micros(t);

        let mut state = State { fingerprint: fingerprint(ws), js: ws.config.js.clone(), calls_cfg, resolver, config_deps: HashSet::new(), parsed, results };
        state.refresh_config_deps();
        let t = Instant::now();
        let n = state.parsed.len();
        let mut out = state.assemble(n, &[]);
        let assemble_us = micros(t);
        out.stats.insert("js_us_config".into(), config_us as u64);
        out.stats.insert("js_us_parse".into(), parse_us as u64);
        out.stats.insert("js_us_resolve".into(), resolve_us as u64);
        out.stats.insert("js_us_assemble".into(), assemble_us as u64);
        out.stats.insert("js_us_total".into(), micros(total) as u64);
        self.state = Some(state);
        Ok(out)
    }

    fn update(&mut self, ws: &Workspace, changes: &ChangeSet) -> Result<ExtractOutput> {
        let total = Instant::now();
        let fp = fingerprint(ws);
        if self.state.as_ref().is_none_or(|s| s.fingerprint != fp) {
            return self.build(ws);
        }
        let pool = self.pool()?;
        let state = self.state.as_mut().expect("checked above");

        // 1. Apply the change set to the file set.
        let mut structural = false;
        let mut config_touched = false;
        let mut to_parse: Vec<&String> = Vec::new();
        {
            let mut set = state.resolver.files.write().unwrap_or_else(|e| e.into_inner());
            for r in &changes.removed {
                if set.remove(r) {
                    structural = true;
                    state.parsed.remove(r);
                    state.results.remove(r);
                }
                config_touched |= paths::is_config_name(r) || state.config_deps.contains(r);
            }
            for m in &changes.modified {
                if set.insert(m) {
                    structural = true;
                }
                config_touched |= paths::is_config_name(m) || state.config_deps.contains(m);
                if paths::is_code(m) {
                    to_parse.push(m);
                }
            }
            if set.len() != ws.files.len() {
                // The change set did not account for every difference; do not guess.
                drop(set);
                return self.build(ws);
            }
        }

        // 2. Re-parse changed code files; keep only those whose content really changed.
        let t = Instant::now();
        let cfg = &state.calls_cfg;
        let fresh: Vec<(String, Parsed)> = pool.install(|| to_parse.par_iter().map(|f| ((*f).clone(), parse_file(&ws.root, f, cfg))).collect());
        let parse_us = micros(t);
        let files_parsed = fresh.len();
        let mut changed: Vec<String> = Vec::new();
        for (file, parsed) in fresh {
            if state.parsed.get(&file).is_some_and(|old| old.hash == parsed.hash && !old.unreadable && !parsed.unreadable) {
                continue;
            }
            state.parsed.insert(file.clone(), parsed);
            changed.push(file);
        }

        // 3. Re-resolve what the change can affect.
        let t = Instant::now();
        if config_touched {
            state.resolver.load_config(&state.js);
            state.refresh_config_deps();
        } else if structural {
            state.resolver.clear_cache();
        }
        if structural || config_touched {
            state.results = resolve_all(&pool, &state.resolver, &state.parsed);
        } else {
            let fresh: Vec<(String, edges::FileResult)> = pool.install(|| {
                changed.par_iter().map(|f| (f.clone(), edges::file_result(&state.resolver, f, &state.parsed[f]))).collect()
            });
            state.results.extend(fresh);
        }
        let resolve_us = micros(t);

        let t = Instant::now();
        let mut out = state.assemble(files_parsed, &[]);
        out.stats.insert("js_us_parse".into(), parse_us as u64);
        out.stats.insert("js_us_resolve".into(), resolve_us as u64);
        out.stats.insert("js_us_assemble".into(), micros(t) as u64);
        out.stats.insert("js_us_total".into(), micros(total) as u64);
        Ok(out)
    }
}

fn resolve_all(pool: &rayon::ThreadPool, resolver: &ModuleResolver, parsed: &BTreeMap<String, Parsed>) -> BTreeMap<String, edges::FileResult> {
    let entries: Vec<(&String, &Parsed)> = parsed.iter().collect();
    pool.install(|| entries.par_iter().map(|(f, p)| ((*f).clone(), edges::file_result(resolver, f, p))).collect::<Vec<_>>()).into_iter().collect()
}
