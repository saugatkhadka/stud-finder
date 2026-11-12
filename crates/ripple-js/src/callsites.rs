//! HTTP call-site detection: which calls look like network requests, and what
//! URL / verb they carry. Purely syntactic (no scope analysis): `axios` means an
//! identifier spelled `axios`, which is right far more often than it is wrong.

use oxc_ast::ast::{Argument, BinaryExpression, BinaryOperator, CallExpression, Expression, ObjectPropertyKind, PropertyKey, TemplateLiteral};
use oxc_span::GetSpan;
use ripple_core::JsCallSite;
use std::collections::{HashMap, HashSet};

use crate::parse::LineIndex;

/// The slice of `ripple.yml` that call-site detection depends on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallSiteConfig {
    /// `boundary.http_callees`.
    pub callees: Vec<String>,
    /// `boundary.url_prefixes`.
    pub prefixes: Vec<String>,
}

const VERBS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
/// Identifiers whose `.get/.post/...` methods are HTTP calls out of the box.
const HTTP_ROOTS: [&str; 7] = ["axios", "request", "superagent", "$", "jQuery", "got", "ky"];
/// Roots that wrap a global object and are stripped before classification.
const GLOBAL_PREFIXES: [&str; 3] = ["window.", "globalThis.", "self."];

/// Collects call sites while the visitor walks a file.
pub struct CallSites<'c> {
    cfg: &'c CallSiteConfig,
    file: &'c str,
    lines: &'c LineIndex,
    /// Local names bound to a js-routes module (`import Routes from "./routes"`).
    route_namespaces: HashSet<String>,
    /// Local name -> helper name, for `import { user_path } from "./routes"`.
    route_functions: HashMap<String, String>,
    /// Source offsets already accounted for by a call site, so the generic
    /// "string that looks like an API path" pass does not report them again.
    consumed: HashSet<u32>,
    scratch: String,
    pub out: Vec<JsCallSite>,
}

impl<'c> CallSites<'c> {
    pub fn new(cfg: &'c CallSiteConfig, file: &'c str, lines: &'c LineIndex) -> Self {
        Self {
            cfg,
            file,
            lines,
            route_namespaces: HashSet::new(),
            route_functions: HashMap::new(),
            consumed: HashSet::new(),
            scratch: String::new(),
            out: Vec::new(),
        }
    }

    /// Registers the bindings of an import from a js-routes style module.
    pub fn note_routes_import(&mut self, spec: &str, bindings: impl IntoIterator<Item = RouteBinding>) {
        if !is_routes_module(spec) {
            return;
        }
        for b in bindings {
            match b {
                RouteBinding::Namespace(local) => {
                    self.route_namespaces.insert(local);
                }
                RouteBinding::Named { imported, local } => {
                    if imported == "Routes" {
                        self.route_namespaces.insert(local);
                    } else if is_route_helper_name(&imported) {
                        self.route_functions.insert(local, imported);
                    }
                }
            }
        }
    }

    /// Handles one call; says whether it is a route helper or an HTTP call
    /// (sites it emits for an HTTP call consume the response: never links).
    pub fn on_call(&mut self, call: &CallExpression<'_>) -> CallKind {
        if self.try_route_helper(call, None, call.span.start) {
            return CallKind::RouteHelper;
        }
        let mut text = std::mem::take(&mut self.scratch);
        text.clear();
        let mut kind = CallKind::Other;
        if write_callee(&call.callee, &mut text) {
            if self.classify(&text).is_some() {
                kind = CallKind::Http;
            }
            self.classify_and_emit(call, &text);
        }
        self.scratch = text;
        kind
    }

    /// A `"/api/..."`-looking string literal that is not an argument to a recognised call.
    pub fn on_string(&mut self, start: u32, value: &str) {
        if self.consumed.contains(&start) {
            return;
        }
        if let Some(url) = normalize_url(value).filter(|u| self.looks_like_api(u)) {
            self.push(start, None, url, "<literal>".to_string(), false);
        }
    }

    pub fn on_template_literal(&mut self, t: &TemplateLiteral<'_>) {
        let mut leaves = vec![t.span.start];
        let raw = concat_template(t, &mut leaves).0;
        self.on_literal_like(t.span.start, raw, leaves);
    }

    pub fn on_binary(&mut self, b: &BinaryExpression<'_>) {
        let mut leaves = Vec::new();
        let (raw, has_literal) = concat_binary(b, &mut leaves);
        if has_literal {
            self.on_literal_like(b.span.start, raw, leaves);
        }
    }

