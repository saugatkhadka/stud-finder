//! JS HTTP call sites -> Rails controllers (`EdgeKind::ApiCall`).

use crate::matching::{is_catch_all, Hit, Matcher};
use crate::routes::{Route, RouteKind, RouteTable};
use ripple_core::config::BoundaryConfig;
use ripple_core::{Confidence, JsCallSite};

/// More distinct controllers than this and the call site is too ambiguous to link.
pub const MAX_CONTROLLERS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallOutcome {
    Matched(Confidence),
    /// Matched more than [`MAX_CONTROLLERS`] controllers; no edges.
    Ambiguous(usize),
    /// Matched only redirects, mounts or rack endpoints.
    NonController,
    Unmatched,
    /// Excluded by `boundary.url_prefixes`.
    Filtered,
}

/// The result for one call site, as last linked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallMatch {
    /// Index into `facts.js_call_sites`.
    pub site: usize,
    /// The URL actually matched (after applying `url_bases`).
    pub url: String,
    pub outcome: CallOutcome,
    /// Matching action routes, first route per controller, in table order.
    pub routes: Vec<usize>,
    /// Every matching action route of those controllers, one per distinct
    /// `controller#action`, in table order: one `ApiCall` edge each, scoped
    /// to its action (`posts_path` with no verb -> `posts#index` and `posts#create`).
    pub actions: Vec<usize>,
}

/// `Routes.userPath`, `api_v1_statuses_url` -> `user`, `api_v1_statuses`.
fn helper_route_name(helper: &str) -> String {
    let last = helper.rsplit('.').next().unwrap_or(helper);
    let mut snake = String::with_capacity(last.len() + 4);
    for (i, c) in last.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                snake.push('_');
            }
            snake.push(c.to_ascii_lowercase());
        } else {
            snake.push(c);
        }
    }
    for suffix in ["_path", "_url"] {
        if let Some(s) = snake.strip_suffix(suffix) {
            return s.to_string();
        }
    }
    snake
}

fn join_base(base: &str, url: &str) -> String {
    format!("/{}/{}", base.trim_matches('/'), url.trim_start_matches('/')).replace("//", "/")
}

/// Action routes among `hits` whose controller is one of `picked`, first per
/// distinct `controller#action`, in table order.
fn actions_of(routes: &[Route], hits: &[usize], picked: &[usize]) -> Vec<usize> {
    let mut sorted = hits.to_vec();
    sorted.sort_unstable();
    let mut out: Vec<usize> = Vec::new();
    for h in sorted {
        let r = &routes[h];
        if r.kind != RouteKind::Action || !picked.iter().any(|&p| routes[p].controller == r.controller) {
            continue;
        }
        if !out.iter().any(|&o| routes[o].controller == r.controller && routes[o].action == r.action) {
            out.push(h);
        }
    }
    out
}

/// Distinct controllers among `hits`, keeping the first route of each.
fn first_per_controller(routes: &[Route], hits: impl IntoIterator<Item = usize>) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    for h in hits {
        let r = &routes[h];
        if r.kind == RouteKind::Action && !out.iter().any(|&o| routes[o].controller == r.controller) {
            out.push(h);
        }
    }
    out
}

