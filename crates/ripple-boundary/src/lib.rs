//! ripple-boundary: edges that cross the Rails <-> JavaScript boundary.
//!
//! * [`EdgeKind::RailsRoute`]: route file -> controller file, from a static
//!   interpretation of the routing DSL ([`routes`]).
//! * [`EdgeKind::ApiCall`]: JS file -> controller file, by matching
//!   `facts.js_call_sites` against that route table ([`matching`], [`calls`]).
//! * [`EdgeKind::ViewMount`]: template/controller <-> JS component or
//!   entrypoint, both directions ([`mounts`]).
//!
//! The linker caches route sources and per-file mount scans; a warm `link`
//! re-reads only changed files and recomputes the (cheap) joins, so its output
//! always equals a full link on the current tree.
//!
//! GraphQL is not handled. It would slot in as a fourth pass: parse the
//! schema dump (`schema.graphql`/`schema.json`) or the `GraphQL::Schema`
//! classes to map root fields to resolver/type files, have the JS extractor
//! emit operation documents as facts (`gql` tags, `.graphql` files), and link
//! each operation's root fields to those files as `EdgeKind::GraphQl`.

pub mod calls;
pub mod inflect;
pub mod matching;
pub mod mounts;
pub mod routes;

pub use calls::{CallMatch, CallOutcome};
pub use mounts::{MountKind, MountRecord, MountRef, Outcome};
pub use routes::{Route, RouteKind, RouteTable};

use matching::Matcher;
use mounts::{BundlerConfig, JsIndex, Registry};
use ripple_core::{ChangeSet, Confidence, Edge, EdgeKind, ExtractOutput, Facts, Linker, RelPath, Warning, Workspace};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Cap on per-item warnings of one kind (unmatched call sites, unresolved mounts, ...).
const WARNING_SAMPLE: usize = 20;

#[derive(Default)]
pub struct BoundaryLinker {
    built: bool,
    route_sources: BTreeMap<RelPath, Vec<u8>>,
    api_only: bool,
    table: RouteTable,
    matcher: Matcher,
    mount_scans: HashMap<RelPath, Vec<MountRef>>,
    registrations: HashMap<RelPath, Vec<(String, String)>>,
    mounts: Vec<MountRecord>,
    calls: Vec<CallMatch>,
}

fn glob_set(globs: &[String]) -> Vec<glob::Pattern> {
    globs.iter().filter_map(|g| glob::Pattern::new(g).ok()).collect()
}

fn matches_any(patterns: &[glob::Pattern], path: &str) -> bool {
    let opts = glob::MatchOptions { case_sensitive: true, require_literal_separator: true, require_literal_leading_dot: false };
    patterns.iter().any(|p| p.matches_with(path, opts))
}

fn read_text(ws: &Workspace, path: &str) -> Option<String> {
    std::fs::read(ws.abs(path)).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Controller path (`api/v1/statuses`) -> files defining it, from
/// `**/app/controllers/<controller>_controller.rb`.
fn controller_index(files: &[RelPath]) -> HashMap<String, Vec<RelPath>> {
    let mut idx: HashMap<String, Vec<RelPath>> = HashMap::new();
    for f in files {
        let Some(stem) = f.strip_suffix("_controller.rb") else { continue };
        let Some(i) = stem.rfind("app/controllers/") else { continue };
        if i > 0 && !stem[..i].ends_with('/') {
            continue;
        }
        idx.entry(stem[i + "app/controllers/".len()..].to_string()).or_default().push(f.clone());
    }
    idx
}

fn api_only(ws: &Workspace) -> bool {
    read_text(ws, "config/application.rb").is_some_and(|t| t.lines().any(|l| l.trim_start().starts_with("config.api_only = true")))
}

/// A Ruby method name usable as an edge scope (`index`, `show?` excluded: not an action).
fn is_action_name(a: &str) -> bool {
    !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !a.starts_with(|c: char| c.is_ascii_digit())
}

/// Literal (non-parameter) segments of a route path, optional groups ignored.
fn literal_segments(path: &str) -> usize {
    let mut depth = 0usize;
    let mut required = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            c if depth == 0 => required.push(c),
            _ => {}
        }
    }
    required.split('/').filter(|s| !s.is_empty() && !s.contains([':', '*'])).count()
}

fn bump(stats: &mut BTreeMap<String, u64>, key: impl Into<String>) {
    *stats.entry(key.into()).or_default() += 1;
}

impl BoundaryLinker {
    pub fn new() -> Self {
        Self::default()
    }

    /// The route table from the last `link`, in definition (= match) order.
    pub fn routes(&self) -> &[Route] {
        &self.table.routes
    }

