//! Matching concrete or partially-dynamic URLs against the route table with
//! Rails' dispatch semantics: routes are tried in definition order and the
//! first match wins.
//!
//! A call-site URL may contain `:param` segments (unknown values). Then several
//! routes can be the first match depending on the value, so we collect every
//! matching route until one *subsumes* the URL — matches it for every possible
//! value of its `:param`s (each JS `:param` lands on a route param or glob) and
//! has no constraints. Later routes are unreachable for that URL.

use crate::routes::{Route, RouteKind};
use regex::Regex;
use std::collections::HashMap;

#[derive(Debug, Clone)]
enum Seg {
    Lit(String),
    /// `:name` — one segment without `/` or `.` (Rails' default requirement).
    Param,
    /// `*name` — one or more segments.
    Glob,
    /// A segment mixing literals and params (`@:username`, `:feed_code.:format`).
    Pattern(Regex),
}

#[derive(Debug)]
struct Variant {
    route: usize,
    segs: Vec<Seg>,
}

/// Index over the route table: variants (optional groups expanded) bucketed by
/// their first literal segment.
#[derive(Debug, Default)]
pub struct Matcher {
    variants: Vec<Variant>,
    by_first: HashMap<String, Vec<usize>>,
    /// Variants whose first segment is not a literal (or that are empty).
    wild: Vec<usize>,
}

/// Expands `( )` optional groups into concrete alternatives (`/a(/:b)` ->
/// `/a`, `/a/:b`). Capped to keep pathological paths bounded.
fn expand(path: &str) -> Vec<String> {
    fn go(chars: &[char], i: &mut usize, depth: usize) -> Vec<String> {
        let mut alts = vec![String::new()];
        while *i < chars.len() {
            match chars[*i] {
                '(' => {
                    *i += 1;
                    let inner = go(chars, i, depth + 1);
                    let mut next = Vec::new();
                    for a in &alts {
                        next.push(a.clone());
                        for b in &inner {
                            next.push(format!("{a}{b}"));
                        }
                    }
                    next.truncate(64);
                    alts = next;
                }
                ')' if depth > 0 => {
                    *i += 1;
                    return alts;
                }
                c => {
                    for a in &mut alts {
                        a.push(c);
                    }
                    *i += 1;
                }
            }
        }
        alts
    }
    let chars: Vec<char> = path.chars().collect();
    let mut out = go(&chars, &mut 0, 0);
    out.dedup();
    out
}

fn compile_seg(s: &str) -> Seg {
    if let Some(rest) = s.strip_prefix(':')
        && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Seg::Param;
        }
    if let Some(rest) = s.strip_prefix('*')
        && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Seg::Glob;
        }
    if !s.contains(':') && !s.contains('*') {
        return Seg::Lit(s.to_string());
    }
    let mut re = String::from("^");
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ':' || c == '*' {
            while chars.peek().is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_') {
                chars.next();
            }
            re.push_str(if c == ':' { "[^/.?]+" } else { ".+" });
        } else {
            re.push_str(&regex::escape(&c.to_string()));
        }
    }
    re.push('$');
    Regex::new(&re).map_or_else(|_| Seg::Lit(s.to_string()), Seg::Pattern)
}

fn split(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|s| !s.is_empty())
}

/// How a route matched a URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    pub route: usize,
    /// Matches every instantiation of the URL's `:param`s: each one lands on
    /// a route param or glob and the route has no constraints, so no later
    /// route can be reached for this verb.
    pub subsumes: bool,
    /// Every URL `:param` lands on a route param or glob (constraints aside).
    /// A hit without this only matches when a `:param` happens to equal a
    /// literal segment (`/accounts/:param` vs `/accounts/lookup`).
    pub binds: bool,
}

const URL_PARAM: &str = ":param";

/// Matches one route segment against one URL segment. `Some(true)` when the
/// match holds for any value of the URL segment.
fn seg_match(seg: &Seg, url: &str) -> Option<bool> {
    let dynamic = url.contains(URL_PARAM);
    match seg {
        Seg::Param if url == URL_PARAM => Some(true),
        Seg::Param => (!url.contains('.') || dynamic).then_some(false),
        Seg::Lit(l) if !dynamic => (l == url).then_some(true),
        Seg::Lit(_) if url == URL_PARAM => Some(false),
        Seg::Lit(l) => url_seg_matches(url, l).then_some(false),
        Seg::Pattern(re) => re.is_match(&url.replace(URL_PARAM, "x")).then_some(false),
        Seg::Glob => Some(true),
    }
    .map(|exact| exact || !dynamic)
}