pub fn match_call(site_index: usize, site: &JsCallSite, table: &RouteTable, matcher: &Matcher, cfg: &BoundaryConfig) -> CallMatch {
    let routes = &table.routes[..];
    let routes_table = routes;
    let verb = site.method.as_deref().map(str::to_ascii_uppercase);
    let done = |url: String, outcome, routes: Vec<usize>, hits: &[usize]| {
        let actions = actions_of(routes_table, hits, &routes);
        CallMatch { site: site_index, url, outcome, routes, actions }
    };

    if site.route_helper {
        let name = helper_route_name(&site.url);
        let Some(named_idx) = table.named(&name) else {
            return done(site.url.clone(), CallOutcome::Unmatched, Vec::new(), &[]);
        };
        let named = &routes[named_idx];
        // The helper names one route; the same path under other verbs is the
        // rest of the resource (`posts_path` -> index and create). Per verb,
        // routes count up to the first unconstrained one (the named one for
        // its own verb, unless it is constrained: `get :issues, action:
        // :issues_calendar, constraints: ics?` followed by `get :issues`), so
        // host-constrained duplicates of the path further down are skipped.
        let mut seen_verbs: Vec<&str> = if named.constrained { Vec::new() } else { vec![named.verb.as_str()] };
        let mut hits = vec![named_idx];
        for (i, r) in routes.iter().enumerate() {
            if i != named_idx && r.path == named.path && !seen_verbs.contains(&r.verb.as_str()) && (i > named_idx || r.verb != named.verb) {
                if !r.constrained {
                    seen_verbs.push(&r.verb);
                }
                hits.push(i);
            }
        }
        let all = hits.clone();
        hits.retain(|&i| verb.as_ref().is_none_or(|v| routes[i].verb == *v || routes[i].verb == "ANY"));
        // A verb the path does not serve (a `_method` override, a verb guessed
        // from context): keep the whole path rather than drop the edge.
        let fallback = hits.is_empty();
        if fallback {
            hits = all;
        }
        hits.sort_unstable();
        let picked = first_per_controller(routes, hits.iter().copied());
        let outcome = match picked.len() {
            0 => CallOutcome::NonController,
            n if n > MAX_CONTROLLERS => CallOutcome::Ambiguous(n),
            _ if fallback => CallOutcome::Matched(Confidence::Heuristic),
            _ => CallOutcome::Matched(Confidence::Exact),
        };
        let picked = if matches!(outcome, CallOutcome::Matched(_)) { picked } else { Vec::new() };
        return done(named.path.clone(), outcome, picked, &hits);
    }

    let candidates: Vec<String> = if site.url.starts_with('/') {
        vec![site.url.clone()]
    } else {
        cfg.url_bases.iter().map(|b| join_base(b, &site.url)).collect()
    };
    let allowed = |u: &str| cfg.url_prefixes.is_empty() || cfg.url_prefixes.iter().any(|p| u.starts_with(p.as_str()));
    let mut any_allowed = false;
    let mut first_url = None;
    for url in candidates {
        if !allowed(&url) {
            continue;
        }
        any_allowed = true;
        first_url.get_or_insert_with(|| url.clone());
        let matching = |verb: Option<&str>| -> Vec<Hit> { matcher.matches(routes, verb, &url).into_iter().filter(|h| !is_catch_all(&routes[h.route])).collect() };
        let mut hits = matching(verb.as_deref());
        // No route serves the verb at this path (`_method` override, a verb
        // guessed from context, a mock with the wrong verb): any verb, heuristic.
        let fallback = hits.is_empty() && verb.is_some();
        if fallback {
            hits = matching(None);
        }
        // A JS `:param` is almost always an id, not the literal segment of
        // an earlier route: prefer the routes that bind it as a parameter.
        if hits.iter().any(|h| h.binds) {
            hits.retain(|h| h.binds);
        }
        let hits: Vec<usize> = hits.into_iter().map(|h| h.route).collect();
        if hits.is_empty() {
            continue;
        }
        let picked = first_per_controller(routes, hits.iter().copied());
        if picked.is_empty() {
            return done(url, CallOutcome::NonController, Vec::new(), &[]);
        }
        if picked.len() > MAX_CONTROLLERS {
            return done(url, CallOutcome::Ambiguous(picked.len()), Vec::new(), &[]);
        }
        let action_hits = hits.iter().filter(|&&i| routes[i].kind == RouteKind::Action).count();
        let confidence = if !fallback && !url.contains(":param") && verb.is_some() && action_hits == 1 { Confidence::Inferred } else { Confidence::Heuristic };
        return done(url, CallOutcome::Matched(confidence), picked, &hits);
    }
    if !any_allowed {
        return done(site.url.clone(), CallOutcome::Filtered, Vec::new(), &[]);
    }
    done(first_url.unwrap_or_else(|| site.url.clone()), CallOutcome::Unmatched, Vec::new(), &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_names() {
        assert_eq!(helper_route_name("Routes.user_path"), "user");
        assert_eq!(helper_route_name("Routes.apiV1StatusesPath"), "api_v1_statuses");
        assert_eq!(helper_route_name("edit_post_url"), "edit_post");
        assert_eq!(join_base("/api/", "v1/foo"), "/api/v1/foo");
        assert_eq!(join_base("/", "v1/foo"), "/v1/foo");
    }
}

#[cfg(test)]
mod helper_tests {
    use super::*;
    use crate::routes;

    #[test]
    fn a_constrained_named_route_falls_through_to_the_next_one() {
        let src = "Rails.application.routes.draw do\n  resource :dashboard, controller: 'dashboard', only: [] do\n    get :issues, action: :issues_calendar, constraints: ->(req) { req.format == :ics }\n    get :issues\n    get :home\n  end\nend\n";
        let table = routes::build(&[("config/routes.rb".into(), src.as_bytes().to_vec())], false);
        let matcher = Matcher::new(&table.routes);
        let site = JsCallSite { file: "spec/x_spec.rb".into(), line: 1, method: Some("GET".into()), url: "issues_dashboard_path".into(), callee: "visit".into(), route_helper: true, link: false };
        let m = match_call(0, &site, &table, &matcher, &BoundaryConfig::default());
        let actions: Vec<&str> = m.actions.iter().map(|&i| table.routes[i].action.as_str()).collect();
        assert_eq!(actions, ["issues_calendar", "issues"]);
        assert_eq!(m.outcome, CallOutcome::Matched(Confidence::Exact));
    }
}