    fn on_literal_like(&mut self, start: u32, raw: String, leaves: Vec<u32>) {
        if self.consumed.contains(&start) {
            return;
        }
        if let Some(url) = normalize_url(&raw).filter(|u| self.looks_like_api(u)) {
            self.consumed.extend(leaves);
            self.consumed.insert(start);
            self.push(start, None, url, "<literal>".to_string(), false);
        }
    }

    fn looks_like_api(&self, url: &str) -> bool {
        if url.len() >= 300 || url.contains(char::is_whitespace) {
            return false;
        }
        let tail = url
            .strip_prefix("/api/")
            .or_else(|| self.cfg.prefixes.iter().filter(|p| !p.is_empty()).find_map(|p| url.strip_prefix(p.as_str())));
        // `/api/:param` alone would match every one-segment route; require real text.
        tail.is_some_and(|t| t.replace(":param", "").chars().any(char::is_alphanumeric))
    }

    fn push(&mut self, offset: u32, method: Option<&str>, url: String, callee: String, route_helper: bool) {
        self.out.push(JsCallSite {
            file: self.file.to_string(),
            line: self.lines.line(offset),
            method: method.map(str::to_string),
            url,
            callee,
            route_helper,
            link: false,
        });
    }

    /// `Routes.user_path(...)` / `user_path(...)` imported from a routes module.
    /// `method` comes from an enclosing `request({ method, url: Routes.x_path() })`.
    fn try_route_helper(&mut self, call: &CallExpression<'_>, method: Option<&str>, key: u32) -> bool {
        if self.consumed.contains(&key) && method.is_none() {
            return true;
        }
        let (callee, helper) = match call.callee.get_inner_expression() {
            Expression::StaticMemberExpression(m) => {
                let Expression::Identifier(obj) = m.object.get_inner_expression() else { return false };
                let name = m.property.name.as_str();
                let is_ns = obj.name == "Routes" || self.route_namespaces.contains(obj.name.as_str());
                if !is_ns || !is_route_helper_name(name) {
                    return false;
                }
                (format!("{}.{}", obj.name, name), name.to_string())
            }
            Expression::Identifier(id) => match self.route_functions.get(id.name.as_str()) {
                Some(imported) => (id.name.to_string(), imported.clone()),
                None => return false,
            },
            _ => return false,
        };
        self.consumed.insert(key);
        self.push(call.span.start, method, helper, callee, true);
        true
    }

    fn classify_and_emit(&mut self, call: &CallExpression<'_>, text: &str) {
        let Some(kind) = self.classify(text) else { return };
        let Some(first) = call.arguments.first().and_then(arg_expr) else { return };

        let mut method: Option<String> = kind.verb.map(str::to_string);
        let mut url_expr: Option<&Expression<'_>> = None;

        match first.get_inner_expression() {
            // apiRequest("POST", "v1/statuses"): a leading verb, then the URL.
            Expression::StringLiteral(s) if verb_of(&s.value).is_some() && call.arguments.len() >= 2 => {
                method = method.or_else(|| verb_of(&s.value).map(str::to_string));
                url_expr = call.arguments.get(1).and_then(arg_expr);
            }
            // axios({ url, method }) / $.ajax({ url, type }) / request({ method, url })
            Expression::ObjectExpression(obj) => {
                for prop in &obj.properties {
                    let ObjectPropertyKind::ObjectProperty(p) = prop else { continue };
                    match property_name(&p.key).as_deref() {
                        Some("url" | "uri") => url_expr = Some(&p.value),
                        Some("method" | "type") if method.is_none() => method = literal_verb(&p.value),
                        _ => {}
                    }
                }
            }
            _ => url_expr = Some(first),
        }
        // fetch(url, { method }) / axios(url, { method }) / $.ajax(url, { type })
        if method.is_none() {
            if let Some(Expression::ObjectExpression(obj)) = call.arguments.get(1).and_then(arg_expr).map(Expression::get_inner_expression) {
                for prop in &obj.properties {
                    if let ObjectPropertyKind::ObjectProperty(p) = prop {
                        if matches!(property_name(&p.key).as_deref(), Some("method" | "type")) {
                            method = literal_verb(&p.value);
                        }
                    }
                }
            }
        }

        let Some(expr) = url_expr else { return };
        // A route helper as the URL: record it with the verb we learned here.
        if let Expression::CallExpression(inner) = expr.get_inner_expression() {
            self.try_route_helper(inner, method.as_deref(), inner.span.start);
            return;
        }
        let mut leaves = Vec::new();
        let Some(url) = url_of(expr, &mut leaves).and_then(|raw| normalize_url(&raw)) else { return };
        self.consumed.extend(leaves);
        self.consumed.insert(expr.span().start);
        self.push(expr.span().start, method.as_deref(), url, text.to_string(), false);
    }

