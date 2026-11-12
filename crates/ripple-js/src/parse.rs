//! Parses one JS/TS source text with oxc and extracts module references plus
//! HTTP call sites in a single AST walk.

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    CallExpression, Expression, ExportAllDeclaration, ExportFromDeclaration, ImportDeclaration, ImportDeclarationSpecifier,
    ImportExpression, ImportOrExportKind, TSImportEqualsDeclaration, TSImportType, TSModuleReference, TemplateLiteral,
    BinaryExpression, BinaryOperator, StringLiteral, Argument,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::SourceType;
use regex::Regex;
use std::sync::LazyLock;

use crate::callsites::{CallSiteConfig, CallSites, RouteBinding};
use crate::refs::{Ref, RefKind};
use ripple_core::JsCallSite;

/// Byte offset -> 1-based line, for one source text embedded at `base_line`.
pub struct LineIndex {
    newlines: Vec<u32>,
    /// Lines to add (a `<script>` block starts partway through a `.vue` file).
    base: u32,
}

impl LineIndex {
    pub fn new(src: &str, base: u32) -> Self {
        let newlines = src.bytes().enumerate().filter(|&(_, b)| b == b'\n').map(|(i, _)| i as u32).collect();
        Self { newlines, base }
    }

    pub fn line(&self, offset: u32) -> u32 {
        self.base + self.newlines.partition_point(|&n| n < offset) as u32 + 1
    }
}

/// References and call sites collected from one parsed block.
#[derive(Default)]
pub struct Collected {
    pub refs: Vec<Ref>,
    pub calls: Vec<JsCallSite>,
}

/// The oxc source type for a path, or `None` for extensions we do not parse.
pub fn source_type_for(path: &str) -> Option<SourceType> {
    let ext = path.rsplit('.').next()?;
    Some(match ext {
        "js" | "mjs" | "cjs" | "jsx" => SourceType::from_path(path).ok()?.with_jsx(true),
        "ts" | "mts" | "cts" | "tsx" => SourceType::from_path(path).ok()?,
        _ => return None,
    })
}

/// Parses `src` and appends what it finds to `out`. Returns the first syntax
/// error message, if any. The AST oxc recovers is still walked.
pub fn parse_block(src: &str, source_type: SourceType, base_line: u32, file: &str, cfg: &CallSiteConfig, out: &mut Collected) -> Option<String> {
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, src, source_type).parse();
    let error = ret.diagnostics.first().map(|d| d.to_string());
    let lines = LineIndex::new(src, base_line);
    if ret.fatal_error {
        // oxc gives up with an empty AST; salvage module references textually
        // so a half-typed file does not silently drop out of the graph.
        out.refs.extend(scan_imports(src, &lines));
        return error;
    }
    let mut walker = Walker { lines: &lines, refs: Vec::new(), sites: CallSites::new(cfg, file, &lines) };
    walker.visit_program(&ret.program);
    out.refs.extend(walker.refs);
    out.calls.extend(walker.sites.out);
    error
}

struct Walker<'c> {
    lines: &'c LineIndex,
    refs: Vec<Ref>,
    sites: CallSites<'c>,
}

