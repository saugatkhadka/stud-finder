//! Turns per-file facts plus the resolved Rubydex graph into file edges.
//!
//! This runs over every file on every build/update. It only does hash
//! lookups (no parsing), so recomputing everything keeps `update` trivially
//! equal to a fresh `build`: any file's edges can change when *another* file
//! adds or removes a definition, and recomputing avoids tracking that.

use crate::actions::{self, Action};
use crate::analyse::FileEntry;
use crate::inflector::Inflector;
use crate::paths::{self, SourceKind};
use crate::rails::{ClassName, RequireKind};
use crate::views::{RenderKind, ViewIndex};
use ripple_core::{Confidence, Edge, EdgeKind};
use rubydex::model::graph::Graph;
use rubydex::model::ids::{DeclarationId, NameId};
use rubydex::model::name::{NameRef, ParentScope};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

/// Rubydex's document for core classes; every other document is a repo file.
const BUILTIN_URI: &str = "rubydex:built-in";

#[derive(Debug, Default)]
pub struct EdgeStats {
    pub refs_total: u64,
    pub refs_resolved: u64,
    pub refs_unresolved: u64,
    /// Resolved to a declaration with no definition in the repo (gems, core).
    pub refs_external: u64,
    /// Leaf unresolved, edge emitted to the deepest resolved enclosing constant.
    pub refs_prefix_fallback: u64,
    /// Unresolved by Rubydex, found as a top-level (`Object`) constant.
    pub refs_top_level_fallback: u64,
    /// Declarations over the namespace limit that resolved to their canonical file.
    pub namespace_canonical: u64,
    /// ...and those with no canonical file (no edge).
    pub namespace_no_canonical: u64,
    pub refs_to_namespace_without_canonical: u64,
    pub inferred_unresolved: u64,
    pub requires_unresolved: u64,
    pub renders_unresolved: u64,
    pub template_consts_unresolved: u64,
}

/// Resolution targets for one declaration, after the namespace policy.
struct Targets<'g> {
    files: Vec<&'g str>,
    name: &'g str,
    /// Over the limit with no canonical file.
    dropped: bool,
}

pub struct EdgeBuilder<'a> {
    graph: &'a Graph,
    files: &'a BTreeMap<String, FileEntry>,
    limit: usize,
    inflector: Inflector,
    views: ViewIndex<'a>,
    all_files: HashSet<&'a str>,
    lib_roots: Vec<String>,
    /// Shared so cache hits are a refcount bump, not a `Vec` clone.
    cache: HashMap<DeclarationId, Option<Rc<Targets<'a>>>>,
    pub stats: EdgeStats,
    out: Vec<Edge>,
    /// Per-`from` dedup of (to, kind, from_scope) -> index into `out`.
    seen: HashMap<(&'a str, EdgeKind, Option<&'a str>), usize>,
    /// Action ranges of the current `from` (empty unless it is a controller).
    actions: &'a [Action],
    /// `ruby.method_level`: mark superclass / mixin references as inheritance.
    method_level: bool,
}

impl<'a> EdgeBuilder<'a> {
    pub fn new(
        graph: &'a Graph,
        files: &'a BTreeMap<String, FileEntry>,
        all_files: &'a [String],
        limit: usize,
        inflector: Inflector,
        method_level: bool,
    ) -> Self {
        let views = ViewIndex::new(
            files
                .iter()
                .filter(|(_, e)| matches!(e.kind, Some(SourceKind::Template(_))))
                .map(|(p, _)| p.as_str()),
        );
        let mut lib_roots: Vec<String> = files
            .keys()
            .filter_map(|p| {
                let i = if p.starts_with("lib/") {
                    Some(0)
                } else {
                    p.find("/lib/").map(|i| i + 1)
                }?;
                Some(p[..i + 4].to_string())
            })
            .collect();
        lib_roots.sort();
        lib_roots.dedup();
        Self {
            graph,
            files,
            limit,
            inflector,
            views,
            all_files: all_files.iter().map(String::as_str).collect(),
            lib_roots,
            cache: HashMap::new(),
            stats: EdgeStats::default(),
            out: Vec::new(),
            seen: HashMap::new(),
            actions: &[],
            method_level,
        }
    }

