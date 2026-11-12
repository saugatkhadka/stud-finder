//! Turns one file's parsed references into edges, warnings and counters.

use crate::paths;
use crate::refs::{Parsed, RefKind};
use crate::resolve::{Class, ModuleResolver, Outcome};
use globset::{GlobBuilder, GlobSetBuilder};
use regex::Regex;
use ripple_core::{Confidence, Edge, EdgeKind, Warning};
use std::collections::HashMap;

/// Per-file counters, summed into `ExtractOutput::stats`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Counts {
    pub imports: u64,
    pub resolved: u64,
    pub external: u64,
    pub unresolved: u64,
    pub relative: u64,
    pub relative_resolved: u64,
    pub alias: u64,
    pub alias_resolved: u64,
    pub workspace: u64,
    pub workspace_resolved: u64,
    /// `import.meta.glob` / `require.context` references and how many files they matched.
    pub globs: u64,
    pub glob_matches: u64,
}

impl Counts {
    pub fn add(&mut self, o: &Counts) {
        self.imports += o.imports;
        self.resolved += o.resolved;
        self.external += o.external;
        self.unresolved += o.unresolved;
        self.relative += o.relative;
        self.relative_resolved += o.relative_resolved;
        self.alias += o.alias;
        self.alias_resolved += o.alias_resolved;
        self.workspace += o.workspace;
        self.workspace_resolved += o.workspace_resolved;
        self.globs += o.globs;
        self.glob_matches += o.glob_matches;
    }
}

#[derive(Debug, Default, Clone)]
pub struct FileResult {
    pub edges: Vec<Edge>,
    pub warnings: Vec<Warning>,
    pub counts: Counts,
}

/// Keeps one edge per `(to, kind)`: highest confidence, then lowest line, then
/// smallest detail. The engine collapses duplicates the same way; doing it here
/// keeps the output small and the choice deterministic.
struct EdgeSet(HashMap<(String, EdgeKind), Edge>);

impl EdgeSet {
    fn insert(&mut self, edge: Edge) {
        let key = (edge.to.clone(), edge.kind);
        let rank = |e: &Edge| (std::cmp::Reverse(e.confidence), e.line, e.detail.clone());
        match self.0.get(&key) {
            Some(cur) if rank(cur) <= rank(&edge) => {}
            _ => {
                self.0.insert(key, edge);
            }
        }
    }
}

pub fn file_result(resolver: &ModuleResolver, file: &str, parsed: &Parsed) -> FileResult {
    let mut set = EdgeSet(HashMap::new());
    let mut res = FileResult::default();
    for r in &parsed.refs {
        match &r.kind {
            RefKind::Import { specifier, dynamic, type_only } => {
                res.counts.imports += 1;
                let outcome = resolver.resolve(file, specifier);
                match &outcome {
                    Outcome::Resolved { class, .. } => {
                        res.counts.resolved += 1;
                        bump(&mut res.counts, *class, true);
                    }
                    Outcome::External => res.counts.external += 1,
                    Outcome::Unresolved { class } => {
                        res.counts.unresolved += 1;
                        bump(&mut res.counts, *class, false);
                    }
                }
                match outcome {
                    Outcome::Resolved { target, .. } if target != file => {
                        let kind = if !paths::is_code(&target) {
                            EdgeKind::Asset
                        } else if *dynamic {
                            EdgeKind::JsDynamicImport
                        } else {
                            EdgeKind::JsImport
                        };
                        let detail = if *type_only { format!("{specifier} (type)") } else { specifier.clone() };
                        set.insert(Edge::new(file, target, kind, Confidence::Exact).at(r.line).detail(detail));
                    }
                    Outcome::Unresolved { class } => {
                        let what = match class {
                            Class::Relative => "relative import",
                            Class::Alias => "alias import",
                            Class::Workspace => "workspace package import",
                        };
                        res.warnings.push(Warning::new("js_unresolved_import", format!("{file}:{}: cannot resolve {what} '{specifier}'", r.line)).file(file));
                    }
                    _ => {}
                }
            }
            RefKind::Glob { patterns } => {
                res.counts.globs += 1;
                for target in expand_glob(resolver, file, patterns) {
                    res.counts.glob_matches += 1;
                    let detail = patterns.join(", ");
                    set.insert(inferred(file, target, r.line, detail));
                }
            }
            RefKind::Context { dir, recursive, filter } => {
                res.counts.globs += 1;
                for target in expand_context(resolver, file, dir, *recursive, filter.as_deref()) {
                    res.counts.glob_matches += 1;
                    set.insert(inferred(file, target, r.line, dir.clone()));
                }
            }
        }
    }
    let mut edges: Vec<Edge> = set.0.into_values().collect();
    edges.sort_by(|a, b| (&a.to, a.kind).cmp(&(&b.to, b.kind)));
    res.edges = edges;
    res
}

