//! Parses one JS/TS source text with oxc and extracts module references plus
//! HTTP call sites in a single AST walk.

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, ArrowFunctionBody, ArrowFunctionExpression, AssignmentExpression, BinaryExpression, BinaryOperator, BindingPattern,
    CallExpression, ConditionalExpression, ExportAllDeclaration, ExportDeclaration, ExportDefaultDeclaration, ExportFromDeclaration,
    ExportSpecifier, Expression, ExpressionStatement, Function, IdentifierReference, IfStatement, ImportDeclaration,
    ImportDeclarationSpecifier, ImportExpression, ImportOrExportKind, JSXAttributeItem, JSXAttributeName, JSXAttributeValue,
    JSXExpressionContainer, JSXOpeningElement, LogicalExpression, LogicalOperator, NewExpression, ObjectExpression, ObjectProperty, ObjectPropertyKind,
    ReturnStatement, StringLiteral, SwitchCase, SwitchStatement, TSImportEqualsDeclaration, TSImportType, TSModuleReference, TemplateLiteral,
    UnaryExpression, VariableDeclarator,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};
use oxc_syntax::scope::ScopeFlags;
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

use crate::callsites::{CallKind, CallSiteConfig, CallSites, RouteBinding, write_callee};
use crate::linkctx::{self, CallUse, Class, Frame};
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
    /// Parallel to `calls`: `Some(context)` when the use of the URL could not
    /// be classified as navigation or data (then `link` stays false).
    pub unclassified: Vec<Option<String>>,
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
    let mut walker = Walker {
        src,
        lines: &lines,
        refs: Vec::new(),
        sites: CallSites::new(cfg, file, &lines),
        frames: Vec::new(),
        classes: Vec::new(),
        uses: HashMap::new(),
        data_verbs: HashMap::new(),
        scratch: String::new(),
    };
    walker.visit_program(&ret.program);
    let unclassified = walker.finish();
    out.refs.extend(walker.refs);
    out.calls.extend(walker.sites.out);
    out.unclassified.extend(unclassified);
    error
}

struct Walker<'c, 'a> {
    src: &'c str,
    lines: &'c LineIndex,
    refs: Vec<Ref>,
    sites: CallSites<'c>,
    /// Syntactic context of the expression being visited (see `linkctx`).
    frames: Vec<Frame<'a>>,
    /// Parallel to `sites.out`.
    classes: Vec<Class<'a>>,
    /// Identifier name -> how it is used anywhere in the file, and the first
    /// unclassified use.
    uses: HashMap<&'a str, (u8, Option<Frame<'a>>)>,
    /// Identifier name -> the verb all its data uses agree on (`post(url)`),
    /// `None` when they name none or disagree.
    data_verbs: HashMap<&'a str, Option<&'static str>>,
    scratch: String,
}