    /// Edges and stats.
    pub fn run(mut self) -> (Vec<Edge>, EdgeStats) {
        let files = self.files;
        for (path, entry) in files.iter() {
            self.seen.clear();
            self.actions = &entry.facts.actions;
            let first = self.out.len();
            let from = path.as_str();
            match entry.kind {
                Some(SourceKind::Ruby) => {
                    self.ruby_constants(from, entry);
                    self.rails_inferred(from, entry);
                    self.requires(from, entry);
                    self.ruby_views(from, entry);
                }
                Some(SourceKind::Template(_)) => {
                    self.template(from, entry);
                }
                None => {}
            }
            if !self.actions.is_empty() {
                self.drop_scoped_twins(first);
            }
        }
        (self.out, self.stats)
    }

    /// Pushes an edge scoped to the action whose body contains `line`, if any.
    fn push(
        &mut self,
        from: &'a str,
        to: &'a str,
        kind: EdgeKind,
        confidence: Confidence,
        line: u32,
        detail: &str,
    ) {
        let scope = actions::action_at(self.actions, line).map(|a| a.name.as_str());
        self.push_full(from, to, kind, confidence, line, detail, scope, false);
    }

    /// A constant reference: scoped to the enclosing action, if any. With
    /// `method_level`, a superclass / mixin reference is an inheritance edge.
    #[allow(clippy::too_many_arguments)]
    fn push_ref(
        &mut self,
        from: &'a str,
        to: &'a str,
        kind: EdgeKind,
        confidence: Confidence,
        line: u32,
        detail: &str,
        inherit: bool,
    ) {
        let inherit = inherit && self.method_level;
        let scope = if inherit { None } else { actions::action_at(self.actions, line).map(|a| a.name.as_str()) };
        self.push_full(from, to, kind, confidence, line, detail, scope, inherit);
    }

    #[allow(clippy::too_many_arguments)]
    fn push_scoped(
        &mut self,
        from: &'a str,
        to: &'a str,
        kind: EdgeKind,
        confidence: Confidence,
        line: u32,
        detail: &str,
        scope: Option<&'a str>,
    ) {
        self.push_full(from, to, kind, confidence, line, detail, scope, false);
    }

    #[allow(clippy::too_many_arguments)]
    fn push_full(
        &mut self,
        from: &'a str,
        to: &'a str,
        kind: EdgeKind,
        confidence: Confidence,
        line: u32,
        detail: &str,
        scope: Option<&'a str>,
        inherit: bool,
    ) {
        if from == to {
            return;
        }
        // Keep the first (lowest-line) occurrence per (to, kind, scope); it
        // inherits if any occurrence does.
        if let Some(&i) = self.seen.get(&(to, kind, scope)) {
            self.out[i].inherit |= inherit;
            return;
        }
        self.seen.insert((to, kind, scope), self.out.len());
        let mut edge = Edge::new(from, to, kind, confidence)
            .at(line)
            .detail(truncate(detail));
        edge.from_scope = scope.map(String::from);
        edge.inherit = inherit;
        self.out.push(edge);
    }

    /// An unscoped edge already covers the whole file: drop the same
    /// (to, kind) scoped to an action among the edges from `first` on.
    fn drop_scoped_twins(&mut self, first: usize) {
        let tail = self.out.split_off(first);
        let whole: HashSet<(&str, EdgeKind)> = tail.iter().filter(|e| e.from_scope.is_none() && !e.inherit).map(|e| (e.to.as_str(), e.kind)).collect();
        let keep: Vec<Edge> = tail.iter().filter(|e| e.from_scope.is_none() || !whole.contains(&(e.to.as_str(), e.kind))).cloned().collect();
        self.out.extend(keep);
    }

