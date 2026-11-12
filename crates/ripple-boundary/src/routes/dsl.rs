//! Interpreter for `ActionDispatch::Routing::Mapper`, written against Rails 7/8
//! semantics. It walks the prism AST with a value-semantics [`Scope`] that
//! mirrors Rails' scope frames (path, module, controller, `as` name prefix,
//! shallow path/prefix, scope level and parent resource).
//!
//! Notable semantics reproduced here, because they decide paths and names:
//!
//! * `resources` evaluates the user block *before* adding its default actions,
//!   so custom collection routes precede `:id` routes (first match wins).
//! * A verb called directly inside a `resources` block is *nested*
//!   (`/photos/:photo_id/x`); directly inside a singular `resource` block it is
//!   a *member* route (`/profile/x`).
//! * `resource :profile` routes to the *plural* controller (`profiles`).
//! * `shallow` nesting drops the parent path for member routes, keeping only
//!   the immediate parent for collection routes.
//! * Conditionals are interpreted as the union of their branches unless the
//!   condition is a literal we can evaluate (loop variables), because the
//!   route table must cover every environment.

use super::path::{join, normalize, normalize_name};
use super::value::{const_name, eval, eval_args, Args, Env, Val};
use super::{Route, RouteKind, RouteTable};
use crate::inflect::{pluralize, singularize};
use ripple_core::{RelPath, Warning};
use ruby_prism::{BlockNode, DefNode, Node};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;

const CANONICAL_ACTIONS: &[&str] = &["index", "create", "new", "show", "update", "destroy"];
const RESOURCE_OPTIONS: &[&str] = &["as", "controller", "path", "only", "except", "param", "concerns"];
const MAX_DEPTH: usize = 24;

/// Calls whose block is plain pass-through for routing purposes.
const PASS_THROUGH_BLOCKS: &[&str] = &["defaults", "devise_scope", "as", "authenticate", "authenticated", "unauthenticated"];
/// Macro DSLs that generate routes we do not model. Their blocks are not walked
/// because they use their own DSL (`controllers ...`).
const MACROS: &[&str] = &[
    "devise_for",
    "use_doorkeeper",
    "use_doorkeeper_openid_connect",
    "use_doorkeeper_device_authorization_grant",
    "mount_devise_token_auth_for",
    "mount_graphql_devise_for",
    "active_admin_routes",
    "comfy_route",
    "jsonapi_resources",
    "rails_admin",
];
/// Calls that never define routes and are silently skipped.
const NOT_ROUTES: &[&str] =
    &["require", "require_relative", "require_dependency", "direct", "resolve", "define_singleton_method", "define_method", "puts", "raise", "redirect", "lambda", "proc", "freeze"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Level {
    #[default]
    Plain,
    Resources,
    Resource,
    Nested,
    Member,
    Collection,
    New,
    Root,
}

#[derive(Debug)]
struct Resource {
    /// `as:` if given, else the entity name.
    name: String,
    path: String,
    controller: String,
    param: String,
    singleton: bool,
    actions: Vec<String>,
    shallow: bool,
}

impl Resource {
    fn new(entity: &str, args: &Args, singleton: bool, shallow: bool, api_only: bool, scope_actions: Option<&(Option<Val>, Option<Val>)>) -> Self {
        let name = args.text("as").unwrap_or_else(|| entity.to_string());
        let path = args.text("path").unwrap_or_else(|| entity.to_string());
        let controller = args.text("controller").unwrap_or_else(|| if singleton { pluralize(entity) } else { entity.to_string() });
        let param = args.text("param").unwrap_or_else(|| "id".to_string());
        let defaults: &[&str] = match (singleton, api_only) {
            (false, false) => &["index", "create", "new", "show", "update", "destroy", "edit"],
            (false, true) => &["index", "create", "show", "update", "destroy"],
            (true, false) => &["show", "create", "update", "destroy", "new", "edit"],
            (true, true) => &["show", "create", "update", "destroy"],
        };
        let (mut only, mut except) = (args.get("only").cloned(), args.get("except").cloned());
        if only.is_none() && except.is_none()
            && let Some((o, e)) = scope_actions {
                only = o.clone();
                except = e.clone();
            }
        let mut actions: Vec<String> = match only {
            Some(v) => v.texts(),
            None => defaults.iter().map(|s| s.to_string()).collect(),
        };
        if let Some(except) = except {
            let except = except.texts();
            actions.retain(|a| !except.contains(a));
        }
        Resource { name, path, controller, param, singleton, actions, shallow }
    }

    fn has(&self, action: &str) -> bool {
        self.actions.iter().any(|a| a == action)
    }
    fn plural(&self) -> String {
        if self.singleton { pluralize(&self.name) } else { self.name.clone() }
    }
    fn singular(&self) -> String {
        if self.singleton { self.name.clone() } else { singularize(&self.name) }
    }
    fn member_name(&self) -> String {
        self.singular()
    }
    fn collection_name(&self) -> String {
        if self.singleton {
            return self.singular();
        }
        let (s, p) = (self.singular(), self.plural());
        if s == p { format!("{p}_index") } else { p }
    }
    fn member_scope(&self) -> String {
        if self.singleton { self.path.clone() } else { format!("{}/:{}", self.path, self.param) }
    }
    fn nested_scope(&self) -> String {
        if self.singleton { self.path.clone() } else { format!("{}/:{}_{}", self.path, self.singular(), self.param) }
    }
}

/// Canvas' `ApiRouteSet.draw(self, prefix)`: verbs get `prefix/` prepended
/// and route names `name_prefix`; `resources` has its own semantics.
#[derive(Debug)]
struct ApiSet {
    prefix: String,
    name_prefix: String,
}

#[derive(Clone, Default)]
struct Scope {
    path: String,
    shallow_path: Option<String>,
    as_: Option<String>,
    shallow_prefix: Option<String>,
    module: Option<String>,
    controller: Option<String>,
    action: Option<String>,
    path_names: Vec<(String, String)>,
    shallow: bool,
    shallow_depth: usize,
    level: Level,
    resource: Option<Rc<Resource>>,
    /// `with_options` options merged under every call in the block.
    defaults: Vec<(Val, Val)>,
    via: Option<Vec<String>>,
    constrained: bool,
    action_options: Option<(Option<Val>, Option<Val>)>,
    api_set: Option<Rc<ApiSet>>,
}

fn merge_as(parent: Option<&str>, child: &str) -> String {
    match parent {
        Some(p) if !p.is_empty() => format!("{p}_{child}"),
        _ => child.to_string(),
    }
}

fn is_action_like(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '/')
}