/// Does a URL segment with embedded `:param`s (`v1:param`, `:param.json`)
/// match the literal route segment? Regexes are cached per URL segment.
fn url_seg_matches(url: &str, literal: &str) -> bool {
    thread_local! {
        static CACHE: std::cell::RefCell<std::collections::HashMap<String, Option<Regex>>> = Default::default();
    }
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() > 4096 {
            c.clear();
        }
        c.entry(url.to_string())
            .or_insert_with(|| {
                let parts: Vec<String> = url.split(URL_PARAM).map(regex::escape).collect();
                Regex::new(&format!("^{}$", parts.join("[^/]+"))).ok()
            })
            .as_ref()
            .is_some_and(|re| re.is_match(literal))
    })
}

fn match_segs(route: &[Seg], url: &[&str]) -> Option<bool> {
    match (route.first(), url.first()) {
        (None, None) => Some(true),
        (Some(Seg::Glob), Some(_)) => {
            let mut best = None;
            for take in 1..=url.len() {
                if let Some(rest) = match_segs(&route[1..], &url[take..]) {
                    best = Some(best.unwrap_or(false) || rest);
                    if rest {
                        break;
                    }
                }
            }
            best
        }
        (Some(seg), Some(u)) => {
            let here = seg_match(seg, u)?;
            match_segs(&route[1..], &url[1..]).map(|rest| rest && here)
        }
        _ => None,
    }
}

/// Splits a trailing `.json`-style format extension off the last segment.
fn strip_format(url: &str) -> Option<&str> {
    let last = url.rsplit('/').next()?;
    let (stem, ext) = last.rsplit_once('.')?;
    let known = !stem.is_empty() && (1..=5).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric());
    known.then(|| &url[..url.len() - ext.len() - 1])
}

impl Matcher {
    pub fn new(routes: &[Route]) -> Self {
        let mut m = Matcher::default();
        for (i, r) in routes.iter().enumerate() {
            let mut paths = expand(&r.path);
            if matches!(r.kind, RouteKind::Mount { .. }) {
                paths = paths.into_iter().flat_map(|p| [p.clone(), format!("{p}/*rest")]).collect();
            }
            for p in paths {
                let segs: Vec<Seg> = split(&p).map(compile_seg).collect();
                let id = m.variants.len();
                match segs.first() {
                    Some(Seg::Lit(l)) => m.by_first.entry(l.clone()).or_default().push(id),
                    _ => m.wild.push(id),
                }
                m.variants.push(Variant { route: i, segs });
            }
        }
        m
    }

    /// Candidate variants for a URL, in route-table order.
    fn candidates(&self, first: Option<&str>) -> Vec<usize> {
        let mut ids: Vec<usize> = match first {
            Some(f) if !f.contains(URL_PARAM) => self.by_first.get(f).into_iter().flatten().chain(&self.wild).copied().collect(),
            _ => (0..self.variants.len()).collect(),
        };
        ids.sort_unstable_by_key(|&id| (self.variants[id].route, id));
        ids
    }

    /// Every route that can be Rails' first match for `url` (see module docs).
    /// `verb` is upper-case; `None` means any verb.
    pub fn matches(&self, routes: &[Route], verb: Option<&str>, url: &str) -> Vec<Hit> {
        let url = url.split(['?', '#']).next().unwrap_or("");
        let hits = self.matches_exact(routes, verb, url);
        // Every formatted route also accepts `/x.json` as `/x` + format, so
        // both readings compete in table order.
        let Some(stripped) = strip_format(url) else { return hits };
        let mut all = hits;
        for h in self.matches_exact(routes, verb, stripped) {
            match all.iter_mut().find(|a| a.route == h.route) {
                Some(a) => {
                    a.subsumes |= h.subsumes;
                    a.binds |= h.binds;
                }
                None => all.push(h),
            }
        }
        all.sort_by_key(|h| h.route);
        prune(routes, verb, all)
    }