    /// Defining files of a declaration, applying `namespace_definition_limit`.
    fn targets(&mut self, decl_id: DeclarationId) -> Option<Rc<Targets<'a>>> {
        if let Some(t) = self.cache.get(&decl_id) {
            return t.clone();
        }
        let t = self.compute_targets(decl_id).map(Rc::new);
        self.cache.insert(decl_id, t.clone());
        t
    }

    fn compute_targets(&mut self, decl_id: DeclarationId) -> Option<Targets<'a>> {
        let graph = self.graph;
        let decl = graph.declarations().get(&decl_id)?;
        let mut files: Vec<&'a str> = decl
            .definitions()
            .iter()
            .filter_map(|d| graph.definitions().get(d))
            .filter_map(|d| graph.documents().get(d.uri_id()))
            .map(|doc| doc.uri())
            .filter(|uri| *uri != BUILTIN_URI)
            .collect();
        files.sort_unstable();
        files.dedup();
        let mut dropped = false;
        if files.len() > self.limit {
            let key = paths::normalize_const(decl.name());
            files.retain(|f| {
                self.files
                    .get(*f)
                    .is_some_and(|e| e.zeitwerk_keys.contains(&key))
            });
            if files.is_empty() {
                self.stats.namespace_no_canonical += 1;
                dropped = true;
            } else {
                self.stats.namespace_canonical += 1;
            }
        }
        Some(Targets {
            files,
            name: decl.name(),
            dropped,
        })
    }

    fn lookup_name(&mut self, fqn: &str) -> Option<Rc<Targets<'a>>> {
        let fqn = fqn.strip_prefix("::").unwrap_or(fqn);
        self.targets(DeclarationId::from(fqn))
    }

    /// Ruby's last resort for a constant that lexical scope and ancestors
    /// did not find: `Object`, i.e. the top-level name (with any parent path
    /// resolved the same way).
    ///
    /// Rubydex 0.4.3 already does this when a nesting's ancestor chain is
    /// complete, but it can cache a class's ancestors while its superclass is
    /// still a placeholder: in mastodon, `class ActivityPub::Activity::Announce
    /// < ActivityPub::Activity` ends up with ancestors `[Announce,
    /// ActivityPub::Activity]` (no `Object`), so `Status` inside it stays
    /// unresolved in a cold build but resolves after that file is re-indexed.
    /// Applying the fallback here makes both paths agree and fixes the misses.
    /// Only names whose fully-qualified form is a known declaration qualify.
    fn top_level_fallback(&mut self, name_id: NameId) -> Option<Rc<Targets<'a>>> {
        let fqn = self.fallback_fqn(name_id, 0)?;
        self.lookup_name(&fqn)
    }

    fn fallback_fqn(&self, name_id: NameId, depth: usize) -> Option<String> {
        let name = self.graph.names().get(&name_id)?;
        if let NameRef::Resolved(r) = name {
            return self
                .graph
                .declarations()
                .get(r.declaration_id())
                .map(|d| d.name().to_string());
        }
        let s = self.graph.strings().get(name.str())?.as_str();
        if s.starts_with('<') || depth > 16 {
            return None;
        }
        match name.parent_scope() {
            ParentScope::None | ParentScope::TopLevel => Some(s.to_string()),
            ParentScope::Some(parent) => {
                Some(format!("{}::{s}", self.fallback_fqn(*parent, depth + 1)?))
            }
            ParentScope::Attached(_) => None,
        }
    }

    /// The nearest enclosing constant of an unresolved path that did resolve
    /// (`Account` for an unresolvable `Account::BAR`), restricted to that
    /// constant's Zeitwerk-canonical file. The restriction matters: apps
    /// reopen gem namespaces (`lib/active_record/batches.rb` reopens
    /// `ActiveRecord`), and `ActiveRecord::Base` must not fall back to them.
    fn prefix_fallback(&mut self, name_id: NameId) -> Option<Rc<Targets<'a>>> {
        let names = self.graph.names();
        let mut current = name_id;
        loop {
            let ParentScope::Some(parent) = names.get(&current)?.parent_scope() else {
                return None;
            };
            current = *parent;
            if let Some(decl) = self.graph.name_id_to_declaration_id(current).copied() {
                let t = self.targets(decl)?;
                return self.canonical_only(t);
            }
        }
    }

    fn canonical_only(&self, t: Rc<Targets<'a>>) -> Option<Rc<Targets<'a>>> {
        let key = paths::normalize_const(t.name);
        let files: Vec<&'a str> = t
            .files
            .iter()
            .copied()
            .filter(|f| {
                self.files
                    .get(*f)
                    .is_some_and(|e| e.zeitwerk_keys.contains(&key))
            })
            .collect();
        if files.is_empty() {
            None
        } else if files.len() == t.files.len() {
            Some(t)
        } else {
            Some(Rc::new(Targets {
                files,
                name: t.name,
                dropped: t.dropped,
            }))
        }
    }

    fn ruby_constants(&mut self, from: &'a str, entry: &'a FileEntry) {
        for r in &entry.refs {
            self.stats.refs_total += 1;
            let (targets, confidence) =
                match self.graph.name_id_to_declaration_id(r.name_id).copied() {
                    Some(decl) => {
                        self.stats.refs_resolved += 1;
                        (self.targets(decl), Confidence::Exact)
                    }
                    None => {
                        self.stats.refs_unresolved += 1;
                        if let Some(t) = self.top_level_fallback(r.name_id) {
                            self.stats.refs_top_level_fallback += 1;
                            for &to in &t.files {
                                self.push_ref(
                                    from,
                                    to,
                                    EdgeKind::RubyConstant,
                                    Confidence::Exact,
                                    r.line,
                                    t.name,
                                    r.inherit,
                                );
                            }
                            continue;
                        }
                        match self.prefix_fallback(r.name_id) {
                            Some(t) => {
                                self.stats.refs_prefix_fallback += 1;
                                (Some(t), Confidence::Inferred)
                            }
                            None => (None, Confidence::Exact),
                        }
                    }
                };
            let Some(t) = targets else { continue };
            if t.dropped {
                self.stats.refs_to_namespace_without_canonical += 1;
            } else if t.files.is_empty() {
                self.stats.refs_external += 1;
            }
            for &to in &t.files {
                self.push_ref(from, to, EdgeKind::RubyConstant, confidence, r.line, t.name, r.inherit);
            }
        }
    }

    fn rails_inferred(&mut self, from: &'a str, entry: &'a FileEntry) {
        for inf in &entry.facts.inferred {
            let name = match &inf.name {
                ClassName::Literal(n) => n.clone(),
                ClassName::Association { name, plural } => {
                    let singular = if *plural {
                        self.inflector.singularize(name)
                    } else {
                        name.clone()
                    };
                    self.inflector.camelize(&singular)
                }
            };
            let mut candidates = Vec::new();
            if let Some(abs) = name.strip_prefix("::") {
                candidates.push(abs.to_string());
            } else {
                if !inf.absolute && !inf.scope.is_empty() {
                    let mut scope = inf.scope.as_str();
                    loop {
                        candidates.push(format!("{scope}::{name}"));
                        match scope.rsplit_once("::") {
                            Some((outer, _)) => scope = outer,
                            None => break,
                        }
                    }
                }
                candidates.push(name.clone());
            }
            candidates.extend(inf.top_level_fallback.iter().cloned());
            // Like Rails, the first candidate that exists wins, even if it is defined outside the repo.
            match candidates.iter().find_map(|c| self.lookup_name(c)) {
                Some(t) => {
                    for &to in &t.files {
                        self.push_ref(
                            from,
                            to,
                            EdgeKind::RailsInferred,
                            Confidence::Inferred,
                            inf.line,
                            t.name,
                            false,
                        );
                    }
                }
                None => self.stats.inferred_unresolved += 1,
            }
        }
    }

    fn requires(&mut self, from: &'a str, entry: &'a FileEntry) {
        for req in &entry.facts.requires {
            match self.resolve_require(from, req.kind, &req.path) {
                Some(to) => self.push(
                    from,
                    to,
                    EdgeKind::RubyRequire,
                    Confidence::Exact,
                    req.line,
                    &req.path,
                ),
                None => self.stats.requires_unresolved += 1,
            }
        }
    }

    fn resolve_require(&self, from: &str, kind: RequireKind, spec: &str) -> Option<&'a str> {
        let with_ext = if spec.ends_with(".rb") {
            spec.to_string()
        } else {
            format!("{spec}.rb")
        };
        let found = |p: &str| self.all_files.get(p).copied();
        if kind == RequireKind::Relative || spec.starts_with("./") || spec.starts_with("../") {
            let base = if kind == RequireKind::Relative {
                paths::dir_of(from)
            } else {
                ""
            };
            return normalize_join(base, &with_ext).and_then(|p| found(&p));
        }
        // Load-path order: the requiring file's own `lib/` (gems inside the
        // monorepo), the app's `lib/`, `spec/`/`test/`, autoload roots for
        // `require_dependency`, then any other `lib/` when exactly one matches.
        let mut roots: Vec<String> = self
            .lib_roots
            .iter()
            .filter(|r| from.starts_with(r.as_str()))
            .rev()
            .cloned()
            .collect();
        roots.push("lib/".into());
        for d in ["spec/", "test/"] {
            if let Some(i) = from
                .find(d)
                .filter(|i| *i == 0 || from.as_bytes()[i - 1] == b'/')
            {
                roots.push(from[..i + d.len()].to_string());
            }
            roots.push(d.into());
        }
        if kind == RequireKind::Dependency {
            for app_dir in [
                "app/models/",
                "app/controllers/",
                "app/helpers/",
                "app/services/",
                "app/lib/",
            ] {
                roots.push(app_dir.into());
            }
        }
        if let Some(hit) = roots.iter().find_map(|r| found(&format!("{r}{with_ext}"))) {
            return Some(hit);
        }
        let mut others = self
            .lib_roots
            .iter()
            .filter_map(|r| found(&format!("{r}{with_ext}")));
        match (others.next(), others.next()) {
            (Some(one), None) => Some(one),
            _ => None,
        }
    }

    /// Controller/mailer -> its templates; explicit `render`/`layout` targets;
    /// view components -> sidecar templates.
    fn ruby_views(&mut self, from: &'a str, entry: &'a FileEntry) {
        let prefix =
            paths::controller_view_prefix(from).or_else(|| paths::mailer_view_prefix(from));
        if let Some(prefix) = prefix {
            let detail = format!("views of {prefix}");
            let mut targets = self.views.templates_for_prefix(prefix);
            targets.extend(self.views.layout(prefix));
            for to in targets {
                // `<prefix>/<action>.*` belongs to that action alone, unless rendered from elsewhere.
                let stem = paths::stem(to);
                let scope = self
                    .actions
                    .iter()
                    .find(|a| a.template_scoped && a.name == stem && paths::split_view_path(to)
                        .is_some_and(|(_, rest)| paths::dir_of(rest) == prefix))
                    .map(|a| a.name.as_str());
                self.push_scoped(
                    from,
                    to,
                    EdgeKind::RailsView,
                    Confidence::Inferred,
                    0,
                    &detail,
                    scope,
                );
            }
            for r in &entry.facts.renders {
                let found = match r.kind {
                    RenderKind::Layout => self.views.layout(&r.name),
                    RenderKind::Partial => self.views.partial(prefix, &r.name),
                    RenderKind::Template if r.name.contains('/') => {
                        self.views.template("", &r.name)
                    }
                    RenderKind::Template => self.views.template(prefix, &r.name),
                };
                if found.is_empty() {
                    self.stats.renders_unresolved += 1;
                }
                // Layouts wrap every action that uses them: never scoped.
                let scope = match r.kind {
                    RenderKind::Layout => None,
                    _ => actions::action_at(self.actions, r.line).map(|a| a.name.as_str()),
                };
                for to in found {
                    self.push_scoped(
                        from,
                        to,
                        EdgeKind::RailsView,
                        Confidence::Inferred,
                        r.line,
                        &r.name,
                        scope,
                    );
                }
            }
        }
        if paths::is_component(from) {
            for to in self.views.component_templates(from) {
                self.push(
                    from,
                    to,
                    EdgeKind::RailsView,
                    Confidence::Inferred,
                    0,
                    "component template",
                );
            }
        }
    }

    fn template(&mut self, from: &'a str, entry: &'a FileEntry) {
        if let Some((_, under_root)) = paths::split_view_path(from) {
            let current = paths::dir_of(under_root);
            for r in &entry.template.renders {
                let found = match r.kind {
                    RenderKind::Template => self.views.template(current, &r.name),
                    RenderKind::Partial | RenderKind::Layout => {
                        self.views.partial(current, &r.name)
                    }
                };
                if found.is_empty() {
                    self.stats.renders_unresolved += 1;
                }
                for to in found {
                    self.push(
                        from,
                        to,
                        EdgeKind::RailsView,
                        Confidence::Inferred,
                        r.line,
                        &r.name,
                    );
                }
            }
        }
        for (name, line) in &entry.template.consts {
            // Template code runs at top level: look the path up as written,
            // falling back to the canonical file of its longest defined prefix.
            let mut current = name.as_str();
            let targets = loop {
                if let Some(t) = self.lookup_name(current) {
                    break if current.len() == name.len() {
                        Some(t)
                    } else {
                        self.canonical_only(t)
                    };
                }
                match current.rsplit_once("::") {
                    Some((outer, _)) => current = outer,
                    None => break None,
                }
            };
            match targets {
                Some(t) => {
                    for &to in &t.files {
                        self.push_ref(
                            from,
                            to,
                            EdgeKind::RailsView,
                            Confidence::Inferred,
                            *line,
                            t.name,
                            false,
                        );
                    }
                }
                None => self.stats.template_consts_unresolved += 1,
            }
        }
    }
}

fn truncate(s: &str) -> String {
    if s.len() <= 80 {
        return s.to_string();
    }
    let mut end = 77;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

/// Joins `base` and a relative path, resolving `.` and `..`; `None` if it escapes the repo.
fn normalize_join(base: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = base.split('/').filter(|s| !s.is_empty()).collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::normalize_join;

    #[test]
    fn join() {
        assert_eq!(
            normalize_join("lib/a", "../b/c.rb").as_deref(),
            Some("lib/b/c.rb")
        );
        assert_eq!(normalize_join("", "../x.rb"), None);
    }
}