    pub fn route_table(&self) -> &RouteTable {
        &self.table
    }

    /// Routes that can be Rails' first match for `verb path` (verb `None` =
    /// any). Literal paths yield at most one route per verb.
    pub fn match_path(&self, verb: Option<&str>, path: &str) -> Vec<&Route> {
        let verb = verb.map(str::to_ascii_uppercase);
        self.matcher.matches(&self.table.routes, verb.as_deref(), path).into_iter().map(|h| &self.table.routes[h.route]).collect()
    }

    /// Every mount found in the last `link`, with its resolution.
    pub fn mounts(&self) -> &[MountRecord] {
        &self.mounts
    }

    /// Per-call-site results of the last `link`, parallel to `facts.js_call_sites`.
    pub fn call_matches(&self) -> &[CallMatch] {
        &self.calls
    }

    /// Re-reads route sources as needed; returns whether the table must be rebuilt.
    fn refresh_routes(&mut self, ws: &Workspace, changes: Option<&ChangeSet>) -> bool {
        let patterns = glob_set(&ws.config.boundary.routes);
        let current: BTreeSet<&RelPath> = ws.files.iter().filter(|f| f.ends_with(".rb") && f.contains("routes") && matches_any(&patterns, f)).collect();
        let changed: HashSet<&str> = changes.map(|c| c.modified.iter().chain(&c.removed).map(String::as_str).collect()).unwrap_or_default();
        let full = changes.is_none() || !self.built;
        let mut dirty = full;
        self.route_sources.retain(|p, _| {
            let keep = current.contains(p);
            dirty |= !keep;
            keep
        });
        for p in current {
            if full || changed.contains(p.as_str()) || !self.route_sources.contains_key(p) {
                let src = std::fs::read(ws.abs(p)).unwrap_or_default();
                self.route_sources.insert(p.clone(), src);
                dirty = true;
            }
        }
        if full || changed.contains("config/application.rb") {
            let a = api_only(ws);
            dirty |= a != self.api_only;
            self.api_only = a;
        }
        dirty
    }

    /// Updates the per-file scan caches for the current tree: drops entries
    /// for changed/removed/vanished files, then scans whatever is missing.
    /// Registrations (`ReactOnRails.register`) are only scanned while some
    /// template mounts a component by name, since that reads every JS file.
    fn refresh_scans(&mut self, ws: &Workspace, changes: Option<&ChangeSet>) {
        if changes.is_none() || !self.built {
            self.mount_scans.clear();
            self.registrations.clear();
        } else if let Some(c) = changes {
            for p in c.modified.iter().chain(&c.removed) {
                self.mount_scans.remove(p);
                self.registrations.remove(p);
            }
        }
        let present: HashSet<&str> = ws.files.iter().map(String::as_str).collect();
        self.mount_scans.retain(|p, _| present.contains(p.as_str()));
        self.registrations.retain(|p, _| present.contains(p.as_str()));
        for f in ws.files.iter().filter(|f| mounts::templates::is_candidate(f)) {
            if !self.mount_scans.contains_key(f) {
                let found = read_text(ws, f)
                    .map(|t| {
                        let mut found = mounts::templates::scan(&t, f);
                        if f.ends_with("_controller.rb") && !found.is_empty() {
                            let actions = mounts::actions::public_actions(t.as_bytes());
                            for m in &mut found {
                                m.action = mounts::actions::action_at(&actions, m.line).map(str::to_string);
                            }
                        }
                        found
                    })
                    .unwrap_or_default();
                self.mount_scans.insert(f.clone(), found);
            }
        }
        let need_registrations = self.mount_scans.values().flatten().any(|m| m.kind == MountKind::ReactComponent);
        if need_registrations {
            for f in ws.files.iter().filter(|f| mounts::resolve::is_js(f) && !mounts::resolve::is_test_like(f)) {
                if !self.registrations.contains_key(f) {
                    let regs = read_text(ws, f).map(|t| mounts::resolve::scan_registrations(&t)).unwrap_or_default();
                    self.registrations.insert(f.clone(), regs);
                }
            }
        } else {
            self.registrations.clear();
        }
    }

