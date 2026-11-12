//! Per-file analysis, run in parallel: read the file once, build its Rubydex
//! local graph, pick out the constant references that count as *uses*, and
//! collect the Rails/template facts. Everything here depends only on the
//! file's own content, so it is cached per file and redone only when the
//! file changes.

use crate::paths::{self, SourceKind};
use crate::rails::{self, RubyFacts};
use crate::methods::{self, MethodFacts};
use crate::templates::{self, TemplateFacts};
use ripple_core::Warning;
use rubydex::diagnostic::Rule;
use rubydex::indexing::local_graph::LocalGraph;
use rubydex::indexing::{IndexerBackend, LanguageId, build_local_graph};
use rubydex::model::ids::NameId;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

/// A constant reference that is a real use of the constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefSite {
    pub name_id: NameId,
    /// 1-based.
    pub line: u32,
    /// Byte offset of the reference, to find the enclosing method.
    pub offset: u32,
    /// Superclass, or argument of `include`/`extend`/`prepend`.
    pub inherit: bool,
}

#[derive(Debug, Default)]
pub struct FileEntry {
    pub kind: Option<SourceKind>,
    pub refs: Vec<RefSite>,
    /// References dropped because they only name an enclosing namespace
    /// segment (`Gitlab` and `Gitlab::Foo` in `Gitlab::Foo::Bar`, or in a
    /// `class Gitlab::Foo` header).
    pub passthrough_refs: u32,
    pub facts: RubyFacts,
    pub template: TemplateFacts,
    /// Methods defined and mentioned (Ruby files and templates).
    pub methods: MethodFacts,
    /// What the file defines and mentions, for the mention filter (taken out
    /// of `methods`); `None` when unknown (method facts off or failed).
    pub mentions: Option<std::sync::Arc<ripple_core::FileMentions>>,
    /// Normalized constant names Zeitwerk would map this path to.
    pub zeitwerk_keys: Vec<String>,
    pub warnings: Vec<Warning>,
    /// The file could not be read; it contributes nothing.
    pub unreadable: bool,
}

pub struct Analysed {
    pub path: String,
    pub entry: FileEntry,
    pub local_graph: Option<LocalGraph>,
}

/// `method_level`: also collect method facts (`ruby.method_level`); off, the
/// prism pass is skipped for files with no Rails DSL, as before method scopes.
pub fn analyse(root: &Path, rel: &str, kind: SourceKind, extra_roots: &[String], method_level: bool) -> Analysed {
    let mut entry = FileEntry {
        kind: Some(kind),
        ..FileEntry::default()
    };
    let bytes = match std::fs::read(root.join(rel)) {
        Ok(b) => b,
        Err(e) => {
            entry.unreadable = true;
            entry
                .warnings
                .push(Warning::new("ruby_read_error", format!("cannot read file: {e}")).file(rel));
            return Analysed {
                path: rel.to_string(),
                entry,
                local_graph: None,
            };
        }
    };
    // Lossy decoding keeps odd encodings indexable; offsets below are into this string.
    let source = String::from_utf8_lossy(&bytes);
    let mut local_graph = None;
    match kind {
        SourceKind::Ruby => {
            let controller = paths::is_scoped_controller(rel);
            entry.zeitwerk_keys = paths::zeitwerk_keys(rel, extra_roots);
            match catch_unwind(AssertUnwindSafe(|| {
                build_local_graph(
                    rel.into(),
                    &source,
                    &LanguageId::Ruby,
                    IndexerBackend::RubyIndexer,
                )
            })) {
                Ok(lg) => {
                    collect_refs(&lg, &source, &mut entry);
                    if let Some(d) = lg
                        .diagnostics()
                        .iter()
                        .find(|d| *d.rule() == Rule::ParseError)
                    {
                        let line = line_starts(&source)
                            .partition_point(|s| *s <= d.offset().start() as usize);
                        entry.warnings.push(
                            Warning::new(
                                "ruby_parse_error",
                                format!(
                                    "line {line}: {} (indexed what could be recovered)",
                                    d.message()
                                ),
                            )
                            .file(rel),
                        );
                    }
                    local_graph = Some(lg);
                }
                Err(_) => entry.warnings.push(
                    Warning::new(
                        "ruby_indexer_panic",
                        "Rubydex panicked indexing this file; skipped",
                    )
                    .file(rel),
                ),
            }
            // One prism parse for the Rails DSL facts, actions and methods.
            match catch_unwind(AssertUnwindSafe(|| {
                if !method_level {
                    return (rails::collect(&source, controller), MethodFacts::default());
                }
                let result = ruby_prism::parse(source.as_bytes());
                let root = result.node();
                (rails::collect_root(&root, &source, controller), methods::collect(&root, source.as_bytes(), controller))
            })) {
                Ok((f, mut m)) => {
                    entry.facts = f;
                    if method_level {
                        entry.mentions = Some(std::sync::Arc::new(std::mem::take(&mut m.mentions)));
                    }
                    entry.methods = m;
                }
                Err(_) => {
                    // No method facts: mentions unknown, no scopes (whole file).
                    entry.warnings.push(
                        Warning::new("ruby_prism_panic", "Rails DSL pass panicked; skipped").file(rel),
                    )
                }
            }
            let ranges = &entry.methods.inherit_ranges;
            for r in &mut entry.refs {
                // Rubydex reports a path's reference with the whole path's span.
                r.inherit = ranges.iter().any(|&(s, e)| s <= r.offset && r.offset < e);
            }
            let mut seen = std::collections::HashSet::new();
            entry.refs.retain(|r| seen.insert((r.line, r.name_id, r.inherit)));
        }
        SourceKind::Template(lang) => {
            entry.template = templates::collect_with(&source, lang, method_level);
            if method_level {
                let mut m = methods::from_template(&entry.template);
                entry.mentions = Some(std::sync::Arc::new(std::mem::take(&mut m.mentions)));
                entry.methods = m;
            }
        }
    }
    Analysed {
        path: rel.to_string(),
        entry,
        local_graph,
    }
}