impl<'a> Walker<'_, 'a> {
    fn with<F: FnOnce(&mut Self)>(&mut self, frame: Option<Frame<'a>>, f: F) {
        let pushed = frame.is_some();
        if let Some(fr) = frame {
            self.frames.push(fr);
        }
        f(self);
        if pushed {
            self.frames.pop();
        }
    }

    /// Classifies the call sites emitted since `before`.
    fn classify_new(&mut self, before: usize, http: bool) {
        let n = self.sites.out.len();
        if n == before {
            return;
        }
        let class = if http { Class::Data } else { linkctx::resolve(&self.frames) };
        if matches!(class, Class::Data) && !http {
            // `form.patch(Routes.checkout_path())`: the verb comes from the context.
            if let Some(verb) = linkctx::data_verb(&self.frames) {
                for site in &mut self.sites.out[before..] {
                    site.method.get_or_insert_with(|| verb.to_string());
                }
            }
        }
        self.classes.resize(n, class);
    }

    /// Settles variable-held URLs from the variable's uses, sets `link`, and
    /// returns the unclassified context per site.
    fn finish(&mut self) -> Vec<Option<String>> {
        debug_assert_eq!(self.classes.len(), self.sites.out.len());
        let mut labels = Vec::with_capacity(self.classes.len());
        for (site, class) in self.sites.out.iter_mut().zip(&self.classes) {
            let (link, label) = match *class {
                Class::Data => (false, None),
                Class::Link => (true, None),
                Class::Unknown(f) => (false, Some(linkctx::label(f, self.src))),
                Class::Var(name) => match self.uses.get(name) {
                    None => (false, Some("var:unused".to_string())),
                    Some((bits, _)) if bits & linkctx::USE_DATA != 0 => {
                        if let Some(Some(verb)) = self.data_verbs.get(name) {
                            site.method.get_or_insert_with(|| verb.to_string());
                        }
                        (false, None)
                    }
                    Some((bits, _)) if *bits == linkctx::USE_LINK => (true, None),
                    Some((_, first)) => (false, Some(format!("var:{}", linkctx::label(*first, self.src)))),
                },
            };
            site.link = link;
            labels.push(label);
        }
        labels
    }

    fn callee_text(&mut self, callee: &Expression<'_>) -> Option<String> {
        self.scratch.clear();
        write_callee(callee, &mut self.scratch).then(|| self.scratch.clone())
    }

    /// The frame for the arguments of a call that is not an HTTP call.
    fn arg_frame(&mut self, callee: &Expression<'_>, args: &[Argument<'_>]) -> Option<Frame<'a>> {
        let Some(text) = self.callee_text(callee) else { return Some(Frame::Arg(callee.span())) };
        match linkctx::call_use(&text) {
            CallUse::Link => match args.get(1).filter(|_| text.ends_with("visit")).and_then(non_get_method_option) {
                // `router.visit(url, { method: "post", data })` submits data.
                Some(verb) => Some(Frame::Data(verb)),
                None => Some(Frame::Link),
            },
            CallUse::Data(verb) => Some(Frame::Data(verb)),
            CallUse::Compare => Some(Frame::Compare),
            CallUse::PassThrough => None,
            CallUse::Unknown => Some(Frame::Arg(callee.span())),
        }
    }

    fn visit_args(&mut self, frame: Option<Frame<'a>>, args: &oxc_allocator::Vec<'a, Argument<'a>>) {
        self.with(frame, |w| w.visit_arguments(args));
    }
}