    fn route_edges(&self, controllers: &HashMap<String, Vec<RelPath>>, out: &mut ExtractOutput) {
        let mut seen: HashSet<(&str, &str)> = HashSet::new();
        let mut missing: BTreeMap<&str, &Route> = BTreeMap::new();
        let mut resolved: BTreeSet<&str> = BTreeSet::new();
        for r in &self.table.routes {
            bump(&mut out.stats, format!("routes.file.{}", r.file));
            match &r.kind {
                RouteKind::Action => {}
                RouteKind::Redirect => {
                    bump(&mut out.stats, "routes.redirect");
                    continue;
                }
                RouteKind::Rack => {
                    bump(&mut out.stats, "routes.rack");
                    continue;
                }
                RouteKind::Mount { .. } => {
                    bump(&mut out.stats, "routes.mount");
                    continue;
                }
            }
            let Some(files) = controllers.get(&r.controller) else {
                missing.entry(&r.controller).or_insert(r);
                continue;
            };
            resolved.insert(&r.controller);
            for f in files {
                if seen.insert((&r.file, f)) {
                    out.edges.push(Edge::new(&r.file, f, EdgeKind::RailsRoute, Confidence::Exact).at(r.line).detail(format!("{} {} → {}", r.verb, r.path, r.endpoint())));
                }
            }
        }
        out.stats.insert("routes.total".into(), self.table.routes.len() as u64);
        out.stats.insert("routes.controllers_resolved".into(), resolved.len() as u64);
        out.stats.insert("routes.controllers_unresolved".into(), missing.len() as u64);
        out.stats.insert("routes.unreached_files".into(), self.table.unreached.len() as u64);
        let total: u64 = self.table.uninterpreted.values().sum();
        out.stats.insert("routes.uninterpreted".into(), total);
        for (method, n) in &self.table.uninterpreted {
            out.stats.insert(format!("routes.uninterpreted.{method}"), *n);
        }
        out.warnings.extend(self.table.warnings.iter().cloned());
        for (i, (c, r)) in missing.iter().enumerate() {
            if i == WARNING_SAMPLE {
                out.warnings.push(Warning::new("boundary_controller_missing", format!("... and {} more controllers without a file", missing.len() - WARNING_SAMPLE)));
                break;
            }
            out.warnings.push(Warning::new("boundary_controller_missing", format!("{}:{}: no app/controllers/{c}_controller.rb for {}", r.file, r.line, r.endpoint())).file(r.file.clone()));
        }
    }

    fn call_edges(&mut self, ws: &Workspace, facts: &Facts, controllers: &HashMap<String, Vec<RelPath>>, out: &mut ExtractOutput) {
        let routes = &self.table.routes;
        let link_edges = ws.config.boundary.link_edges;
        self.calls = facts.js_call_sites.iter().enumerate().map(|(i, s)| calls::match_call(i, s, &self.table, &self.matcher, &ws.config.boundary)).collect();
        let (mut unmatched, mut ambiguous) = (0usize, 0usize);
        out.stats.insert("calls.total".into(), self.calls.len() as u64);
        out.stats.insert("calls.skipped_links".into(), 0);
        for m in &self.calls {
            let site = &facts.js_call_sites[m.site];
            // A page that only links to a URL does not depend on its controller.
            if site.link && !link_edges {
                bump(&mut out.stats, "calls.skipped_links");
                continue;
            }
            let label = || format!("{}:{}: {} {} ({})", site.file, site.line, site.method.as_deref().unwrap_or("ANY"), site.url, site.callee);
            match &m.outcome {
                CallOutcome::Matched(c) => {
                    bump(&mut out.stats, format!("calls.{}", mounts::confidence_label(*c)));
                    let confidence = if site.link { Confidence::Heuristic } else { *c };
                    let prefix = if site.link { "link: " } else { "" };
                    for &ri in &m.routes {
                        if !controllers.contains_key(&routes[ri].controller) {
                            bump(&mut out.stats, "calls.controller_missing");
                        }
                    }
                    for &ri in &m.actions {
                        let r = &routes[ri];
                        let Some(files) = controllers.get(&r.controller) else { continue };
                        for f in files {
                            let mut e = Edge::new(&site.file, f, EdgeKind::ApiCall, confidence).at(site.line).detail(format!("{prefix}{} {} → {}", r.verb, r.path, r.endpoint()));
                            if is_action_name(&r.action) {
                                e = e.to_scope(&r.action);
                            }
                            out.edges.push(e);
                        }
                    }
                }
                CallOutcome::Ambiguous(n) => {
                    bump(&mut out.stats, "calls.ambiguous");
                    if ambiguous < WARNING_SAMPLE {
                        out.warnings.push(Warning::new("boundary_call_ambiguous", format!("{} matches {n} controllers; not linked", label())).file(site.file.clone()));
                    }
                    ambiguous += 1;
                }
                CallOutcome::NonController => bump(&mut out.stats, "calls.non_controller"),
                CallOutcome::Filtered => bump(&mut out.stats, "calls.filtered"),
                CallOutcome::Unmatched => {
                    bump(&mut out.stats, "calls.unmatched");
                    if unmatched < WARNING_SAMPLE {
                        out.warnings.push(Warning::new("boundary_call_unmatched", format!("{} matches no route", label())).file(site.file.clone()));
                    }
                    unmatched += 1;
                }
            }
        }
        if unmatched > WARNING_SAMPLE {
            out.warnings.push(Warning::new("boundary_call_unmatched", format!("... and {} more unmatched call sites", unmatched - WARNING_SAMPLE)));
        }
    }