/// `get "photos/search"` with no `to:` means `photos#search`.
fn to_from_path(path: &str) -> Option<String> {
    let p = path.strip_suffix("(.:format)").unwrap_or(path);
    let body = p.strip_prefix('/').unwrap_or(p);
    let (head, last) = body.rsplit_once('/')?;
    let word = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if word(head.split('/').next().unwrap_or("")) && head.split('/').all(word) && last.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        Some(format!("{head}#{last}").replace('-', "_"))
    } else {
        None
    }
}

fn verbs_of(v: &Val) -> Vec<String> {
    let mut out: Vec<String> = v.texts().iter().map(|s| if s == "all" { "ANY".to_string() } else { s.to_uppercase() }).collect();
    out.dedup();
    out
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

enum ParamPat {
    Name(String),
    Multi(Vec<String>),
}

fn block_params(block: &BlockNode<'_>) -> Vec<ParamPat> {
    let Some(params) = block.parameters().and_then(|p| p.as_block_parameters_node()).and_then(|p| p.parameters()) else { return Vec::new() };
    params
        .requireds()
        .iter()
        .filter_map(|r| match &r {
            Node::RequiredParameterNode { .. } => Some(ParamPat::Name(text(r.as_required_parameter_node()?.name().as_slice()))),
            Node::MultiTargetNode { .. } => Some(ParamPat::Multi(
                r.as_multi_target_node()?
                    .lefts()
                    .iter()
                    .filter_map(|l| l.as_required_parameter_node().map(|p| text(p.name().as_slice())))
                    .collect(),
            )),
            _ => None,
        })
        .collect()
}

fn bind(env: &mut Env, pat: &ParamPat, val: Val) {
    match pat {
        ParamPat::Name(n) => {
            env.insert(n.clone(), val);
        }
        ParamPat::Multi(names) => {
            let items = val.list();
            for (i, n) in names.iter().enumerate() {
                env.insert(n.clone(), items.get(i).cloned().unwrap_or(Val::Nil));
            }
        }
    }
}

struct Interp<'pr> {
    programs: HashMap<String, Node<'pr>>,
    /// `config/routes`, then any other `**/config/routes` dir holding route files.
    draw_dirs: Vec<String>,
    api_only: bool,
    scope: Scope,
    env: Env,
    concerns: HashMap<String, BlockNode<'pr>>,
    defs: HashMap<String, DefNode<'pr>>,
    /// Block-parameter names that alias the mapper (`with_options(...) do |r| r.get ...`).
    mapper_aliases: HashSet<String>,
    file: RelPath,
    depth: usize,
    names: HashSet<String>,
    reached: BTreeSet<RelPath>,
    out: RouteTable,
}

pub fn interpret(files: &[(RelPath, Vec<u8>)], api_only: bool) -> RouteTable {
    let parsed: Vec<_> = files.iter().map(|(_, src)| ruby_prism::parse(src)).collect();
    let mut programs = HashMap::new();
    let mut warnings = Vec::new();
    for ((path, _), result) in files.iter().zip(&parsed) {
        if let Some(err) = result.errors().next() {
            let line = err.location().start_line();
            warnings.push(Warning::new("boundary_route_parse_error", format!("{path}:{line}: Ruby syntax error: {}", String::from_utf8_lossy(err.message()))).file(path.clone()));
        }
        programs.insert(path.clone(), result.node());
    }
    let mut draw_dirs: Vec<String> = Vec::new();
    for (path, _) in files {
        if let Some(i) = path.find("config/routes/") {
            let dir = path[..i + "config/routes".len()].to_string();
            if !draw_dirs.contains(&dir) {
                draw_dirs.push(dir);
            }
        }
    }
    draw_dirs.sort_by_key(|d| (d != "config/routes", d.clone()));
    let entries: Vec<RelPath> = if programs.contains_key("config/routes.rb") {
        vec!["config/routes.rb".to_string()]
    } else {
        let mut e: Vec<RelPath> = files.iter().map(|(p, _)| p.clone()).filter(|p| p.ends_with("routes.rb")).collect();
        e.sort();
        e
    };
    let mut it = Interp {
        programs,
        draw_dirs,
        api_only,
        scope: Scope::default(),
        env: Env::new(),
        concerns: HashMap::new(),
        defs: HashMap::new(),
        mapper_aliases: HashSet::new(),
        file: String::new(),
        depth: 0,
        names: HashSet::new(),
        reached: BTreeSet::new(),
        out: RouteTable { warnings, ..RouteTable::default() },
    };
    for entry in entries {
        it.walk_file(&entry);
    }
    let mut unreached: Vec<RelPath> = files.iter().map(|(p, _)| p.clone()).filter(|p| !it.reached.contains(p)).collect();
    unreached.sort();
    for p in &unreached {
        it.out.warnings.push(Warning::new("boundary_route_file_unreached", format!("{p}: route file is never drawn from config/routes.rb")).file(p.clone()));
    }
    it.out.unreached = unreached;
    it.out
}

impl<'pr> Interp<'pr> {
    fn warn(&mut self, code: &str, method: &str, line: u32, msg: impl AsRef<str>) {
        *self.out.uninterpreted.entry(method.to_string()).or_default() += 1;
        let file = self.file.clone();
        self.out.warnings.push(Warning::new(code, format!("{file}:{line}: {}", msg.as_ref())).file(file));
    }

    fn walk_file(&mut self, path: &str) {
        let Some(program) = self.programs.get(path).and_then(|p| p.as_program_node()) else { return };
        if self.depth >= MAX_DEPTH {
            return;
        }
        let saved_file = std::mem::replace(&mut self.file, path.to_string());
        self.reached.insert(path.to_string());
        self.depth += 1;
        self.walk(&program.statements().as_node());
        self.depth -= 1;
        self.file = saved_file;
    }

    fn with_scope(&mut self, scope: Scope, f: impl FnOnce(&mut Self)) {
        let saved = std::mem::replace(&mut self.scope, scope);
        f(self);
        self.scope = saved;
    }

    fn walk_block(&mut self, block: Option<&BlockNode<'pr>>) {
        if let Some(body) = block.and_then(|b| b.body()) {
            if self.depth >= MAX_DEPTH {
                return;
            }
            self.depth += 1;
            self.walk(&body);
            self.depth -= 1;
        }
    }