fn line_starts(source: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(
            source
                .bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        )
        .collect()
}

/// Rubydex records one `ConstantReference` per *segment* of a constant path:
/// `Gitlab::Foo::Bar` yields references named `Gitlab`, `Gitlab::Foo` and
/// `Gitlab::Foo::Bar`, each with the offset of its last segment's name. A
/// `class Gitlab::Foo` header records no reference for `Gitlab::Foo` itself
/// but does record one for the `Gitlab` prefix. A segment is therefore a
/// pass-through exactly when the source right after its offset continues the
/// path (`::` followed by an uppercase letter); `Foo::bar` (a method call on
/// `Foo`) is a real use and is kept.
///
/// Singleton-class references (`<Foo>`, synthesised for `class << self` and
/// `def self.x`) are not uses either and are dropped.
fn collect_refs(lg: &LocalGraph, source: &str, entry: &mut FileEntry) {
    let starts = line_starts(source);
    let bytes = source.as_bytes();
    let mut refs: Vec<(u32, RefSite)> = Vec::with_capacity(lg.constant_references().len());
    for r in lg.constant_references().values() {
        let synthetic = lg
            .names()
            .get(r.name_id())
            .and_then(|n| lg.strings().get(n.str()))
            .is_some_and(|s| s.as_str().starts_with('<'));
        if synthetic {
            continue;
        }
        let end = r.offset().end() as usize;
        let continues_path = bytes
            .get(end..end + 3)
            .is_some_and(|w| w[0] == b':' && w[1] == b':' && w[2].is_ascii_uppercase());
        if continues_path {
            entry.passthrough_refs += 1;
            continue;
        }
        let start = r.offset().start();
        let line = starts.partition_point(|s| *s <= start as usize) as u32;
        refs.push((
            start,
            RefSite {
                name_id: *r.name_id(),
                line,
                offset: start,
                inherit: false,
            },
        ));
    }
    // HashMap iteration order is arbitrary; sort for deterministic output.
    refs.sort_by_key(|(start, r)| (*start, r.name_id.get()));
    // Deduplicated per (line, name, inherit) once `inherit` is known.
    entry.refs = refs.into_iter().map(|(_, r)| r).collect();
}
