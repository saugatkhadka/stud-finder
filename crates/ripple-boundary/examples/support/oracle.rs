//! DEV-ONLY route-table oracle, extracted from a repo's specs.
//!
//! * literal paths in request/routing/controller specs: `get '/api/v1/x'`,
//!   `post "/x/#{id}/y"` (interpolation -> `:param`), `describe 'GET /x/:id'`;
//!   routing specs add the expected endpoint (`route_to('c#a')`) or
//!   `not_to be_routable`;
//! * controller specs: `get :show` inside `describe FooController` must have
//!   a `GET` route to `foo#show` (Rails 5+ raises without one);
//! * path helpers passed to a request verb or `visit` (`get api_v1_x_path`)
//!   must be route names in the table.

use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Expect {
    /// Some route must match.
    Routable,
    /// The first match must be this `controller#action` (with `:param`s in
    /// the path: one of the candidate matches).
    Endpoint(String),
    /// No route may match.
    NotRoutable,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Check {
    Path { verb: String, path: String, expect: Expect },
    Action { verb: String, controller: String, action: String },
    Helper { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Case {
    pub check: Check,
    pub file: String,
    pub line: u32,
}

struct Patterns {
    call: Regex,
    describe: Regex,
    route_to: Regex,
    action: Regex,
    helper: Regex,
    controller: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        call: Regex::new(r#"(?:^|[\s(])(get|post|put|patch|delete)\s*\(?\s*(["'])(/[^"']*)["']"#).expect("regex"),
        describe: Regex::new(r#"\b(?:describe|context|it)\s*\(?\s*["'](GET|POST|PUT|PATCH|DELETE) (/[^\s"']*)"#).expect("regex"),
        route_to: Regex::new(r#"route_to\(\s*["']([\w/]+#\w+)["']"#).expect("regex"),
        action: Regex::new(r"^\s*(?:\w+\s*=\s*)?(get|post|put|patch|delete)\s*\(?\s*:(\w+)").expect("regex"),
        helper: Regex::new(r"(?:^|[\s(])(?:get|post|put|patch|delete|visit)\s*\(?\s*([a-z][a-z0-9_]*)_(?:path|url)\b").expect("regex"),
        controller: Regex::new(r"^\s*(?:RSpec\.)?describe\s+([A-Z][\w:]*Controller)\b").expect("regex"),
    })
}

pub fn is_spec(path: &str) -> bool {
    path.ends_with("_spec.rb") && ["spec/requests/", "spec/routing/", "spec/controllers/", "spec/integration/"].iter().any(|d| path.starts_with(d))
}

fn clean(path: &str) -> String {
    let mut out = String::new();
    let mut rest = path;
    while let Some(i) = rest.find("#{") {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail.find('}').map_or(tail.len(), |j| j + 1);
        out.push_str(":param");
        rest = &tail[end..];
    }
    out.push_str(rest);
    let p = out.split(['?', '#']).next().unwrap_or("").to_string();
    // `:id` placeholders in describe headings behave like JS `:param`s.
    p.split('/').map(|s| if s.starts_with(':') { ":param" } else { s }).collect::<Vec<_>>().join("/")
}

/// `Api::V1::StatusesController` -> `api/v1/statuses`.
fn controller_path(class: &str) -> String {
    let base = class.strip_suffix("Controller").unwrap_or(class);
    base.split("::")
        .map(|seg| {
            let mut out = String::new();
            for (i, c) in seg.chars().enumerate() {
                if c.is_ascii_uppercase() {
                    if i > 0 {
                        out.push('_');
                    }
                    out.push(c.to_ascii_lowercase());
                } else {
                    out.push(c);
                }
            }
            out
        })
        .collect::<Vec<_>>()
        .join("/")
}

pub fn scan(file: &str, text: &str) -> Vec<Case> {
    let p = patterns();
    let mut out = Vec::new();
    let controller = if file.starts_with("spec/controllers/") { text.lines().find_map(|l| p.controller.captures(l).map(|c| controller_path(&c[1]))) } else { None };
    let mut push = |check, line| out.push(Case { check, file: file.into(), line });
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_start();
        if t.starts_with('#') {
            continue;
        }
        let line_no = i as u32 + 1;
        if let Some(c) = p.describe.captures(line) {
            push(Check::Path { verb: c[1].to_string(), path: clean(&c[2]), expect: Expect::Routable }, line_no);
            continue;
        }
        if let (Some(ctrl), Some(c)) = (&controller, p.action.captures(line)) {
            push(Check::Action { verb: c[1].to_ascii_uppercase(), controller: ctrl.clone(), action: c[2].to_string() }, line_no);
        }
        for c in p.helper.captures_iter(line) {
            push(Check::Helper { name: c[1].to_string() }, line_no);
        }
        for c in p.call.captures_iter(line) {
            if &c[2] == "'" && c[3].contains("#{") {
                continue;
            }
            let expect = if line.contains("not_to be_routable") || line.contains("not_to route_to") {
                Expect::NotRoutable
            } else if let Some(r) = p.route_to.captures(line) {
                Expect::Endpoint(r[1].to_string())
            } else {
                Expect::Routable
            };
            push(Check::Path { verb: c[1].to_ascii_uppercase(), path: clean(&c[3]), expect }, line_no);
        }
    }
    out
}