impl Walker<'_> {
    fn import(&mut self, offset: u32, specifier: &str, dynamic: bool, type_only: bool) {
        self.refs.push(Ref {
            line: self.lines.line(offset),
            kind: RefKind::Import { specifier: specifier.to_string(), dynamic, type_only },
        });
    }

    /// `import(expr)` / `require(expr)` argument: a literal, or a template
    /// with holes which bundlers turn into a glob.
    fn dynamic_target(&mut self, offset: u32, expr: &Expression<'_>, dynamic: bool) {
        match expr.get_inner_expression() {
            Expression::StringLiteral(s) => self.import(offset, &s.value, dynamic, false),
            Expression::TemplateLiteral(t) if t.expressions.is_empty() => {
                if let Some(q) = t.quasis.first() {
                    let text = q.value.cooked.as_ref().map_or(q.value.raw.as_str(), |c| c.as_str());
                    self.import(offset, text, dynamic, false);
                }
            }
            Expression::TemplateLiteral(t) => {
                let mut pattern = String::new();
                for (i, q) in t.quasis.iter().enumerate() {
                    pattern.push_str(q.value.cooked.as_ref().map_or(q.value.raw.as_str(), |c| c.as_str()));
                    if i < t.expressions.len() {
                        pattern.push('*');
                    }
                }
                // Only relative templates are expanded; `${x}` with a bare prefix could be anything.
                if pattern.starts_with("./") || pattern.starts_with("../") {
                    self.refs.push(Ref { line: self.lines.line(offset), kind: RefKind::Glob { patterns: vec![pattern] } });
                }
            }
            _ => {}
        }
    }

    fn special_call(&mut self, call: &CallExpression<'_>) {
        let offset = call.span.start;
        match call.callee.get_inner_expression() {
            Expression::Identifier(id) if id.name == "require" && call.arguments.len() == 1 => {
                if let Some(Argument::StringLiteral(s)) = call.arguments.first() {
                    self.import(offset, &s.value, false, false);
                } else if let Some(Argument::TemplateLiteral(t)) = call.arguments.first() {
                    if t.expressions.is_empty() {
                        if let Some(q) = t.quasis.first() {
                            self.import(offset, q.value.raw.as_str(), false, false);
                        }
                    }
                }
            }
            Expression::StaticMemberExpression(m) => {
                let prop = m.property.name.as_str();
                match (m.object.get_inner_expression(), prop) {
                    (Expression::Identifier(id), "context") if id.name == "require" => {
                        let Some(Argument::StringLiteral(dir)) = call.arguments.first() else { return };
                        let recursive = match call.arguments.get(1) {
                            Some(Argument::BooleanLiteral(b)) => b.value,
                            _ => true,
                        };
                        let filter = match call.arguments.get(2) {
                            Some(Argument::RegExpLiteral(r)) => r.raw.as_ref().and_then(|raw| regex_source(raw.as_str())),
                            _ => None,
                        };
                        self.refs.push(Ref {
                            line: self.lines.line(offset),
                            kind: RefKind::Context { dir: dir.value.to_string(), recursive, filter },
                        });
                    }
                    (Expression::ImportMeta(_), "glob" | "globEager") => {
                        let mut patterns = Vec::new();
                        match call.arguments.first() {
                            Some(Argument::StringLiteral(s)) => patterns.push(s.value.to_string()),
                            Some(Argument::ArrayExpression(a)) => {
                                for el in &a.elements {
                                    if let Some(Expression::StringLiteral(s)) = el.as_expression() {
                                        patterns.push(s.value.to_string());
                                    }
                                }
                            }
                            _ => {}
                        }
                        if !patterns.is_empty() {
                            self.refs.push(Ref { line: self.lines.line(offset), kind: RefKind::Glob { patterns } });
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// Textual import scan used only when the parser could not produce an AST.
fn scan_imports(src: &str, lines: &LineIndex) -> Vec<Ref> {
    static PATTERNS: LazyLock<Vec<(Regex, bool)>> = LazyLock::new(|| {
        let p = |re: &str, dynamic| (Regex::new(re).expect("static regex"), dynamic);
        vec![
            p(r#"(?m)^[ \t]*(?:import|export)\b[^'"\n;]*?\bfrom\s*['"]([^'"]+)['"]"#, false),
            p(r#"(?m)^[ \t]*import\s*['"]([^'"]+)['"]"#, false),
            p(r#"\brequire\(\s*['"]([^'"]+)['"]\s*\)"#, false),
            p(r#"\bimport\(\s*['"]([^'"]+)['"]\s*\)"#, true),
        ]
    });
    let mut refs = Vec::new();
    for (re, dynamic) in PATTERNS.iter() {
        for caps in re.captures_iter(src) {
            let whole = caps.get(0).expect("group 0");
            let type_only = {
                let t = whole.as_str().trim_start();
                t.starts_with("import type ") || t.starts_with("export type ")
            };
            refs.push(Ref {
                line: lines.line(whole.start() as u32),
                kind: RefKind::Import { specifier: caps[1].to_string(), dynamic: *dynamic, type_only },
            });
        }
    }
    refs
}

/// `/^\.\/.*\.js$/i` -> `^\.\/.*\.js$` (flags dropped; `regex` handles the rest).
fn regex_source(raw: &str) -> Option<String> {
    let end = raw.rfind('/')?;
    (end > 0).then(|| raw[1..end].to_string())
}

fn kind_is_type(k: ImportOrExportKind) -> bool {
    k == ImportOrExportKind::Type
}

impl<'a> Visit<'a> for Walker<'_> {
    fn visit_import_declaration(&mut self, it: &ImportDeclaration<'a>) {
        let all_type = kind_is_type(it.import_kind)
            || it.specifiers.as_ref().is_some_and(|specs| {
                !specs.is_empty() && specs.iter().all(|s| matches!(s, ImportDeclarationSpecifier::ImportSpecifier(i) if kind_is_type(i.import_kind)))
            });
        self.import(it.span.start, &it.source.value, false, all_type);
        if let Some(specs) = &it.specifiers {
            let bindings = specs.iter().map(|s| match s {
                ImportDeclarationSpecifier::ImportSpecifier(i) => RouteBinding::Named {
                    imported: i.imported.name().to_string(),
                    local: i.local.name.to_string(),
                },
                ImportDeclarationSpecifier::ImportDefaultSpecifier(d) => RouteBinding::Namespace(d.local.name.to_string()),
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(n) => RouteBinding::Namespace(n.local.name.to_string()),
            });
            self.sites.note_routes_import(&it.source.value, bindings);
        }
    }

    /// `export { a } from "x"`. (oxc 0.152 splits this from local `export { a }`.)
    fn visit_export_from_declaration(&mut self, it: &ExportFromDeclaration<'a>) {
        self.import(it.span.start, &it.source.value, false, kind_is_type(it.export_kind));
    }

    fn visit_export_all_declaration(&mut self, it: &ExportAllDeclaration<'a>) {
        self.import(it.span.start, &it.source.value, false, kind_is_type(it.export_kind));
    }

    fn visit_ts_import_equals_declaration(&mut self, it: &TSImportEqualsDeclaration<'a>) {
        if let TSModuleReference::ExternalModuleReference(r) = &it.module_reference {
            self.import(it.span.start, &r.expression.value, false, kind_is_type(it.import_kind));
        }
    }

    fn visit_ts_import_type(&mut self, it: &TSImportType<'a>) {
        self.import(it.span.start, &it.source.value, false, true);
        walk::walk_ts_import_type(self, it);
    }

    fn visit_import_expression(&mut self, it: &ImportExpression<'a>) {
        self.dynamic_target(it.span.start, &it.source, true);
        walk::walk_import_expression(self, it);
    }

    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        self.special_call(it);
        self.sites.on_call(it);
        walk::walk_call_expression(self, it);
    }

    fn visit_string_literal(&mut self, it: &StringLiteral<'a>) {
        self.sites.on_string(it.span.start, &it.value);
    }

    fn visit_template_literal(&mut self, it: &TemplateLiteral<'a>) {
        // Re-wrap as an expression-free check: the concat logic takes `Expression`,
        // so reconstruct only when the template could plausibly be a URL.
        if it.quasis.first().is_some_and(|q| {
            let raw = q.value.raw.as_str();
            raw.starts_with('/') || raw.starts_with("http") || raw.is_empty()
        }) {
            self.sites.on_template_literal(it);
        }
        walk::walk_template_literal(self, it);
    }

    fn visit_binary_expression(&mut self, it: &BinaryExpression<'a>) {
        if it.operator == BinaryOperator::Addition {
            self.sites.on_binary(it);
        }
        walk::walk_binary_expression(self, it);
    }
}
