//! The static Rails route table: what `rails routes` would print, computed by
//! interpreting the routing DSL over the prism AST instead of booting the app.

mod dsl;
pub(crate) mod path;
mod value;

use ripple_core::{RelPath, Warning};
use serde::Serialize;
use std::collections::BTreeMap;

/// What a route dispatches to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum RouteKind {
    /// A controller action (`controller`/`action` are set).
    Action,
    /// `to: redirect(...)`.
    Redirect,
    /// A lambda, proc or other Rack endpoint given as `to:`.
    Rack,
    /// `mount App, at: "/path"`: everything under `path` belongs to `app`,
    /// whose own routes are not known statically.
    Mount { app: String },
}

/// One row of the route table. One row per HTTP verb.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Route {
    /// Upper-case verb, or `ANY` for `via: :all` and mounts.
    pub verb: String,
    /// Normalised path with `:param`, `*glob` and `( )` optional groups;
    /// the `(.:format)` suffix is dropped.
    pub path: String,
    /// Controller path, e.g. `api/v1/statuses`; empty unless `kind` is `Action`.
    pub controller: String,
    pub action: String,
    /// Route name as used by `<name>_path` helpers.
    pub name: Option<String>,
    pub file: RelPath,
    pub line: u32,
    pub kind: RouteKind,
    /// The route has segment or request constraints, so it may not match every
    /// request its path pattern matches. Such routes never shadow later ones.
    pub constrained: bool,
}

impl Route {
    pub fn endpoint(&self) -> String {
        match &self.kind {
            RouteKind::Action => format!("{}#{}", self.controller, self.action),
            RouteKind::Redirect => "redirect".to_string(),
            RouteKind::Rack => "rack app".to_string(),
            RouteKind::Mount { app } => format!("mount {app}"),
        }
    }
}

/// The interpreted route table plus everything that could not be interpreted.
#[derive(Debug, Clone, Default)]
pub struct RouteTable {
    pub routes: Vec<Route>,
    pub warnings: Vec<Warning>,
    /// DSL method name -> number of calls that were not interpreted.
    pub uninterpreted: BTreeMap<String, u64>,
    /// Route files that were never drawn from the entry file.
    pub unreached: Vec<RelPath>,
    /// Extra helper names defined with `direct` from existing route names:
    /// helper name -> index of the route it stands for (see [`helper_aliases`]).
    pub helper_aliases: std::collections::HashMap<String, usize>,
    /// Route name -> first route with that name.
    pub by_name: std::collections::HashMap<String, usize>,
}

impl RouteTable {
    /// The route a `<name>_path` helper resolves to: a named route, else a
    /// [`helper_aliases`] entry.
    pub fn named(&self, name: &str) -> Option<usize> {
        self.by_name.get(name).or_else(|| self.helper_aliases.get(name)).copied()
    }
}

/// `routes.set...name...each { |name| direct(name.sub('namespace_project', 'project')) ... }`
/// (GitLab): every route whose name contains the first string also gets a
/// helper with that part replaced. Recognised textually, only in a route file
/// that calls `direct`, as `.sub('a', 'b')` / `.sub("a", "b")` with literal
/// snake_case arguments.
fn helper_aliases(files: &[(RelPath, Vec<u8>)], routes: &[Route]) -> std::collections::HashMap<String, usize> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r#"\.sub\(\s*['"]([a-z][a-z0-9_]*)['"]\s*,\s*['"]([a-z][a-z0-9_]*)['"]\s*\)"#).expect("regex"));
    let mut out = std::collections::HashMap::new();
    for (_, src) in files {
        let text = String::from_utf8_lossy(src);
        if !text.contains("direct(") {
            continue;
        }
        for c in re.captures_iter(&text) {
            let (from, to) = (&c[1], &c[2]);
            for (i, r) in routes.iter().enumerate() {
                let Some(name) = r.name.as_deref().filter(|n| n.contains(from)) else { continue };
                out.entry(name.replacen(from, to, 1)).or_insert(i);
            }
        }
    }
    let named: std::collections::HashSet<&str> = routes.iter().filter_map(|r| r.name.as_deref()).collect();
    out.retain(|k, _| !named.contains(k.as_str()));
    out
}

/// Interprets `files` (path, source) starting from the entry file
/// (`config/routes.rb`, or every file named `routes.rb` when that is absent).
/// `api_only` mirrors `config.api_only` (no `new`/`edit` resource actions).
pub fn build(files: &[(RelPath, Vec<u8>)], api_only: bool) -> RouteTable {
    let mut table = dsl::interpret(files, api_only);
    for (i, r) in table.routes.iter().enumerate() {
        if let Some(n) = &r.name {
            table.by_name.entry(n.clone()).or_insert(i);
        }
    }
    table.helper_aliases = helper_aliases(files, &table.routes);
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r##"Rails.application.routes.draw do
  get "/:ns/:id", to: "projects#show", as: :namespace_project
  get "/:ns/:project_id/issues", to: "projects/issues#index", as: :namespace_project_issues
  get "/explore", to: "explore#index", as: :explore_project
  Gitlab::Application.routes.set.filter_map { |route| route.name if route.name&.include?('namespace_project') }.each do |name|
    new_name = name.sub('namespace_project', 'project')
    direct(new_name) { |project, *args| send("#{name}_url", project&.namespace, project, *args) }
  end
end
"##;

    #[test]
    fn direct_sub_aliases_name_the_original_route() {
        let t = build(&[("config/routes.rb".into(), SRC.as_bytes().to_vec())], false);
        let ctrl = |n: &str| t.named(n).map(|i| t.routes[i].endpoint());
        assert_eq!(ctrl("project").as_deref(), Some("projects#show"));
        assert_eq!(ctrl("project_issues").as_deref(), Some("projects/issues#index"));
        // A real route name always wins over an alias; unknown names stay unknown.
        assert_eq!(ctrl("explore_project").as_deref(), Some("explore#index"));
        assert_eq!(ctrl("project_wikis"), None);
    }

    #[test]
    fn no_direct_no_aliases() {
        let src = SRC.replace("direct(new_name)", "puts(new_name)");
        let t = build(&[("config/routes.rb".into(), src.into_bytes())], false);
        assert!(t.helper_aliases.is_empty());
    }
}
