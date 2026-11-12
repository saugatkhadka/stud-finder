//! DEV-ONLY regex scanner for JS HTTP call sites.
//!
//! This is NOT the production extractor (that lives in the JS crate and
//! produces `facts.js_call_sites` from a real parse). It exists so the
//! boundary linker can be exercised against real repos from the `dump`
//! example and tests. It recognises:
//!
//! * `<expr>.get|post|put|patch|delete(<literal>` (`api().get('/api/v1/x')`,
//!   `axios.post(...)`, `http.get(...)`), including across line breaks;
//! * `fetch(<literal>, { method: "X" })` (GET when no method is given);
//! * `apiRequestGet('v1/x')` and friends, `apiRequest('PUT', 'v1/x')`;
//! * `{ method: "X", url: <literal> }` request objects;
//! * `Routes.foo_path(...)` js-routes helpers (`route_helper = true`), with
//!   the verb of an enclosing request object when there is one.
//!
//! Template literals have `${...}` replaced by `:param`; query strings and
//! origins are stripped. Anything starting with an expression is skipped.

use regex::Regex;
use ripple_core::JsCallSite;
use std::sync::OnceLock;

struct Patterns {
    verb_call: Regex,
    fetch: Regex,
    api_request: Regex,
    url_key: Regex,
    helper: Regex,
    method_key: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        verb_call: Regex::new(r"(?s)((?:[A-Za-z_$][\w$]*(?:\([^()]*\))?\.)*[A-Za-z_$][\w$]*(?:\([^()]*\))?)\.(get|post|put|patch|delete)\s*(?:<[^>()]*>)?\(\s*([`'\x22])").expect("regex"),
        fetch: Regex::new(r"(?s)\bfetch\(\s*([`'\x22])").expect("regex"),
        api_request: Regex::new(r#"(?s)\b(apiRequest(Get|Post|Put|Patch|Delete)?)\s*(?:<[^>()]*>)?\(\s*(?:['"](GET|POST|PUT|PATCH|DELETE)['"]\s*,\s*)?([`'"])"#).expect("regex"),
        url_key: Regex::new(r"\burl:\s*([`'\x22])").expect("regex"),
        helper: Regex::new(r"\bRoutes\.([A-Za-z_][A-Za-z0-9_]*(?:_path|_url|Path|Url))\b").expect("regex"),
        method_key: Regex::new(r#"\bmethod:\s*['"]([A-Za-z]+)['"]"#).expect("regex"),
    })
}

fn line_of(text: &str, offset: usize) -> u32 {
    (text[..offset].bytes().filter(|&b| b == b'\n').count() + 1) as u32
}