    /// Ruby tests -> the controller actions their requests are routed to
    /// (`EdgeKind::ApiCall`, scoped to the action): `get users_path` in a
    /// request spec, `visit "/settings"` in a system spec. Same matching as
    /// JS call sites; unmatched requests (external URLs, Grape APIs, engines)
    /// are only counted, since tests routinely hit URLs Rails does not route.
    fn request_edges(&self, ws: &Workspace, facts: &Facts, controllers: &HashMap<String, Vec<RelPath>>, out: &mut ExtractOutput) {
        let routes = &self.table.routes;
        out.stats.insert("requests.total".into(), facts.http_requests.len() as u64);
        for (i, site) in facts.http_requests.iter().enumerate() {
            let m = calls::match_call(i, site, &self.table, &self.matcher, &ws.config.boundary);
            let c = match m.outcome {
                CallOutcome::Matched(c) => c,
                CallOutcome::Ambiguous(_) => {
                    bump(&mut out.stats, "requests.ambiguous");
                    continue;
                }
                CallOutcome::NonController => {
                    bump(&mut out.stats, "requests.non_controller");
                    continue;
                }
                CallOutcome::Unmatched | CallOutcome::Filtered => {
                    bump(&mut out.stats, "requests.unmatched");
                    continue;
                }
            };
            bump(&mut out.stats, format!("requests.{}", mounts::confidence_label(c)));
            // A literal segment in the test's path was written for the route
            // that has it literally: `/library` means `library#index`, not
            // the host-constrained `/:username` that also matches it.
            let best = m.actions.iter().map(|&ri| literal_segments(&routes[ri].path)).max().unwrap_or(0);
            for &ri in m.actions.iter().filter(|&&ri| literal_segments(&routes[ri].path) == best) {
                let r = &routes[ri];
                let Some(files) = controllers.get(&r.controller) else { continue };
                for f in files {
                    let mut e = Edge::new(&site.file, f, EdgeKind::ApiCall, c).at(site.line).detail(format!("{} {} → {}", site.callee, site.url, r.endpoint()));
                    if is_action_name(&r.action) {
                        e = e.to_scope(&r.action);
                    }
                    out.edges.push(e);
                }
            }
        }
    }