fn bump(c: &mut Counts, class: Class, resolved: bool) {
    let (total, ok) = match class {
        Class::Relative => (&mut c.relative, &mut c.relative_resolved),
        Class::Alias => (&mut c.alias, &mut c.alias_resolved),
        Class::Workspace => (&mut c.workspace, &mut c.workspace_resolved),
    };
    *total += 1;
    *ok += u64::from(resolved);
}

fn inferred(from: &str, to: String, line: u32, detail: String) -> Edge {
    let kind = if paths::is_code(&to) { EdgeKind::JsDynamicImport } else { EdgeKind::Asset };
    Edge::new(from, to, kind, Confidence::Inferred).at(line).detail(detail)
}

/// Files matched by `import.meta.glob`-style patterns. Supports `./`, `../`,
/// repo-root `/` patterns and `!negations`; aliased patterns are not expanded.
fn expand_glob(resolver: &ModuleResolver, importer: &str, patterns: &[String]) -> Vec<String> {
    let dir = paths::parent(importer);
    let mut include = GlobSetBuilder::new();
    let mut exclude = GlobSetBuilder::new();
    let mut roots = Vec::new();
    for p in patterns {
        let (neg, p) = p.strip_prefix('!').map_or((false, p.as_str()), |r| (true, r));
        let base = if p.starts_with('/') { "" } else { dir };
        if !(p.starts_with("./") || p.starts_with("../") || p.starts_with('/')) {
            continue;
        }
        let Some(full) = paths::join(base, p) else { continue };
        let Ok(glob) = GlobBuilder::new(&full).literal_separator(true).build() else { continue };
        if neg {
            exclude.add(glob);
        } else {
            include.add(glob);
            roots.push(literal_dir(&full));
        }
    }
    let (Ok(include), Ok(exclude)) = (include.build(), exclude.build()) else { return Vec::new() };
    let files = resolver.files();
    let mut out: Vec<String> = Vec::new();
    for root in roots {
        out.extend(files.under(&root).filter(|f| f.as_str() != importer && include.is_match(f.as_str()) && !exclude.is_match(f.as_str())).cloned());
    }
    out.sort();
    out.dedup();
    out
}

/// The longest directory prefix of a glob that contains no metacharacters.
fn literal_dir(glob: &str) -> String {
    let end = glob.find(['*', '?', '[', '{']).unwrap_or(glob.len());
    glob[..end].rsplit_once('/').map_or(String::new(), |(d, _)| d.to_string())
}

/// `require.context(dir, recursive, filter)`: files under `dir` whose
/// `./relative/path` matches `filter` (default: all).
fn expand_context(resolver: &ModuleResolver, importer: &str, dir: &str, recursive: bool, filter: Option<&str>) -> Vec<String> {
    let Some(base) = paths::join(paths::parent(importer), dir) else { return Vec::new() };
    let filter = filter.and_then(|f| Regex::new(f).ok());
    let files = resolver.files();
    files
        .under(&base)
        .filter(|f| f.as_str() != importer)
        .filter(|f| {
            let rel = if base.is_empty() { f.as_str() } else { &f[base.len() + 1..] };
            (recursive || !rel.contains('/')) && filter.as_ref().is_none_or(|re| re.is_match(&format!("./{rel}")))
        })
        .cloned()
        .collect()
}