/// Reads the literal starting right after `quote` at `start`; returns the
/// normalised URL or None if it is not a usable path.
fn literal_url(text: &str, start: usize, quote: char) -> Option<String> {
    let rest = &text[start..];
    let mut out = String::new();
    let mut chars = rest.char_indices().peekable();
    let mut end = None;
    while let Some((i, c)) = chars.next() {
        if c == '\\' {
            chars.next();
            continue;
        }
        if c == quote {
            end = Some(i);
            break;
        }
        if c == '\n' && quote != '`' {
            return None;
        }
        if quote == '`' && c == '$' && chars.peek().is_some_and(|&(_, n)| n == '{') {
            // Skip the balanced ${ ... } expression.
            let mut depth = 0;
            for (_, c2) in chars.by_ref() {
                match c2 {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            out.push_str(":param");
            continue;
        }
        out.push(c);
    }
    let end = end?;
    // `'/api/v1/statuses/' + id`
    if out.ends_with('/') && rest[end + 1..].trim_start().starts_with('+') {
        out.push_str(":param");
    }
    normalise(&out)
}

fn normalise(url: &str) -> Option<String> {
    let mut u = url;
    if let Some(i) = u.find("://") {
        u = &u[i + 3..];
        u = u.find('/').map_or("/", |j| &u[j..]);
    }
    let u = u.split(['?', '#']).next().unwrap_or("");
    if u.is_empty() || u.starts_with(":param") || u.contains(' ') || u.starts_with('.') {
        return None;
    }
    Some(u.to_string())
}

fn method_near(text: &str, from: usize, to: usize) -> Option<String> {
    let (from, to) = (floor(text, from), floor(text, to.min(text.len())));
    patterns().method_key.captures(&text[from..to]).map(|c| c[1].to_ascii_uppercase())
}

fn floor(text: &str, mut i: usize) -> usize {
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// The `{ ... }` object enclosing `offset`, approximately (bounded window).
fn enclosing_object(text: &str, offset: usize) -> (usize, usize) {
    let lo = floor(text, offset.saturating_sub(300));
    let start = text[lo..offset].rfind('{').map_or(offset, |i| lo + i);
    let hi = floor(text, (offset + 300).min(text.len()));
    let end = text[offset..hi].find('}').map_or(hi, |i| offset + i);
    (start, end)
}

/// `api.get('/path', (req, res) => ...)`: a server route, not a request.
fn is_route_handler(text: &str, from: usize) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#"^[^\n]*?['"`]\s*,\s*(?:async\s+)?(?:function\s*)?\(\s*req\b"#).expect("regex"));
    re.is_match(&text[from..floor(text, (from + 200).min(text.len()))])
}

const NOT_CLIENTS: &[&str] = &["cache", "caches", "app", "router", "server", "map", "params", "searchParams", "headers", "store", "storage", "localStorage", "sessionStorage", "formData", "this"];

pub fn scan(file: &str, text: &str) -> Vec<JsCallSite> {
    let p = patterns();
    let mut out = Vec::new();
    let mut push = |offset: usize, method: Option<String>, url: String, callee: String, route_helper: bool| {
        out.push(JsCallSite { file: file.to_string(), line: line_of(text, offset), method, url, callee, route_helper });
    };
    for c in p.verb_call.captures_iter(text) {
        let (recv, verb, q) = (&c[1], &c[2], c.get(3).unwrap());
        // Not HTTP clients: the Cache API, Express/Koa routers (they *serve*
        // the path), Maps and storage. Also require the literal to look like a path.
        let last = recv.rsplit('.').next().unwrap_or(recv);
        if NOT_CLIENTS.contains(&last) || is_route_handler(text, q.end()) {
            continue;
        }
        let Some(url) = literal_url(text, q.end(), q.as_str().chars().next().unwrap_or('\'')) else { continue };
        if !url.starts_with('/') && !url.starts_with("v1/") && !url.starts_with("v2/") && !url.starts_with("api/") {
            continue;
        }
        push(c.get(2).map_or(q.start(), |v| v.start()), Some(verb.to_ascii_uppercase()), url, format!("{recv}.{verb}"), false);
    }
    for c in p.fetch.captures_iter(text) {
        let (m, q) = (c.get(0).unwrap(), c.get(1).unwrap());
        let Some(url) = literal_url(text, q.end(), q.as_str().chars().next().unwrap_or('\'')) else { continue };
        // Options end at the call's first `)` (they rarely contain calls before `method:`).
        let end = text[q.end()..floor(text, (q.end() + 300).min(text.len()))].find(')').map_or(q.end() + 300, |i| q.end() + i);
        let method = method_near(text, q.end(), end).or(Some("GET".into()));
        push(m.start(), method, url, "fetch".into(), false);
    }
    for c in p.api_request.captures_iter(text) {
        let (m, q) = (c.get(0).unwrap(), c.get(4).unwrap());
        let method = c.get(2).or(c.get(3)).map(|v| v.as_str().to_ascii_uppercase());
        let Some(url) = literal_url(text, q.end(), q.as_str().chars().next().unwrap_or('\'')) else { continue };
        // `apiRequest('GET', url)` with a non-literal URL: the verb is not a URL.
        if ["GET", "POST", "PUT", "PATCH", "DELETE"].contains(&url.as_str()) {
            continue;
        }
        push(m.start(), method, url, c[1].to_string(), false);
    }
    for c in p.url_key.captures_iter(text) {
        let (m, q) = (c.get(0).unwrap(), c.get(1).unwrap());
        let Some(url) = literal_url(text, q.end(), q.as_str().chars().next().unwrap_or('\'')) else { continue };
        if !url.starts_with('/') {
            continue;
        }
        let (s, e) = enclosing_object(text, m.start());
        let Some(method) = method_near(text, s, e) else { continue };
        push(m.start(), Some(method), url, "request".into(), false);
    }
    for c in p.helper.captures_iter(text) {
        let m = c.get(0).unwrap();
        let (s, e) = enclosing_object(text, m.start());
        let method = method_near(text, s, e);
        push(m.start(), method, m.as_str().to_string(), "Routes".into(), true);
    }
    out.sort_by(|a, b| (a.line, &a.url).cmp(&(b.line, &b.url)));
    out.dedup_by(|a, b| a.line == b.line && a.url == b.url && a.method == b.method);
    out
}

/// Should this file be scanned at all?
#[allow(dead_code)] // unused by the scanner's own test
pub fn is_source(path: &str) -> bool {
    let js = [".js", ".jsx", ".ts", ".tsx", ".mjs", ".vue"].iter().any(|e| path.ends_with(e));
    js && !path.contains("node_modules/")
        && !path.contains("__tests__/")
        && !path.contains("/vendor/")
        && !path.starts_with("vendor/")
        && !path.starts_with("spec/")
        && !path.starts_with("public/")
        && ![".test.", ".spec.", ".stories.", ".min.js"].iter().any(|m| path.contains(m))
}