    fn walk(&mut self, node: &Node<'pr>) {
        match node {
            Node::StatementsNode { .. } => {
                if let Some(s) = node.as_statements_node() {
                    for stmt in s.body().iter() {
                        self.walk(&stmt);
                    }
                }
            }
            Node::BeginNode { .. } => {
                let Some(b) = node.as_begin_node() else { return };
                if let Some(s) = b.statements() {
                    self.walk(&s.as_node());
                }
                let mut rescue = b.rescue_clause();
                while let Some(r) = rescue {
                    if let Some(s) = r.statements() {
                        self.walk(&s.as_node());
                    }
                    rescue = r.subsequent();
                }
                if let Some(s) = b.else_clause().and_then(|e| e.statements()) {
                    self.walk(&s.as_node());
                }
                if let Some(s) = b.ensure_clause().and_then(|e| e.statements()) {
                    self.walk(&s.as_node());
                }
            }
            Node::ParenthesesNode { .. } => {
                if let Some(body) = node.as_parentheses_node().and_then(|p| p.body()) {
                    self.walk(&body);
                }
            }
            Node::IfNode { .. } => {
                let Some(n) = node.as_if_node() else { return };
                let cond = eval(&n.predicate(), &self.env).truthy();
                if cond != Some(false)
                    && let Some(s) = n.statements() {
                        self.walk(&s.as_node());
                    }
                if cond != Some(true)
                    && let Some(sub) = n.subsequent() {
                        self.walk(&sub);
                    }
            }
            Node::ElseNode { .. } => {
                if let Some(s) = node.as_else_node().and_then(|e| e.statements()) {
                    self.walk(&s.as_node());
                }
            }
            Node::UnlessNode { .. } => {
                let Some(n) = node.as_unless_node() else { return };
                let cond = eval(&n.predicate(), &self.env).truthy();
                if cond != Some(true)
                    && let Some(s) = n.statements() {
                        self.walk(&s.as_node());
                    }
                if cond != Some(false)
                    && let Some(s) = n.else_clause().and_then(|e| e.statements()) {
                        self.walk(&s.as_node());
                    }
            }
            Node::CaseNode { .. } => {
                let Some(n) = node.as_case_node() else { return };
                for when in n.conditions().iter() {
                    if let Some(s) = when.as_when_node().and_then(|w| w.statements()) {
                        self.walk(&s.as_node());
                    }
                }
                if let Some(s) = n.else_clause().and_then(|e| e.statements()) {
                    self.walk(&s.as_node());
                }
            }
            Node::LocalVariableWriteNode { .. } => {
                if let Some(w) = node.as_local_variable_write_node() {
                    let v = eval(&w.value(), &self.env);
                    self.env.insert(text(w.name().as_slice()), v);
                }
            }
            Node::ConstantWriteNode { .. } => {
                if let Some(w) = node.as_constant_write_node() {
                    let v = eval(&w.value(), &self.env);
                    self.env.insert(text(w.name().as_slice()), v);
                }
            }
            Node::DefNode { .. } => {
                if let Some(d) = node.as_def_node()
                    && d.receiver().is_none() {
                        self.defs.insert(text(d.name().as_slice()), d);
                    }
            }
            Node::CallNode { .. } => {
                if let Some(c) = node.as_call_node() {
                    self.call(&c);
                }
            }
            // Class/module bodies, literals, ivar writes, ...: not routing.
            _ => {}
        }
    }

    fn call(&mut self, call: &ruby_prism::CallNode<'pr>) {
        let name = text(call.name().as_slice());
        let line = call.location().start_line().max(0) as u32;
        let block = call.block().and_then(|b| b.as_block_node());
        if let Some(recv) = call.receiver() {
            let aliased = recv.as_local_variable_read_node().is_some_and(|r| self.mapper_aliases.contains(&text(r.name().as_slice())));
            if !aliased {
                self.receiver_call(call, &recv, &name, block.as_ref(), line);
                return;
            }
        }
        match name.as_str() {
            "get" | "post" | "put" | "patch" | "delete" | "options" | "head" => self.verb(call, Some(&name), line),
            "match" => self.verb(call, None, line),
            "root" => self.root(call, line),
            "resources" | "resource" => {
                let mut args = eval_args(call.arguments(), &self.env);
                args.merge_defaults(&self.scope.defaults);
                let names: Vec<String> = args.pos.iter().flat_map(|v| v.texts()).collect();
                if names.is_empty() {
                    self.warn("boundary_route_dynamic", &name, line, format!("{name} with a non-literal name"));
                    return;
                }
                if self.scope.api_set.is_some() && name == "resources" {
                    self.api_set_resources(&names, args, line);
                } else {
                    self.resources(names, args, block.as_ref(), name == "resource", line);
                }
            }
            "namespace" => self.namespace(call, block.as_ref(), line),
            "scope" | "controller" | "constraints" => {
                let mut args = eval_args(call.arguments(), &self.env);
                args.merge_defaults(&self.scope.defaults);
                match name.as_str() {
                    "controller" => {
                        if let Some(c) = args.pos.first().and_then(|v| v.text().map(str::to_string)) {
                            args.pos.clear();
                            args.set("controller", Val::Str(c));
                        }
                    }
                    "constraints" => {
                        let c = args.pos.pop().unwrap_or(Val::Hash(args.opts.clone()));
                        args = Args { opts: vec![(Val::Sym("constraints".into()), c)], ..Args::default() };
                    }
                    _ => {}
                }
                let s = self.scope_from(args);
                self.with_scope(s, |me| me.walk_block(block.as_ref()));
            }
            "with_options" => {
                let args = eval_args(call.arguments(), &self.env);
                let mut s = self.scope.clone();
                let mut merged = args.opts.clone();
                for (k, v) in &self.scope.defaults {
                    if !merged.iter().any(|(own, _)| own == k) {
                        merged.push((k.clone(), v.clone()));
                    }
                }
                s.defaults = merged;
                if args.opts.iter().any(|(k, _)| matches!(k, Val::Sym(s) if s == "constraints")) {
                    s.constrained = true;
                }
                let alias = block.as_ref().map(block_params).and_then(|p| match p.into_iter().next() {
                    Some(ParamPat::Name(n)) => Some(n),
                    _ => None,
                });
                let added = alias.as_ref().is_some_and(|a| self.mapper_aliases.insert(a.clone()));
                self.with_scope(s, |me| me.walk_block(block.as_ref()));
                if added {
                    self.mapper_aliases.remove(alias.as_deref().unwrap_or_default());
                }
            }
            "member" | "collection" | "new" => match self.level_scope(&name) {
                Some(s) => self.with_scope(s, |me| me.walk_block(block.as_ref())),
                None => self.warn("boundary_route_misplaced", &name, line, format!("`{name}` outside a resource scope")),
            },
            "nested" => match self.nested_scope() {
                Some(s) => self.with_scope(s, |me| me.walk_block(block.as_ref())),
                None => self.warn("boundary_route_misplaced", &name, line, "`nested` outside a resource scope"),
            },
            "shallow" => {
                let mut s = self.scope.clone();
                s.shallow = true;
                self.with_scope(s, |me| me.walk_block(block.as_ref()));
            }
            "concern" => {
                let args = eval_args(call.arguments(), &self.env);
                match (args.pos.first().and_then(|v| v.text().map(str::to_string)), block) {
                    (Some(n), Some(b)) => {
                        self.concerns.insert(n, b);
                    }
                    _ => self.warn("boundary_route_uninterpreted", "concern", line, "concern without a literal name and block"),
                }
            }
            "concerns" => {
                let args = eval_args(call.arguments(), &self.env);
                let names: Vec<String> = args.pos.iter().flat_map(|v| v.texts()).collect();
                self.apply_concerns(&names, line);
            }
            "draw" | "draw_all" => {
                let args = eval_args(call.arguments(), &self.env);
                match args.pos.first().and_then(|v| v.text().map(str::to_string)) {
                    Some(n) => self.draw(&n, name == "draw_all", line),
                    None => self.warn("boundary_route_dynamic", &name, line, "draw with a non-literal name"),
                }
            }
            "mount" => self.mount(call, line),
            n if PASS_THROUGH_BLOCKS.contains(&n) => self.walk_block(block.as_ref()),
            n if MACROS.contains(&n) => self.warn("boundary_route_uninterpreted", n, line, format!("`{n}` generates routes that are not modelled")),
            n if NOT_ROUTES.contains(&n) => {}
            n if self.defs.contains_key(n) => self.inline_def(call, n, line),
            n => {
                if block.is_some() {
                    self.warn("boundary_route_unknown_block", n, line, format!("unknown block call `{n}`; its block was walked as pass-through"));
                    self.walk_block(block.as_ref());
                } else {
                    self.warn("boundary_route_uninterpreted", n, line, format!("unknown routing call `{n}`"));
                }
            }
        }
    }