    fn mount_edges(&mut self, ws: &Workspace, controllers: &HashMap<String, Vec<RelPath>>, out: &mut ExtractOutput) {
        let config = BundlerConfig::read(ws);
        let index = JsIndex::new(&ws.files, &config);
        let mut registry: Registry = BTreeMap::new();
        let mut reg_files: Vec<&RelPath> = self.registrations.keys().collect();
        reg_files.sort();
        for f in reg_files {
            for (name, spec) in &self.registrations[f] {
                if let Some(target) = index.resolve_specifier(f, spec) {
                    let entry = registry.entry(name.clone()).or_default();
                    if !entry.contains(&target) {
                        entry.push(target);
                    }
                }
            }
        }
        let mut files: Vec<&RelPath> = self.mount_scans.iter().filter(|(_, v)| !v.is_empty()).map(|(k, _)| k).collect();
        files.sort();
        self.mounts.clear();
        let mut unresolved = 0usize;
        for f in files {
            for m in &self.mount_scans[f] {
                let outcome = index.resolve(m, &registry);
                bump(&mut out.stats, "mounts.found");
                bump(&mut out.stats, format!("mounts.kind.{}", serde_json::to_value(m.kind).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()));
                match &outcome {
                    Outcome::Resolved(targets, c) => {
                        bump(&mut out.stats, "mounts.resolved");
                        // A template's props come from the controller action rendering it.
                        let owner = mounts::actions::template_action(f).and_then(|(prefix, ctrl, action)| {
                            let files: Vec<&RelPath> = controllers.get(ctrl)?.iter().filter(|c| c.starts_with(prefix) && c[prefix.len()..].starts_with("app/controllers/")).collect();
                            (!files.is_empty()).then_some((files, ctrl, action))
                        });
                        for t in targets {
                            let mut mounts_edge = Edge::new(f, t, EdgeKind::ViewMount, *c).at(m.line).detail(format!("mounts {}", m.name));
                            let mut props_edge = Edge::new(t, f, EdgeKind::ViewMount, *c).detail(format!("props from {f}"));
                            if let Some(a) = &m.action {
                                mounts_edge = mounts_edge.from_scope(a);
                                props_edge = props_edge.to_scope(a);
                            }
                            out.edges.push(mounts_edge);
                            out.edges.push(props_edge);
                            if let Some((files, ctrl, action)) = &owner {
                                for cf in files {
                                    for a in mounts::actions::rendering_actions(action) {
                                        let conf = (*c).min(Confidence::Inferred);
                                        out.edges.push(Edge::new(t, *cf, EdgeKind::ViewMount, conf).detail(format!("props from {ctrl}#{a} via {}", m.name)).to_scope(a));
                                    }
                                }
                            }
                        }
                    }
                    Outcome::NoTarget => bump(&mut out.stats, "mounts.no_target"),
                    Outcome::Dynamic => bump(&mut out.stats, "mounts.dynamic"),
                    Outcome::Ambiguous(_) | Outcome::Unresolved => {
                        bump(&mut out.stats, if matches!(outcome, Outcome::Ambiguous(_)) { "mounts.ambiguous" } else { "mounts.unresolved" });
                        if unresolved < WARNING_SAMPLE {
                            out.warnings.push(Warning::new("boundary_mount_unresolved", format!("{f}:{}: {} {:?} resolves to no JS file", m.line, m.helper, m.name)).file(f.clone()));
                        }
                        unresolved += 1;
                    }
                }
                self.mounts.push(MountRecord { file: f.clone(), mount: m.clone(), outcome });
            }
        }
        let actions: HashSet<(&str, &str)> = self.table.routes.iter().map(|r| (r.controller.as_str(), r.action.as_str())).collect();
        for (ruby, js, label, action) in mounts::page_bundles(&ws.files, controllers, &actions, &index) {
            bump(&mut out.stats, "mounts.found");
            bump(&mut out.stats, "mounts.resolved");
            bump(&mut out.stats, "mounts.kind.page_bundle");
            match &action {
                // An action bundle is loaded for that action (GitLab also loads
                // `new` for `create` and `edit` for `update`).
                Some(a) => {
                    for a in mounts::actions::rendering_actions(a) {
                        out.edges.push(Edge::new(&ruby, &js, EdgeKind::ViewMount, Confidence::Inferred).detail(format!("mounts {label}")).from_scope(a));
                        out.edges.push(Edge::new(&js, &ruby, EdgeKind::ViewMount, Confidence::Inferred).detail(format!("props from {ruby}")).to_scope(a));
                    }
                }
                None => {
                    out.edges.push(Edge::new(&ruby, &js, EdgeKind::ViewMount, Confidence::Inferred).detail(format!("mounts {label}")));
                    out.edges.push(Edge::new(&js, &ruby, EdgeKind::ViewMount, Confidence::Inferred).detail(format!("props from {ruby}")));
                }
            }
            let mount = MountRef { kind: MountKind::PageBundle, helper: "webpack_controller_bundle_tags".into(), name: label, line: 0, action };
            self.mounts.push(MountRecord { file: ruby, mount, outcome: Outcome::Resolved(vec![js], Confidence::Inferred) });
        }
    }
}

impl Linker for BoundaryLinker {
    fn name(&self) -> &'static str {
        "boundary"
    }

    fn link(&mut self, ws: &Workspace, facts: &Facts, changes: Option<&ChangeSet>) -> anyhow::Result<ExtractOutput> {
        let mut out = ExtractOutput::default();
        if !ws.config.boundary.enabled {
            return Ok(out);
        }
        if self.refresh_routes(ws, changes) {
            let sources: Vec<(RelPath, Vec<u8>)> = self.route_sources.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            self.table = routes::build(&sources, self.api_only);
            self.matcher = Matcher::new(&self.table.routes);
        }
        let controllers = controller_index(&ws.files);
        self.refresh_scans(ws, changes);
        self.built = true;

        self.route_edges(&controllers, &mut out);
        self.call_edges(ws, facts, &controllers, &mut out);
        self.request_edges(ws, facts, &controllers, &mut out);
        self.mount_edges(ws, &controllers, &mut out);

        out.edges.sort_by(|a, b| (&a.from, &a.to, a.kind, a.line, &a.detail, &a.from_scope, &a.to_scope).cmp(&(&b.from, &b.to, b.kind, b.line, &b.detail, &b.from_scope, &b.to_scope)));
        out.edges.dedup();
        Ok(out)
    }
}