    /// Decides whether `text` (the callee as written) is an HTTP call and what
    /// verb it implies.
    fn classify(&self, text: &str) -> Option<Callee> {
        let text = GLOBAL_PREFIXES.iter().find_map(|p| text.strip_prefix(p)).unwrap_or(text);
        if text == "fetch" {
            return Some(Callee { verb: None });
        }
        if let Some((root, method)) = text.split_once('.') {
            if HTTP_ROOTS.contains(&root) && !method.contains(['.', '(']) {
                let verb = match method {
                    "get" | "getJSON" => Some("GET"),
                    "post" => Some("POST"),
                    "put" => Some("PUT"),
                    "patch" => Some("PATCH"),
                    "delete" | "del" => Some("DELETE"),
                    "head" => Some("HEAD"),
                    "options" => Some("OPTIONS"),
                    "ajax" | "request" => None,
                    _ => return self.configured(text),
                };
                return Some(Callee { verb });
            }
        }
        if text == "axios" || text == "request" {
            return Some(Callee { verb: None });
        }
        self.configured(text)
    }

    fn configured(&self, text: &str) -> Option<Callee> {
        self.cfg
            .callees
            .iter()
            .any(|entry| text == entry || text.strip_suffix(entry.as_str()).is_some_and(|rest| rest.ends_with('.')))
            .then(|| Callee { verb: verb_from_name(text) })
    }
}

struct Callee {
    verb: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallKind {
    RouteHelper,
    Http,
    Other,
}

pub enum RouteBinding {
    /// `import Routes from` / `import * as Routes from`.
    Namespace(String),
    Named { imported: String, local: String },
}

fn is_routes_module(spec: &str) -> bool {
    let stem = spec.rsplit('/').next().unwrap_or(spec);
    let stem = stem.split('.').next().unwrap_or(stem);
    stem == "routes" || stem == "js-routes"
}

fn is_route_helper_name(name: &str) -> bool {
    name.ends_with("_path") || name.ends_with("_url")
}

fn arg_expr<'a, 'b>(arg: &'b Argument<'a>) -> Option<&'b Expression<'a>> {
    match arg {
        Argument::SpreadElement(_) => None,
        other => Some(other.to_expression()),
    }
}

fn property_name(key: &PropertyKey<'_>) -> Option<String> {
    key.static_name().map(|n| n.into_owned())
}

fn verb_of(s: &str) -> Option<&'static str> {
    VERBS.iter().copied().find(|v| v.eq_ignore_ascii_case(s))
}

fn literal_verb(e: &Expression<'_>) -> Option<String> {
    match e.get_inner_expression() {
        Expression::StringLiteral(s) => verb_of(&s.value).map(str::to_string),
        _ => None,
    }
}

/// `api().get` -> GET, `apiRequestPost` -> POST, `http.delete` -> DELETE.
fn verb_from_name(text: &str) -> Option<&'static str> {
    let last = text.rsplit('.').next().unwrap_or(text);
    if let Some(v) = verb_of(last) {
        return Some(v);
    }
    // camelCase suffix: apiRequestPost. The capital keeps `target`, `widget` out.
    VERBS.iter().copied().find(|v| {
        let cap = format!("{}{}", &v[..1], v[1..].to_ascii_lowercase());
        last.ends_with(cap.as_str())
    })
}