    fn receiver_call(&mut self, call: &ruby_prism::CallNode<'pr>, recv: &Node<'pr>, name: &str, block: Option<&BlockNode<'pr>>, line: u32) {
        let recv_name = |r: &Node<'pr>| match r {
            Node::CallNode { .. } => r.as_call_node().map(|c| text(c.name().as_slice())).unwrap_or_default(),
            _ => const_name(r).unwrap_or_default(),
        };
        let rname = recv_name(recv);
        match name {
            "draw" if rname == "routes" && block.is_some() => self.walk_block(block),
            "draw" if rname.contains("ApiRouteSet") && block.is_some() => {
                let args = eval_args(call.arguments(), &self.env);
                let (prefix, name_prefix) = match args.pos.get(1).and_then(|v| v.text().map(str::to_string)) {
                    Some(p) => (p, String::new()),
                    None if rname.ends_with("V1") => ("/api/v1".to_string(), "api_v1_".to_string()),
                    None => {
                        self.warn("boundary_route_dynamic", "ApiRouteSet.draw", line, "ApiRouteSet without a prefix");
                        return;
                    }
                };
                let mut s = self.scope.clone();
                s.api_set = Some(Rc::new(ApiSet { prefix, name_prefix }));
                self.with_scope(s, |me| me.walk_block(block));
            }
            "each" | "each_with_index" | "each_pair" if block.is_some() => {
                let items = match eval(recv, &self.env) {
                    Val::Array(v) => v,
                    Val::Hash(pairs) => pairs.into_iter().map(|(k, v)| Val::Array(vec![k, v])).collect(),
                    _ => {
                        self.warn("boundary_route_dynamic", "each", line, "loop over a non-literal collection");
                        return;
                    }
                };
                let Some(block) = block else { return };
                let params = block_params(block);
                let saved = self.env.clone();
                for (i, item) in items.into_iter().enumerate() {
                    match (name, params.as_slice()) {
                        ("each_with_index", [first, second, ..]) => {
                            bind(&mut self.env, first, item);
                            bind(&mut self.env, second, Val::Int(i as i64));
                        }
                        (_, [only]) => bind(&mut self.env, only, item),
                        (_, many) => {
                            let parts = item.list();
                            for (j, p) in many.iter().enumerate() {
                                bind(&mut self.env, p, parts.get(j).cloned().unwrap_or(Val::Nil));
                            }
                        }
                    }
                    self.walk_block(Some(block));
                }
                self.env = saved;
            }
            // GitLab edition blocks (`Gitlab.ee do ... end`): conditional, so union.
            "ee" | "jh" | "com" | "saas" if block.is_some() => self.walk_block(block),
            _ if block.is_some() => {
                let label = format!("{rname}.{name}");
                self.warn("boundary_route_unknown_block", &label, line, format!("unknown block call `{label}`; its block was walked as pass-through"));
                self.walk_block(block);
            }
            _ => {}
        }
    }

    // ---- scope construction -------------------------------------------------

    /// `scope(...)`: positional path, `path:`, `module:`, `as:`, `controller:`, ...
    fn scope_from(&self, mut args: Args) -> Scope {
        let mut s = self.scope.clone();
        if !args.pos.is_empty() {
            let p: Vec<String> = args.pos.iter().flat_map(|v| v.texts()).collect();
            args.set("path", Val::Str(p.join("/")));
        }
        if s.level != Level::Nested {
            if let (Some(p), false) = (args.get("path").cloned(), args.has("shallow_path")) {
                args.set("shallow_path", p);
            }
            if let (Some(a), false) = (args.get("as").cloned(), args.has("shallow_prefix")) {
                args.set("shallow_prefix", a);
            }
        }
        for (k, v) in &args.opts {
            let Val::Sym(key) = k else { continue };
            let t = v.text().map(str::to_string);
            match key.as_str() {
                "path" => s.path = join(&s.path, t.as_deref().unwrap_or("")),
                "shallow_path" => s.shallow_path = Some(join(s.shallow_path.as_deref().unwrap_or(""), t.as_deref().unwrap_or(""))),
                "as" => {
                    if let Some(t) = t {
                        s.as_ = Some(merge_as(s.as_.as_deref(), &t));
                    }
                }
                "shallow_prefix" => {
                    if let Some(t) = t {
                        s.shallow_prefix = Some(merge_as(s.shallow_prefix.as_deref(), &t));
                    }
                }
                "module" => {
                    if let Some(t) = t {
                        s.module = Some(match &s.module {
                            Some(m) => format!("{m}/{t}"),
                            None => t,
                        });
                    }
                }
                "controller" => s.controller = t,
                "action" => s.action = t,
                "shallow" => s.shallow = v.truthy().unwrap_or(false),
                "via" => s.via = Some(verbs_of(v)),
                "path_names" => {
                    if let Val::Hash(pairs) = v {
                        for (pk, pv) in pairs {
                            if let (Some(a), Some(b)) = (pk.text(), pv.text()) {
                                s.path_names.push((a.to_string(), b.to_string()));
                            }
                        }
                    }
                }
                "constraints"
                    if !matches!(v, Val::Hash(h) if h.is_empty()) => {
                        s.constrained = true;
                    }
                _ => {}
            }
        }
        if args.has("only") || args.has("except") {
            s.action_options = Some((args.get("only").cloned(), args.get("except").cloned()));
        }
        s
    }

    fn is_shallow(&self) -> bool {
        self.scope.shallow && self.scope.resource.as_ref().is_some_and(|r| !r.singleton)
    }

    fn reset_to_shallow(s: &mut Scope) {
        s.path = s.shallow_path.clone().unwrap_or_default();
        s.as_ = s.shallow_prefix.clone();
    }

    fn in_resource_scope(&self) -> bool {
        matches!(self.scope.level, Level::Resources | Level::Resource)
    }

    /// `member` / `collection` / `new` scopes. Only valid directly inside a resource.
    fn level_scope(&self, which: &str) -> Option<Scope> {
        if !self.in_resource_scope() {
            return None;
        }
        let res = self.scope.resource.clone()?;
        let mut s = self.scope.clone();
        match which {
            "member" => {
                s.level = Level::Member;
                if self.is_shallow() {
                    Self::reset_to_shallow(&mut s);
                }
                s.path = join(&s.path, &res.member_scope());
            }
            "collection" => {
                s.level = Level::Collection;
                s.path = join(&s.path, &res.path);
            }
            "new" => {
                s.level = Level::New;
                s.path = join(&s.path, &format!("{}/{}", res.path, self.action_path("new")));
            }
            _ => return None,
        }
        Some(s)
    }

    fn nested_scope(&self) -> Option<Scope> {
        if !self.in_resource_scope() {
            return None;
        }
        let res = self.scope.resource.clone()?;
        let mut s = self.scope.clone();
        s.level = Level::Nested;
        if self.is_shallow() && s.shallow_depth >= 1 {
            Self::reset_to_shallow(&mut s);
        }
        s.path = join(&s.path, &res.nested_scope());
        s.as_ = Some(merge_as(s.as_.as_deref(), &res.member_name()));
        Some(s)
    }

    fn action_path(&self, action: &str) -> String {
        self.scope.path_names.iter().rev().find(|(k, _)| k == action).map_or_else(|| action.to_string(), |(_, v)| v.clone())
    }

    // ---- DSL methods --------------------------------------------------------

    fn namespace(&mut self, call: &ruby_prism::CallNode<'pr>, block: Option<&BlockNode<'pr>>, line: u32) {
        let mut args = eval_args(call.arguments(), &self.env);
        args.merge_defaults(&self.scope.defaults);
        let Some(name) = args.pos.first().and_then(|v| v.text().map(str::to_string)) else {
            self.warn("boundary_route_dynamic", "namespace", line, "namespace with a non-literal name");
            return;
        };
        args.pos.clear();
        let path = args.take("path").and_then(|v| v.text().map(str::to_string)).unwrap_or_else(|| name.clone());
        let as_ = if args.has("as") { args.get("as").and_then(|v| v.text().map(str::to_string)) } else { Some(name.clone()) };
        if !args.has("module") {
            args.set("module", Val::Str(name.clone()));
        }
        if !args.has("shallow_path") {
            args.set("shallow_path", Val::Str(path.clone()));
        }
        if !args.has("shallow_prefix") {
            args.set("shallow_prefix", as_.clone().map_or(Val::Nil, Val::Str));
        }
        args.take("as");
        if let Some(a) = as_ {
            args.set("as", Val::Str(a));
        }
        let mut s = self.scope.clone();
        s.path = join(&s.path, &path);
        let s = {
            let saved = std::mem::replace(&mut self.scope, s);
            let scoped = self.scope_from(args);
            self.scope = saved;
            scoped
        };
        self.with_scope(s, |me| me.walk_block(block));
    }

    fn resources(&mut self, names: Vec<String>, mut args: Args, block: Option<&BlockNode<'pr>>, singleton: bool, line: u32) {
        if names.len() > 1 {
            for n in names {
                self.resources(vec![n], args.clone(), block, singleton, line);
            }
            return;
        }
        if args.get("shallow").and_then(Val::truthy) == Some(true) {
            args.take("shallow");
            let mut s = self.scope.clone();
            s.shallow = true;
            self.with_scope(s, |me| me.resources(names, args, block, singleton, line));
            return;
        }
        if self.in_resource_scope() {
            if let Some(s) = self.nested_scope() {
                self.with_scope(s, |me| me.resources(names, args, block, singleton, line));
            }
            return;
        }
        // Regexp-valued options are segment constraints; everything that is not
        // a resource option becomes an enclosing `scope`.
        let mut scope_args = Args::default();
        let mut keep = Vec::new();
        for (k, v) in std::mem::take(&mut args.opts) {
            match &k {
                Val::Sym(key) if RESOURCE_OPTIONS.contains(&key.as_str()) => keep.push((k, v)),
                _ if v == Val::Regex => scope_args.opts.push((Val::Sym("constraints".into()), Val::Hash(vec![(k, v)]))),
                Val::Sym(_) => scope_args.opts.push((k, v)),
                _ => {}
            }
        }
        args.opts = keep;
        if !scope_args.opts.is_empty() {
            let s = self.scope_from(scope_args);
            self.with_scope(s, |me| me.resources(names, args, block, singleton, line));
            return;
        }
        let entity = &names[0];
        let res = Rc::new(Resource::new(entity, &args, singleton, self.scope.shallow, self.api_only, self.scope.action_options.as_ref()));
        let mut s = self.scope.clone();
        s.level = if singleton { Level::Resource } else { Level::Resources };
        s.controller = Some(res.controller.clone());
        s.shallow_depth += usize::from(res.shallow);
        s.resource = Some(res.clone());
        let concerns = args.get("concerns").map(Val::texts).unwrap_or_default();
        self.with_scope(s, |me| {
            me.walk_block(block);
            me.apply_concerns(&concerns, line);
            let defaults = |me: &mut Self, which: &str, verbs: &[(&str, &str)]| {
                let todo: Vec<&(&str, &str)> = verbs.iter().filter(|(_, a)| res.has(a)).collect();
                if todo.is_empty() {
                    return;
                }
                if let Some(sc) = me.level_scope(which) {
                    me.with_scope(sc, |me| {
                        for (verb, action) in todo {
                            me.internal(verb, action, line);
                        }
                    });
                }
            };
            if singleton {
                defaults(me, "new", &[("get", "new")]);
                defaults(me, "member", &[("get", "edit"), ("get", "show"), ("patch", "update"), ("put", "update"), ("delete", "destroy")]);
                defaults(me, "collection", &[("post", "create")]);
            } else {
                defaults(me, "collection", &[("get", "index"), ("post", "create")]);
                defaults(me, "new", &[("get", "new")]);
                defaults(me, "member", &[("get", "edit"), ("get", "show"), ("patch", "update"), ("put", "update"), ("delete", "destroy")]);
            }
        });
    }

    /// A route generated by `resources` itself (no `with_options` merging).
    fn internal(&mut self, verb: &str, action: &str, line: u32) {
        let mut args = Args::default();
        args.set("via", Val::Sym(verb.to_string()));
        self.map_match(vec![Val::Sym(action.to_string())], args, line);
    }

    fn apply_concerns(&mut self, names: &[String], line: u32) {
        for n in names {
            match self.concerns.get(n).map(|b| b.as_node()) {
                Some(node) => {
                    if let Some(b) = node.as_block_node() {
                        self.walk_block(Some(&b));
                    }
                }
                None => self.warn("boundary_route_uninterpreted", "concerns", line, format!("unknown concern `{n}`")),
            }
        }
    }

    fn draw(&mut self, name: &str, all: bool, line: u32) {
        let candidates: Vec<String> = self.draw_dirs.iter().map(|d| format!("{d}/{name}.rb")).filter(|p| self.programs.contains_key(p)).collect();
        if candidates.is_empty() {
            self.warn("boundary_route_draw_missing", "draw", line, format!("draw :{name}: no {name}.rb among the configured route files"));
            return;
        }
        let take = if all { candidates.len() } else { 1 };
        for path in candidates.into_iter().take(take) {
            self.walk_file(&path);
        }
    }

    fn mount(&mut self, call: &ruby_prism::CallNode<'pr>, line: u32) {
        let args = eval_args(call.arguments(), &self.env);
        let app_name = |v: &Val| match v {
            Val::Const(c) => c.clone(),
            _ => "app".to_string(),
        };
        let (app, at) = match (args.pos.first(), args.text("at")) {
            (Some(app), Some(at)) => (app_name(app), at),
            _ => match args.opts.iter().find(|(k, v)| !matches!(k, Val::Sym(_)) && v.text().is_some()) {
                Some((k, v)) => (app_name(k), v.text().unwrap_or_default().to_string()),
                None => {
                    // `mount Foo::Engine => "/foo"` where the key is a method
                    // call evaluates to Unknown; recover the path from the args.
                    let at = call
                        .arguments()
                        .and_then(|a| a.arguments().iter().find_map(|n| n.as_keyword_hash_node()))
                        .and_then(|h| h.elements().iter().find_map(|e| e.as_assoc_node()))
                        .map(|a| eval(&a.value(), &self.env));
                    match at.as_ref().and_then(Val::text) {
                        Some(at) => ("app".to_string(), at.to_string()),
                        None => {
                            self.warn("boundary_route_uninterpreted", "mount", line, "mount without a literal `at:` path");
                            return;
                        }
                    }
                }
            },
        };
        let path = join(&self.scope.path, &at);
        // `as:` or, for `Foo::Engine`, the engine's default name (`foo`);
        // other rack apps are unnamed.
        let as_ = match args.get("as") {
            Some(v) => v.text().map(str::to_string),
            None => app.strip_suffix("::Engine").map(|m| underscore(m.rsplit("::").next().unwrap_or(m))),
        };
        let name = as_.and_then(|a| self.name_for(Some(&a), None));
        if let Some(n) = &name {
            self.names.insert(n.clone());
        }
        self.push(Route {
            verb: "ANY".into(),
            path,
            controller: String::new(),
            action: String::new(),
            name,
            file: self.file.clone(),
            line,
            kind: RouteKind::Mount { app },
            constrained: self.scope.constrained,
        });
    }

    fn inline_def(&mut self, call: &ruby_prism::CallNode<'pr>, name: &str, line: u32) {
        let Some(def) = self.defs.get(name).map(|d| d.as_node()) else { return };
        let Some(def) = def.as_def_node() else { return };
        if self.depth >= MAX_DEPTH {
            self.warn("boundary_route_uninterpreted", name, line, "recursion limit reached");
            return;
        }
        let args = eval_args(call.arguments(), &self.env);
        let saved = self.env.clone();
        if let Some(params) = def.parameters() {
            let mut pos = args.pos.iter();
            for r in params.requireds().iter() {
                if let Some(p) = r.as_required_parameter_node() {
                    self.env.insert(text(p.name().as_slice()), pos.next().cloned().unwrap_or(Val::Unknown));
                }
            }
            for o in params.optionals().iter() {
                if let Some(p) = o.as_optional_parameter_node() {
                    let v = pos.next().cloned().unwrap_or_else(|| eval(&p.value(), &saved));
                    self.env.insert(text(p.name().as_slice()), v);
                }
            }
            for k in params.keywords().iter() {
                if let Some(p) = k.as_required_keyword_parameter_node() {
                    let n = text(p.name().as_slice());
                    self.env.insert(n.clone(), args.get(&n).cloned().unwrap_or(Val::Unknown));
                } else if let Some(p) = k.as_optional_keyword_parameter_node() {
                    let n = text(p.name().as_slice());
                    let v = args.get(&n).cloned().unwrap_or_else(|| eval(&p.value(), &saved));
                    self.env.insert(n, v);
                }
            }
        }
        if let Some(body) = def.body() {
            self.depth += 1;
            self.walk(&body);
            self.depth -= 1;
        }
        self.env = saved;
    }

    fn root(&mut self, call: &ruby_prism::CallNode<'pr>, line: u32) {
        let mut args = eval_args(call.arguments(), &self.env);
        args.merge_defaults(&self.scope.defaults);
        if let Some(to) = args.pos.first().cloned() {
            args.pos.clear();
            if to.text().is_some() {
                args.set("to", to);
            }
        }
        if !args.has("as") {
            args.set("as", Val::Sym("root".into()));
        }
        if !args.has("via") {
            args.set("via", Val::Sym("get".into()));
        }
        if self.in_resource_scope() {
            let Some(res) = self.scope.resource.clone() else { return };
            let mut s = self.scope.clone();
            s.level = Level::Root;
            s.path = join(&s.path, &res.path);
            self.with_scope(s, |me| me.map_match(vec![Val::Str("/".into())], args, line));
        } else {
            self.map_match(vec![Val::Str("/".into())], args, line);
        }
    }

    /// `get`/`post`/.../`match`.
    fn verb(&mut self, call: &ruby_prism::CallNode<'pr>, verb: Option<&str>, line: u32) {
        let mut args = eval_args(call.arguments(), &self.env);
        args.merge_defaults(&self.scope.defaults);
        if let Some(v) = verb {
            args.set("via", Val::Sym(v.to_string()));
        }
        let mut paths: Vec<Val> = Vec::new();
        if args.pos.is_empty() {
            match args.string_key_pair() {
                Some((path, to)) => {
                    args.remove_string_keys();
                    match &to {
                        Val::Str(s) if s.contains('#') => args.set("to", to.clone()),
                        Val::Str(_) => args.set("controller", to.clone()),
                        Val::Sym(_) => args.set("action", to.clone()),
                        _ => args.set("to", to.clone()),
                    }
                    paths.push(Val::Str(path));
                }
                None => {
                    let label = verb.unwrap_or("match");
                    self.warn("boundary_route_dynamic", label, line, format!("`{label}` without a literal path"));
                    return;
                }
            }
        } else {
            for p in std::mem::take(&mut args.pos) {
                match p {
                    Val::Str(_) | Val::Sym(_) => paths.push(p),
                    Val::Array(items) => paths.extend(items.into_iter().filter(|i| i.text().is_some())),
                    _ => {
                        let label = verb.unwrap_or("match");
                        self.warn("boundary_route_dynamic", label, line, format!("`{label}` with a non-literal path"));
                    }
                }
            }
        }
        if let Some(set) = self.scope.api_set.clone() {
            // ApiRouteSet#route: "#{prefix}/#{path}", `as` (or `path_name`) prefixed.
            paths = paths.into_iter().filter_map(|p| p.text().map(|t| Val::Str(format!("{}/{t}", set.prefix)))).collect();
            let as_ = args.take("as").or_else(|| args.take("path_name"));
            if let Some(a) = as_.as_ref().and_then(Val::text) {
                args.set("as", Val::Str(format!("{}{a}", set.name_prefix)));
            } else {
                // ApiRouteSet routes without `as` get no automatic name.
                args.set("as", Val::Nil);
            }
        }
        if args.get("via").is_none() {
            self.warn("boundary_route_uninterpreted", "match", line, "`match` without `via:`; treated as any verb");
            args.set("via", Val::Sym("all".into()));
        }
        self.map_match(paths, args, line);
    }

    /// Canvas `ApiRouteSet#resources`.
    fn api_set_resources(&mut self, names: &[String], args: Args, line: u32) {
        for name in names {
            let mut a = args.clone();
            let path = a.take("path").and_then(|v| v.text().map(str::to_string)).unwrap_or_else(|| name.clone());
            let name_prefix = a.take("name_prefix").and_then(|v| v.text().map(str::to_string)).unwrap_or_default();
            let only = a.take("only").map(|v| v.texts());
            let except = a.take("except").map(|v| v.texts()).unwrap_or_default();
            let want = |x: &str| only.as_ref().is_none_or(|o| o.iter().any(|s| s == x)) && !except.iter().any(|s| s == x);
            let single = singularize(name);
            let member = format!("{path}/:{single}_id");
            let emit = |me: &mut Self, verb: &str, p: &str, action: &str, as_: Option<String>| {
                let mut r = a.clone();
                r.set("action", Val::Sym(action.into()));
                r.set("as", as_.map_or(Val::Nil, Val::Str));
                r.set("via", Val::Sym(verb.into()));
                let set = me.scope.api_set.clone();
                let prefixed = set.as_ref().map_or_else(|| p.to_string(), |s| format!("{}/{p}", s.prefix));
                if let (Some(s), Some(Val::Str(n))) = (set.as_ref(), r.get("as").cloned()) {
                    r.set("as", Val::Str(format!("{}{n}", s.name_prefix)));
                }
                me.map_match(vec![Val::Str(prefixed)], r, line);
            };
            if want("index") {
                emit(self, "get", &path, "index", Some(format!("{name_prefix}{name}")));
            }
            if want("show") {
                emit(self, "get", &member, "show", Some(format!("{name_prefix}{single}")));
            }
            if want("create") {
                emit(self, "post", &path, "create", (!want("index")).then(|| format!("{name_prefix}{name}")));
            }
            if want("update") {
                emit(self, "put", &member, "update", None);
            }
            if want("destroy") {
                emit(self, "delete", &member, "destroy", None);
            }
        }
    }

    /// `Mapper#map_match`: resolves `to:`/`controller:`/`via:` and dispatches
    /// each path to [`Self::decomposed_match`].
    fn map_match(&mut self, paths: Vec<Val>, mut args: Args, line: u32) {
        let on = args.take("on").and_then(|v| v.text().map(str::to_string));
        if let (Some(c), Some(a), false) = (&self.scope.controller, &self.scope.action, args.has("to")) {
            args.set("to", Val::Str(format!("{c}#{a}")));
        }
        let controller = args.take("controller").and_then(|v| v.text().map(str::to_string)).or_else(|| self.scope.controller.clone());
        let option_path = args.take("path").and_then(|v| v.text().map(str::to_string));
        let to = args.take("to");
        let via = args.take("via").map(|v| verbs_of(&v)).or_else(|| self.scope.via.clone()).unwrap_or_else(|| vec!["ANY".into()]);
        for p in paths {
            match p {
                Val::Str(path) => {
                    let to = to.clone().or_else(|| if args.has("action") { None } else { to_from_path(&path).map(Val::Str) });
                    self.decomposed_match(on.as_deref(), &path, controller.clone(), &args, Some(path.clone()), to, &via, line);
                }
                Val::Sym(action) => self.decomposed_match(on.as_deref(), &action, controller.clone(), &args, option_path.clone(), to.clone(), &via, line),
                _ => {}
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn decomposed_match(&mut self, on: Option<&str>, action: &str, controller: Option<String>, args: &Args, path: Option<String>, to: Option<Val>, via: &[String], line: u32) {
        if let Some(on) = on {
            match self.level_scope(on) {
                Some(s) => self.with_scope(s, |me| me.decomposed_match(None, action, controller, args, path, to, via, line)),
                None => self.warn("boundary_route_misplaced", on, line, format!("`on: :{on}` outside a resource scope")),
            }
            return;
        }
        match self.scope.level {
            Level::Resources => {
                if let Some(s) = self.nested_scope() {
                    self.with_scope(s, |me| me.decomposed_match(None, action, controller, args, path, to, via, line));
                }
            }
            Level::Resource => {
                if let Some(s) = self.level_scope("member") {
                    self.with_scope(s, |me| me.decomposed_match(None, action, controller, args, path, to, via, line));
                }
            }
            _ => self.add_route(action, controller, args, path, to, via, line),
        }
    }

    fn canonical(&self, action: &str) -> bool {
        matches!(self.scope.level, Level::Collection | Level::Member | Level::New) && CANONICAL_ACTIONS.contains(&action)
    }

    /// `Mapper#name_for_action`.
    fn name_for(&self, as_: Option<&str>, action: Option<&str>) -> Option<String> {
        let prefix = match (as_, action) {
            (Some(a), _) => Some(a.to_string()),
            (None, Some(a)) if !self.canonical(a) => Some(a.to_string()),
            _ => None,
        }
        .filter(|p| !p.is_empty() && p != "/")
        .map(|p| normalize_name(&p.replace('-', "_")));
        let name_prefix = self.scope.as_.clone();
        let (collection, member) = match &self.scope.resource {
            Some(r) => {
                if as_.is_none() && action.is_none() {
                    return None;
                }
                (Some(r.collection_name()), Some(r.member_name()))
            }
            None => (None, None),
        };
        let parts: Vec<Option<String>> = match self.scope.level {
            Level::Nested => vec![name_prefix, prefix],
            Level::Collection => vec![prefix, name_prefix, collection],
            Level::New => vec![prefix, Some("new".into()), name_prefix, member],
            Level::Member => vec![prefix, name_prefix, member],
            Level::Root => vec![name_prefix, collection, prefix],
            _ => vec![name_prefix, member, prefix],
        };
        let candidate = parts.into_iter().flatten().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("_");
        if candidate.is_empty() || self.names.contains(&candidate) {
            return None;
        }
        let valid = candidate.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
        (as_.is_some() || valid).then_some(candidate)
    }

    #[allow(clippy::too_many_arguments)]
    fn add_route(&mut self, action: &str, controller: Option<String>, args: &Args, path: Option<String>, to: Option<Val>, via: &[String], line: u32) {
        let raw_path = match &path {
            Some(p) => format!("{}/{p}", self.scope.path),
            None if self.canonical(action) => self.scope.path.clone(),
            None => format!("{}/{}", self.scope.path, self.action_path(action)),
        };
        let action_ok = is_action_like(action);
        let mut default_action = args.text("action").or_else(|| self.scope.action.clone());
        if action_ok && !action.contains('/') && default_action.is_none() {
            default_action = Some(action.replace('-', "_"));
        }
        let name = match args.get("as") {
            Some(Val::Nil | Val::Bool(false)) => None,
            Some(v) => match v.text() {
                Some(a) => self.name_for(Some(a), action_ok.then_some(action)),
                None => None,
            },
            None => self.name_for(None, action_ok.then_some(action)),
        };
        let path = normalize(&raw_path);
        let (kind, endpoint) = match &to {
            Some(Val::Redirect) => (RouteKind::Redirect, None),
            Some(Val::Str(s)) if s.contains('#') => {
                let (c, a) = s.split_once('#').unwrap_or_default();
                let c = if c.is_empty() { controller.clone() } else { Some(c.to_string()) };
                (RouteKind::Action, Some((c, Some(a.to_string()))))
            }
            // Rails 7 `split_to`: a `to:` without `#` contributes nothing.
            Some(Val::Str(_) | Val::Sym(_)) | None => (RouteKind::Action, Some((controller.clone(), default_action))),
            Some(_) => (RouteKind::Rack, None),
        };
        // `Mapping#add_controller_module`: the scope module is prepended unless
        // the controller is absolute (`/oauth/applications`); with no
        // controller at all, the module itself is the controller
        // (`namespace :events do post 'installed' end` -> `events#installed`).
        let (controller, action) = match endpoint {
            None => (String::new(), String::new()),
            Some((c, Some(a))) if !a.is_empty() && (c.is_some() || self.scope.module.is_some()) => {
                let module = self.scope.module.as_deref();
                let c = match c {
                    Some(c) => match (module, c.strip_prefix('/')) {
                        (_, Some(abs)) => abs.to_string(),
                        (Some(m), None) => format!("{m}/{c}"),
                        (None, None) => c,
                    },
                    None => module.unwrap_or_default().to_string(),
                };
                (c, a)
            }
            Some(_) => {
                self.warn("boundary_route_no_endpoint", "match", line, format!("route {path} has no static controller#action"));
                return;
            }
        };
        let constrained = self.scope.constrained || args.has("constraints") || args.opts.iter().any(|(_, v)| *v == Val::Regex);
        if let Some(n) = &name {
            self.names.insert(n.clone());
        }
        for verb in via {
            self.push(Route {
                verb: verb.clone(),
                path: path.clone(),
                controller: controller.clone(),
                action: action.clone(),
                name: name.clone(),
                file: self.file.clone(),
                line,
                kind: kind.clone(),
                constrained,
            });
        }
    }

    fn push(&mut self, route: Route) {
        self.out.routes.push(route);
    }
}

/// `ActiveSupport::Inflector#underscore` for a single constant segment.
fn underscore(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
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
}
