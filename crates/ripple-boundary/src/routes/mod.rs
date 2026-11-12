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
}

/// Interprets `files` (path, source) starting from the entry file
/// (`config/routes.rb`, or every file named `routes.rb` when that is absent).
/// `api_only` mirrors `config.api_only` (no `new`/`edit` resource actions).
pub fn build(files: &[(RelPath, Vec<u8>)], api_only: bool) -> RouteTable {
    dsl::interpret(files, api_only)
}