    fn matches_exact(&self, routes: &[Route], verb: Option<&str>, url: &str) -> Vec<Hit> {
        let segs: Vec<&str> = split(url).collect();
        let mut hits: Vec<Hit> = Vec::new();
        let mut closed: Vec<&str> = Vec::new();
        let mut last_route = usize::MAX;
        for id in self.candidates(segs.first().copied()) {
            let v = &self.variants[id];
            if v.route == last_route {
                continue;
            }
            let r = &routes[v.route];
            let any = r.verb == "ANY";
            if let Some(verb) = verb {
                if !any && r.verb != verb {
                    continue;
                }
            } else if closed.contains(&"ANY") || (!any && closed.contains(&r.verb.as_str())) {
                continue;
            }
            let Some(subsumes) = match_segs(&v.segs, &segs) else { continue };
            last_route = v.route;
            let binds = subsumes;
            let subsumes = subsumes && !r.constrained;
            hits.push(Hit { route: v.route, subsumes, binds });
            if subsumes {
                if verb.is_some() {
                    break;
                }
                closed.push(if any { "ANY" } else { r.verb.as_str() });
            }
        }
        hits
    }
}

/// Rails' first-match rule over hits in table order: once a route matches
/// every instantiation for a verb, later routes for that verb are unreachable.
fn prune(routes: &[Route], verb: Option<&str>, hits: Vec<Hit>) -> Vec<Hit> {
    let mut out = Vec::new();
    let mut closed: Vec<&str> = Vec::new();
    for h in hits {
        let r = &routes[h.route];
        let any = r.verb == "ANY";
        if closed.contains(&"ANY") || (!any && closed.contains(&r.verb.as_str())) {
            continue;
        }
        out.push(h);
        if h.subsumes {
            if verb.is_some() {
                break;
            }
            closed.push(if any { "ANY" } else { r.verb.as_str() });
        }
    }
    out
}

/// A catch-all route (`match "*path"`): it matches every URL and only means
/// "no route", so call-site matching ignores it.
pub fn is_catch_all(route: &Route) -> bool {
    route.path.trim_start_matches(['/', '(']).starts_with('*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(verb: &str, path: &str, ctrl: &str) -> Route {
        Route {
            verb: verb.into(),
            path: path.into(),
            controller: ctrl.into(),
            action: "x".into(),
            name: None,
            file: "config/routes.rb".into(),
            line: 1,
            kind: RouteKind::Action,
            constrained: false,
        }
    }

    #[test]
    fn expands_optional_groups() {
        assert_eq!(expand("/a(/:b(/:c))"), vec!["/a", "/a/:b", "/a/:b/:c"]);
        assert_eq!(expand("(/*full_path)"), vec!["", "/*full_path"]);
    }

    #[test]
    fn first_match_and_subsumption() {
        let routes = vec![
            route("GET", "/accounts/lookup", "lookup"),
            route("GET", "/accounts/:id", "accounts"),
            route("GET", "/accounts/:id/statuses", "statuses"),
            route("GET", "/@:username", "short"),
            route("GET", "/media_proxy/:id(/*any)", "proxy"),
        ];
        let m = Matcher::new(&routes);
        let ctrls = |verb: Option<&str>, url: &str| -> Vec<String> { m.matches(&routes, verb, url).iter().map(|h| routes[h.route].controller.clone()).collect() };
        assert_eq!(ctrls(Some("GET"), "/accounts/lookup"), ["lookup"]);
        assert_eq!(ctrls(Some("GET"), "/accounts/:param"), ["lookup", "accounts"]);
        assert_eq!(ctrls(Some("GET"), "/accounts/123"), ["accounts"]);
        assert_eq!(ctrls(Some("GET"), "/accounts/123.json"), ["accounts"]);
        assert_eq!(ctrls(Some("POST"), "/accounts/123"), Vec::<String>::new());
        assert_eq!(ctrls(None, "/@:param"), ["short"]);
        assert_eq!(ctrls(None, "/@alice"), ["short"]);
        assert_eq!(ctrls(None, "/media_proxy/1/original"), ["proxy"]);
        assert_eq!(ctrls(None, "/media_proxy/1"), ["proxy"]);
    }
}