/// `{ method: "post" }` (any literal verb but GET: `Some(Some(POST))`) or a
/// non-literal method (`Some(None)`); `None` for GET or no method.
fn non_get_method_option(arg: &Argument<'_>) -> Option<Option<&'static str>> {
    let Argument::ObjectExpression(obj) = arg else { return None };
    obj.properties.iter().find_map(|p| match p {
        ObjectPropertyKind::ObjectProperty(p) if p.key.static_name().is_some_and(|k| k == "method") => match p.value.get_inner_expression() {
            Expression::StringLiteral(s) if s.value.eq_ignore_ascii_case("get") => None,
            Expression::StringLiteral(s) => Some(linkctx::verb_of_method(&s.value)),
            _ => Some(None),
        },
        _ => None,
    })
}

impl Walker<'_, '_> {
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

impl<'a> Visit<'a> for Walker<'_, 'a> {
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
        let before = self.sites.out.len();
        let kind = self.sites.on_call(it);
        self.classify_new(before, kind == CallKind::Http);
        self.visit_expression(&it.callee);
        if let Some(t) = &it.type_arguments {
            self.visit_ts_type_parameter_instantiation(t);
        }
        let frame = match kind {
            CallKind::Http => Some(Frame::Data(None)),
            CallKind::RouteHelper => Some(Frame::Ignore),
            CallKind::Other => self.arg_frame(&it.callee, &it.arguments),
        };
        self.visit_args(frame, &it.arguments);
    }

    fn visit_new_expression(&mut self, it: &NewExpression<'a>) {
        self.visit_expression(&it.callee);
        if let Some(t) = &it.type_arguments {
            self.visit_ts_type_parameter_instantiation(t);
        }
        let frame = match self.callee_text(&it.callee).as_deref() {
            Some("URL") => None,
            Some("Request" | "EventSource" | "WebSocket") => Some(Frame::Data(None)),
            Some("DirectUpload") => Some(Frame::Data(Some("POST"))),
            _ => Some(Frame::Arg(it.callee.span())),
        };
        self.visit_args(frame, &it.arguments);
    }

    fn visit_string_literal(&mut self, it: &StringLiteral<'a>) {
        let before = self.sites.out.len();
        self.sites.on_string(it.span.start, &it.value);
        self.classify_new(before, false);
    }

    fn visit_template_literal(&mut self, it: &TemplateLiteral<'a>) {
        // Re-wrap as an expression-free check: the concat logic takes `Expression`,
        // so reconstruct only when the template could plausibly be a URL.
        if it.quasis.first().is_some_and(|q| {
            let raw = q.value.raw.as_str();
            raw.starts_with('/') || raw.starts_with("http") || raw.is_empty()
        }) {
            let before = self.sites.out.len();
            self.sites.on_template_literal(it);
            self.classify_new(before, false);
        }
        walk::walk_template_literal(self, it);
    }

    fn visit_binary_expression(&mut self, it: &BinaryExpression<'a>) {
        if it.operator == BinaryOperator::Addition {
            let before = self.sites.out.len();
            self.sites.on_binary(it);
            self.classify_new(before, false);
            walk::walk_binary_expression(self, it);
        } else if it.operator.is_equality() {
            self.with(Some(Frame::Compare), |w| walk::walk_binary_expression(w, it));
        } else {
            self.with(Some(Frame::Ignore), |w| walk::walk_binary_expression(w, it));
        }
    }

    fn visit_identifier_reference(&mut self, it: &IdentifierReference<'a>) {
        let (bits, frame) = linkctx::use_bits(&self.frames);
        if bits != 0 {
            let e = self.uses.entry(it.name.as_str()).or_insert((0, None));
            e.0 |= bits;
            if e.1.is_none() {
                e.1 = frame;
            }
            if bits == linkctx::USE_DATA {
                let verb = linkctx::data_verb(&self.frames);
                self.data_verbs.entry(it.name.as_str()).and_modify(|v| if *v != verb { *v = None }).or_insert(verb);
            }
        }
    }

    fn visit_jsx_opening_element(&mut self, it: &JSXOpeningElement<'a>) {
        self.visit_jsx_element_name(&it.name);
        if let Some(t) = &it.type_arguments {
            self.visit_ts_type_parameter_instantiation(t);
        }
        // `<Link href=.. method="post">`, `<form action=.. method="post">` submit data.
        // `Some(verb)` when the element submits with a non-GET method.
        let submits: Option<Option<&'static str>> = it.attributes.iter().find_map(|a| match a {
            JSXAttributeItem::Attribute(a) if matches!(&a.name, JSXAttributeName::Identifier(n) if n.name == "method") => match &a.value {
                Some(JSXAttributeValue::StringLiteral(s)) if s.value.eq_ignore_ascii_case("get") => None,
                Some(JSXAttributeValue::StringLiteral(s)) => Some(linkctx::verb_of_method(&s.value)),
                Some(_) => Some(None),
                None => None,
            },
            _ => None,
        });
        for item in &it.attributes {
            let JSXAttributeItem::Attribute(attr) = item else {
                self.visit_jsx_attribute_item(item);
                continue;
            };
            let frame = match &attr.name {
                JSXAttributeName::Identifier(n) if linkctx::is_nav_attr(&n.name) => {
                    match submits {
                        Some(verb) => Frame::Data(verb),
                        None => Frame::Link,
                    }
                }
                JSXAttributeName::NamespacedName(n) if n.name.name == "href" => Frame::Link,
                name => Frame::JsxAttr(name.span()),
            };
            self.with(Some(frame), |w| match &attr.value {
                Some(JSXAttributeValue::ExpressionContainer(c)) => w.visit_jsx_expression(&c.expression),
                Some(v) => w.visit_jsx_attribute_value(v),
                None => {}
            });
        }
    }

    /// Reached only for JSX children (attribute values are visited above).
    fn visit_jsx_expression_container(&mut self, it: &JSXExpressionContainer<'a>) {
        self.with(Some(Frame::JsxChild), |w| walk::walk_jsx_expression_container(w, it));
    }

    /// `{ url, method: "POST" }` / `{ path, headers }` describes a request
    /// (Pact fixtures, request configs), wherever it ends up.
    fn visit_object_expression(&mut self, it: &ObjectExpression<'a>) {
        let request_like = it.properties.iter().any(|p| match p {
            ObjectPropertyKind::ObjectProperty(p) => p.key.static_name().is_some_and(|k| matches!(k.as_ref(), "method" | "headers" | "body")),
            _ => false,
        });
        // `{ method: "GET", url }` names its verb.
        let verb = it.properties.iter().find_map(|p| match p {
            ObjectPropertyKind::ObjectProperty(p) if p.key.static_name().is_some_and(|k| k == "method") => match p.value.get_inner_expression() {
                Expression::StringLiteral(s) => linkctx::verb_of_method(&s.value),
                _ => None,
            },
            _ => None,
        });
        self.with(request_like.then_some(Frame::Data(verb)), |w| walk::walk_object_expression(w, it));
    }

    fn visit_object_property(&mut self, it: &ObjectProperty<'a>) {
        // Keys are visited in the enclosing context (`{ "/api/x": fixture }`).
        self.visit_property_key(&it.key);
        let frame = match it.key.static_name() {
            Some(k) if linkctx::is_nav_prop(&k) => Frame::NavProp,
            _ => Frame::Prop(it.key.span()),
        };
        self.with(Some(frame), |w| w.visit_expression(&it.value));
    }

    fn visit_variable_declarator(&mut self, it: &VariableDeclarator<'a>) {
        self.with(Some(Frame::Ignore), |w| w.visit_binding_pattern(&it.id));
        if let Some(t) = &it.type_annotation {
            self.visit_ts_type_annotation(t);
        }
        if let Some(init) = &it.init {
            let frame = match &it.id {
                BindingPattern::BindingIdentifier(id) => Frame::Var(id.name.as_str()),
                other => Frame::Assign(other.span()),
            };
            self.with(Some(frame), |w| w.visit_expression(init));
        }
    }

    fn visit_assignment_expression(&mut self, it: &AssignmentExpression<'a>) {
        self.with(Some(Frame::Ignore), |w| w.visit_assignment_target(&it.left));
        let span = it.left.span();
        let target = self.src.get(span.start as usize..span.end as usize).unwrap_or("");
        let frame = if linkctx::is_location_target(target) {
            Frame::Link
        } else if let Some(id) = it.left.get_identifier_name().filter(|_| linkctx::is_identifier(target.trim())) {
            Frame::Var(id)
        } else {
            Frame::Assign(span)
        };
        self.with(Some(frame), |w| w.visit_expression(&it.right));
    }

    fn visit_return_statement(&mut self, it: &ReturnStatement<'a>) {
        self.with(Some(Frame::Return), |w| walk::walk_return_statement(w, it));
    }

    fn visit_expression_statement(&mut self, it: &ExpressionStatement<'a>) {
        self.with(Some(Frame::Stmt), |w| walk::walk_expression_statement(w, it));
    }

    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        self.with(Some(Frame::Func), |w| walk::walk_function(w, it, flags));
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        let expression_body = !matches!(it.body, ArrowFunctionBody::FunctionBody(_));
        self.frames.push(Frame::Func);
        self.with(expression_body.then_some(Frame::Return), |w| walk::walk_arrow_function_expression(w, it));
        self.frames.pop();
    }

    fn visit_if_statement(&mut self, it: &IfStatement<'a>) {
        self.with(Some(Frame::Ignore), |w| w.visit_expression(&it.test));
        self.visit_statement(&it.consequent);
        if let Some(alt) = &it.alternate {
            self.visit_statement(alt);
        }
    }

    fn visit_switch_statement(&mut self, it: &SwitchStatement<'a>) {
        self.with(Some(Frame::Compare), |w| w.visit_expression(&it.discriminant));
        self.visit_switch_cases(&it.cases);
    }

    fn visit_switch_case(&mut self, it: &SwitchCase<'a>) {
        if let Some(test) = &it.test {
            self.with(Some(Frame::Compare), |w| w.visit_expression(test));
        }
        self.visit_statements(&it.consequent);
    }

    fn visit_conditional_expression(&mut self, it: &ConditionalExpression<'a>) {
        self.with(Some(Frame::Ignore), |w| w.visit_expression(&it.test));
        self.visit_expression(&it.consequent);
        self.visit_expression(&it.alternate);
    }

    fn visit_logical_expression(&mut self, it: &LogicalExpression<'a>) {
        // `url && <a href={url}>`: the left side of `&&` is a test.
        let left = (it.operator == LogicalOperator::And).then_some(Frame::Ignore);
        self.with(left, |w| w.visit_expression(&it.left));
        self.visit_expression(&it.right);
    }

    fn visit_unary_expression(&mut self, it: &UnaryExpression<'a>) {
        self.with(Some(Frame::Ignore), |w| walk::walk_unary_expression(w, it));
    }

    fn visit_export_declaration(&mut self, it: &ExportDeclaration<'a>) {
        self.with(Some(Frame::Export), |w| walk::walk_export_declaration(w, it));
    }

    fn visit_export_default_declaration(&mut self, it: &ExportDefaultDeclaration<'a>) {
        self.with(Some(Frame::Export), |w| walk::walk_export_default_declaration(w, it));
    }

    /// `export { url }`: used by other files, so not decidable here.
    fn visit_export_specifier(&mut self, it: &ExportSpecifier<'a>) {
        let e = self.uses.entry(it.local.name().as_str()).or_insert((0, None));
        e.0 |= linkctx::USE_OTHER;
        e.1.get_or_insert(Frame::Export);
    }
}