/// Writes the callee as written (`api().get`, `this.http.post`); false when it is
/// not a simple identifier / member / call chain.
pub(crate) fn write_callee(e: &Expression<'_>, out: &mut String) -> bool {
    match e.get_inner_expression() {
        Expression::Identifier(id) => {
            out.push_str(&id.name);
            true
        }
        Expression::ThisExpression(_) => {
            out.push_str("this");
            true
        }
        Expression::StaticMemberExpression(m) => {
            if !write_callee(&m.object, out) {
                return false;
            }
            out.push('.');
            out.push_str(&m.property.name);
            true
        }
        Expression::CallExpression(c) => {
            if !write_callee(&c.callee, out) {
                return false;
            }
            out.push_str("()");
            true
        }
        Expression::ChainExpression(c) => match &c.expression {
            oxc_ast::ast::ChainElement::CallExpression(c) => write_callee(&c.callee, out) && {
                out.push_str("()");
                true
            },
            oxc_ast::ast::ChainElement::TSNonNullExpression(n) => write_callee(&n.expression, out),
            other => other.as_member_expression().is_some_and(|m| match m {
                oxc_ast::ast::MemberExpression::StaticMemberExpression(s) => {
                    write_callee(&s.object, out) && {
                        out.push('.');
                        out.push_str(&s.property.name);
                        true
                    }
                }
                _ => false,
            }),
        },
        _ => false,
    }
}

/// The URL-ish text of an expression, with `${…}` and non-literal operands of
/// `+` replaced by `:param`. `None` when there is no literal part at all (so a
/// bare variable is not a URL). Offsets of every literal leaf are pushed to `leaves`.
pub fn url_of(e: &Expression<'_>, leaves: &mut Vec<u32>) -> Option<String> {
    let (s, has_literal) = concat(e, leaves);
    has_literal.then_some(s)
}

fn concat(e: &Expression<'_>, leaves: &mut Vec<u32>) -> (String, bool) {
    match e.get_inner_expression() {
        Expression::StringLiteral(s) => {
            leaves.push(s.span.start);
            (s.value.to_string(), true)
        }
        Expression::TemplateLiteral(t) => {
            leaves.push(t.span.start);
            concat_template(t, leaves)
        }
        Expression::BinaryExpression(b) if b.operator == BinaryOperator::Addition => concat_binary(b, leaves),
        _ => (":param".to_string(), false),
    }
}

fn concat_template(t: &TemplateLiteral<'_>, _leaves: &mut Vec<u32>) -> (String, bool) {
    let mut out = String::new();
    for (i, q) in t.quasis.iter().enumerate() {
        out.push_str(q.value.cooked.as_ref().map_or(q.value.raw.as_str(), |c| c.as_str()));
        if i < t.expressions.len() {
            out.push_str(":param");
        }
    }
    (out, true)
}

fn concat_binary(b: &BinaryExpression<'_>, leaves: &mut Vec<u32>) -> (String, bool) {
    let (l, lh) = concat(&b.left, leaves);
    let (r, rh) = concat(&b.right, leaves);
    (l + &r, lh || rh)
}

/// Strips origin, query and fragment; keeps the leading slash as written.
/// Returns `None` when nothing path-like remains (`:param` alone, empty).
pub fn normalize_url(raw: &str) -> Option<String> {
    let mut s = raw.trim();
    if let Some(i) = s.find(['?', '#']) {
        s = &s[..i];
    }
    // scheme://host
    let mut rest = s;
    if let Some(i) = s.find("://") {
        if s[..i].chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) && i > 0 {
            let after = &s[i + 3..];
            rest = after.find('/').map_or("/", |j| &after[j..]);
        }
    } else if let Some(after) = s.strip_prefix("//") {
        rest = after.find('/').map_or("/", |j| &after[j..]);
    }
    // `${BASE_URL}/api/x` -> `/api/x`: a leading variable followed by `/` is a base URL.
    if let Some(after) = rest.strip_prefix(":param") {
        if after.starts_with('/') {
            rest = after;
        }
    }
    if rest.is_empty() {
        return None;
    }
    let mut url = rest.to_string();
    while url.contains(":param:param") {
        url = url.replace(":param:param", ":param");
    }
    url.replace(":param", "").chars().any(|c| c.is_alphanumeric()).then_some(url).or_else(|| (rest == "/").then(|| "/".to_string()))
}

#[cfg(test)]
mod tests {
    use super::normalize_url;

    #[test]
    fn normalizes() {
        assert_eq!(normalize_url("https://x.org/api/v1/a?b=1").as_deref(), Some("/api/v1/a"));
        assert_eq!(normalize_url(":param/api/x").as_deref(), Some("/api/x"));
        assert_eq!(normalize_url(":param"), None);
        assert_eq!(normalize_url("v1/foo#x").as_deref(), Some("v1/foo"));
        assert_eq!(normalize_url("/api/:param").as_deref(), Some("/api/:param"));
    }
}
