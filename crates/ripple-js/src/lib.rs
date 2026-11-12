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

mod bundler;
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
    // `.vue`: code lines of the script blocks plus non-blank lines elsewhere.
    let mut lines = 0u32;
    if rel.ends_with(".vue") {
        lines = ripple_core::metrics::nonblank_lines(text.as_bytes());
        for block in vue::script_blocks(text) {
            lines -= ripple_core::metrics::nonblank_lines(block.content.as_bytes()).min(lines);
            if let Some(src) = &block.src {
                collected.refs.push(Ref { line: block.line_offset + 1, kind: RefKind::Import { specifier: src.clone(), dynamic: false, type_only: false } });
            }
            let e = parse::parse_block(block.content, block.source_type, block.line_offset, rel, cfg, &mut collected);
            error = error.or(e);
        }
    } else if let Some(st) = parse::source_type_for(rel) {
        error = parse::parse_block(text, st, 0, rel, cfg, &mut collected);
    }
    let metrics = Some(collected.complexity.file(lines + collected.lines));
    Parsed { hash: 0, refs: collected.refs, calls: collected.calls, unclassified: collected.unclassified, error, unreadable: false, metrics }
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
        let t = Instant::now();
        let table = ripple_core::MetricsTable::build(self.parsed.iter().filter_map(|(f, p)| Some((f.as_str(), p.metrics?, &[][..]))), false);
        out.facts.metrics.push(Arc::new(table));
        put("js_us_metrics", micros(t) as u64);
        put("js_call_sites", out.facts.js_call_sites.len() as u64);
        put("js_call_sites_link", out.facts.js_call_sites.iter().filter(|c| c.link).count() as u64);
        put("js_call_sites_unclassified", unclassified);
        put("js_edges", out.edges.len() as u64);
        put("js_workspace_packages", self.resolver.workspaces.packages.len() as u64);
        put("js_tsconfigs", self.resolver.config_count() as u64);
        put("js_bundler_aliases", self.resolver.discovered_count() as u64);
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
                config_touched |= paths::is_config_name(r) || bundler::is_bundler_config(r) || state.config_deps.contains(r);
            }
            for m in &changes.modified {
                if set.insert(m) {
                    structural = true;
                }
                config_touched |= paths::is_config_name(m) || bundler::is_bundler_config(m) || state.config_deps.contains(m);
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

/// Debugging aid: `(start line, complexity)` of every function in `src`
/// (parsed as `rel`), sorted, as counted for [`ripple_core::metrics`].
#[doc(hidden)]
pub fn function_complexities(rel: &str, src: &str) -> Vec<(u32, u32)> {
    let cfg = CallSiteConfig { callees: Vec::new(), prefixes: Vec::new() };
    let mut out = parse::Collected { trace: Some(Vec::new()), ..parse::Collected::default() };
    if let Some(st) = parse::source_type_for(rel) {
        parse::parse_block(src, st, 0, rel, &cfg, &mut out);
    }
    let mut v = out.trace.unwrap_or_default();
    v.sort_unstable();
    v
}

#[cfg(test)]
mod metrics_tests {
    use super::*;
    use ripple_core::FileMetrics;

    fn metrics(rel: &str, src: &str) -> FileMetrics {
        let cfg = CallSiteConfig { callees: Vec::new(), prefixes: Vec::new() };
        parse_text(rel, src, &cfg).metrics.expect("metrics")
    }

    /// One function per case; expected values worked out by hand from
    /// ESLint's `complexity` rule (classic): 1 + if / ?: / case-with-test /
    /// for / for-in / for-of / while / do-while / catch / && / || / ?? /
    /// logical assignment / optional member or call / default value.
    #[test]
    fn complexity_per_function_matches_eslint_rules() {
        let cases: &[(&str, u32)] = &[
            ("function a() { return 1; }", 1),
            ("function b(x) { if (x) { return 1 } else if (y) { return 2 } else { return 3 } }", 3),
            ("function c(a, b = 1) { return a ? b : 0 }", 3),
            ("function d(x) { switch (x) { case 1: return a; case 2: case 3: return b; default: return c } }", 4),
            ("function e() { return (a && b) || (c ?? d) }", 4),
            ("function f() { x ||= 1; y &&= 2; z ??= 3; w += 1 }", 4),
            ("function g(u) { return u?.name?.toUpperCase?.() }", 4),
            ("function h(xs) { for (const x of xs) {} for (const k in o) {} for (let i = 0; i < 1; i++) {} while (a) {} do {} while (b) }", 6),
            ("function i() { try { a() } catch (e) { b() } finally { c() } }", 2),
            ("function j({ a = 1, b } = {}, [c = 2] = []) { ({ p = 1 } = q); [r = 2] = s; }", 7),
            ("const k = async (x: number): Promise<number> => x > 1 ? await f(x) : 0;", 2),
        ];
        for (src, want) in cases {
            let m = metrics("a.ts", src);
            assert_eq!((m.complexity, m.methods, u32::from(m.max_method)), (*want, 1, *want), "{src}");
        }
    }

    #[test]
    fn nested_functions_top_level_and_lines() {
        let src = "// header\nconst v = a || b;\n\nfunction k() {\n  /* inner\n     comment */\n  const f = (x) => x ? 1 : 2;\n  return f(1) || 0;\n}\nclass A {\n  m(x) { if (x) {} }\n  get g() { return this.a ?? 1 }\n}\n";
        // k: 2, arrow: 2, m: 2, g: 2, plus `||` at top level.
        assert_eq!(metrics("a.js", src), FileMetrics { lines: 9, complexity: 9, methods: 4, max_method: 2 });
        let jsx = "export const C = ({ items }) => (\n  <ul>{items.map((i) => i.ok && <li key={i.id}>{i.name}</li>)}</ul>\n);\n";
        assert_eq!(metrics("c.jsx", jsx), FileMetrics { lines: 3, complexity: 3, methods: 2, max_method: 2 });
        let vue = "<template>\n  <div>\n\n  </div>\n</template>\n<script>\n// c\nexport default { m(x) { return x || 1 } }\n</script>\n";
        assert_eq!(metrics("a.vue", vue), FileMetrics { lines: 7, complexity: 2, methods: 1, max_method: 2 });
    }
}
