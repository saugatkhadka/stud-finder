//! Method-level scopes (`ruby.method_level`): which methods of a file get a
//! scope, and the edges that name them.
//!
//! Name-based, since Ruby has no static types. Per file (cached, see
//! [`crate::methods`]) we know the methods it defines and the names it
//! mentions, each with the enclosing method. Every build/update then
//! recomputes, from those facts and workspace-wide name tables:
//!
//! 1. reference edges (`B` names a constant of `A`) get `to_scope = @body`
//!    (`A`'s class body only) and `from_scope` = the method of `B` they sit
//!    in; superclass/`include`/`extend`/`prepend` references become
//!    inheritance edges;
//! 2. for every name `m` that `B` mentions and `A` defines (itself or through
//!    its ancestors), an edge `B#s -> A#m`;
//! 3. same-file calls `A#s -> A#m` (also for `m` inherited, and for mentions
//!    in ancestors that resolve to `A`'s own methods);
//! 4. name-only edges `B#s -> D#m` when `m` is defined in at most
//!    `ruby.name_only_limit` files (receiver unknown: `user.purchases.first.refund!`);
//! 5. methods that are invoked implicitly get no scope (a change inside them
//!    is a change to the whole file): a fixed list, setters, operators, and
//!    any method whose name is mentioned nowhere in the workspace outside
//!    its own definition (called by convention: GraphQL, Pundit, ...).
//!    Dynamic dispatch in `B#s` (`obj.send(x)`) makes `B#s` depend on all of
//!    every file `B` references; on `self` (`send(x)`), on all of `B`'s own
//!    methods and all of each parent. An ancestor's dispatch on `self` does
//!    the same for each descendant.
//!
//! Edges never name a scope that cannot be affected: a `to_scope` naming a
//! method that has no scope in the target (nor its ancestors) becomes
//! `@body`, and such self edges are dropped.
//!
//! Soundness rule: when unsure, fall back to "the whole file".
//!
//! The name tables make one file's edges depend on other files (adding the
//! first mention of a method gives it a scope; a second definer can push a
//! name over the limit), so nothing here is cached: everything is recomputed
//! from per-file facts, in parallel. Name ids come from a workspace-wide,
//! append-only [`Interner`]; they never reach the output, which uses names.

use crate::actions;
use crate::analyse::FileEntry;
use crate::methods::{MethodFacts, NONE};
use crate::paths::{self, SourceKind};
use rayon::prelude::*;
use ripple_core::model::{ScopeRange, SCOPE_BODY};
use ripple_core::{Confidence, Edge, EdgeKind};
use std::collections::{BTreeMap, HashMap, HashSet};

/// A name defined in more files than this gets no name-only edges.
/// Names shorter than this get no name-only edges.
const NAME_ONLY_MIN_LEN: usize = 4;

/// Methods called implicitly (by Ruby, Rails or common gems) without their
/// name being written at the call site. A change inside one = whole file.
const IMPLICIT: &[&str] = &[
    // Ruby hooks and conversions
    "method_missing", "respond_to_missing?", "respond_to?", "inherited", "included", "extended",
    "prepended", "method_added", "singleton_method_added", "method_removed", "method_undefined",
    "const_missing", "initialize_copy", "initialize_dup", "initialize_clone", "to_s", "to_str",
    "to_proc", "to_ary", "to_a", "to_h", "to_hash", "to_i", "to_int", "to_f", "to_r", "to_c",
    "to_sym", "to_io", "to_path", "to_regexp", "to_json", "as_json", "to_xml", "to_yaml",
    "to_liquid", "serializable_hash", "inspect", "pretty_print", "hash", "eql?", "each",
    "call", "coerce", "marshal_dump", "marshal_load", "_dump", "encode_with", "init_with",
    "dup", "clone", "freeze",
    // Rails / ActiveModel
    "to_param", "to_partial_path", "to_model", "to_key", "persisted?", "new_record?",
    "model_name", "cache_key", "cache_key_with_version", "cache_version", "attributes",
    "attribute_names", "read_attribute_for_serialization", "read_attribute_for_validation",
    "perform", "validate", "validate_each", "up", "down", "change",
    "before_validation", "after_validation", "before_save", "around_save", "after_save",
    "before_create", "around_create", "after_create", "before_update", "around_update",
    "after_update", "before_destroy", "around_destroy", "after_destroy", "after_commit",
    "after_rollback", "after_initialize", "after_find", "after_touch", "after_create_commit",
    "after_update_commit", "after_destroy_commit", "after_save_commit",
    // GraphQL, Pundit, ViewComponent, ActionCable, test frameworks
    "resolve", "resolve_type", "authorized?", "ready?", "visible?", "coerce_input",
    "coerce_result", "index?", "show?", "new?", "create?", "edit?", "update?", "destroy?",
    "permitted_attributes", "render_in", "before_render", "render?", "subscribed",
    "unsubscribed", "receive", "connect", "disconnect", "setup", "teardown",
];

#[derive(Debug, Default)]
pub struct Interner {
    index: HashMap<String, u32>,
    names: Vec<String>,
}

impl Interner {
    pub fn get(&self, id: u32) -> &str {
        &self.names[id as usize]
    }
    pub fn len(&self) -> usize {
        self.names.len()
    }
    pub fn lookup(&self, s: &str) -> Option<u32> {
        self.index.get(s).copied()
    }

    fn intern(&mut self, s: String) -> u32 {
        if let Some(&i) = self.index.get(&s) {
            return i;
        }
        let i = self.names.len() as u32;
        self.names.push(s.clone());
        self.index.insert(s, i);
        i
    }

    /// Replaces the facts' local name ids with workspace-wide ones.
    pub fn globalize(&mut self, m: &mut MethodFacts) {
        if m.names.is_empty() {
            return;
        }
        let ids: Vec<u32> = std::mem::take(&mut m.names).into_iter().map(|n| self.intern(n)).collect();
        for d in &mut m.defs {
            d.name = ids[d.name as usize];
        }
        for x in &mut m.mentions {
            x.name = ids[x.name as usize];
        }
        for c in &mut m.counts {
            c.0 = ids[c.0 as usize];
        }
        for o in &mut m.own {
            *o = ids[*o as usize];
        }
        m.own.sort_unstable();
        m.mentions.sort_unstable_by_key(|x| (x.name, x.scope));
        m.counts.sort_unstable();
    }
}

/// Why a method has no scope of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Denied {
    Implicit,
    Operator,
    Setter,
    Unmentioned,
    ControllerNonAction,
}

impl Denied {
    fn key(self) -> &'static str {
        match self {
            Denied::Implicit => "implicit_list",
            Denied::Operator => "operator",
            Denied::Setter => "setter",
            Denied::Unmentioned => "unmentioned",
            Denied::ControllerNonAction => "controller_non_action",
        }
    }
}

pub struct FileScope {
    /// Scopes come from controller actions (by line), not from defs.
    pub controller: bool,
    /// Per def: its scope (the def's name id), or `None` when denied.
    pub def_scope: Vec<Option<u32>>,
}

/// Workspace-wide tables and per-file scope decisions, computed before edges.
pub struct Plan {
    pub files: Vec<FileScope>,
    /// name id -> files defining it (`definers[off[id]..off[id+1]]`).
    def_off: Vec<u32>,
    def_files: Vec<u32>,
    pub denied: BTreeMap<Denied, u64>,
}

fn is_operator(name: &str) -> bool {
    !name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_' || !c.is_ascii())
}

impl Plan {
    pub fn new(files: &BTreeMap<String, FileEntry>, names: &Interner) -> Self {
        let n = names.len();
        let mut counts = vec![0u32; n];
        let mut def_count = vec![0u32; n + 1];
        for e in files.values() {
            for &(id, c) in &e.methods.counts {
                counts[id as usize] += c;
            }
            for &id in &e.methods.own {
                def_count[id as usize + 1] += 1;
            }
        }
        for i in 0..n {
            def_count[i + 1] += def_count[i];
        }
        let mut def_files = vec![0u32; def_count[n] as usize];
        let mut cursor = def_count.clone();
        for (fi, e) in files.values().enumerate() {
            for &id in &e.methods.own {
                def_files[cursor[id as usize] as usize] = fi as u32;
                cursor[id as usize] += 1;
            }
        }
        let mut denied: BTreeMap<Denied, u64> = BTreeMap::new();
        let scopes = files
            .iter()
            .map(|(path, e)| {
                let controller = paths::is_scoped_controller(path);
                let def_scope = e
                    .methods
                    .defs
                    .iter()
                    .map(|d| {
                        let name = names.get(d.name);
                        let why = if controller {
                            Some(Denied::ControllerNonAction)
                        } else if IMPLICIT.contains(&name) {
                            Some(Denied::Implicit)
                        } else if is_operator(name) {
                            Some(Denied::Operator)
                        } else if name.ends_with('=') {
                            Some(Denied::Setter)
                        } else if counts[d.name as usize] == 0 {
                            Some(Denied::Unmentioned)
                        } else {
                            None
                        };
                        match why {
                            Some(w) => {
                                *denied.entry(w).or_default() += 1;
                                None
                            }
                            None => Some(d.name),
                        }
                    })
                    .collect();
                FileScope { controller, def_scope }
            })
            .collect();
        Plan { files: scopes, def_off: def_count, def_files, denied }
    }

    fn definers(&self, id: u32) -> &[u32] {
        &self.def_files[self.def_off[id as usize] as usize..self.def_off[id as usize + 1] as usize]
    }
}

/// What kind of method-level edge, for stats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Cat {
    /// Reference edge (`@body`, or unscoped for opaque files).
    Ref,
    Inherit,
    Use,
    SelfCall,
    /// Self edge from a mention in an ancestor (template method).
    SelfAncestor,
    /// Self edge from a dynamic call on `self`.
    SelfDynamic,
    NameOnly,
    Dynamic,
    /// Requires, views: untouched.
    Other,
}

struct Target {
    to: u32,
    kind: EdgeKind,
    confidence: Confidence,
    detail: String,
}

pub struct Output {
    pub edges: Vec<Edge>,
    pub scopes: BTreeMap<String, Vec<ScopeRange>>,
    pub stats: BTreeMap<String, u64>,
}

/// Sorted-set membership.
fn has(set: &[u32], x: u32) -> bool {
    set.binary_search(&x).is_ok()
}

/// Rewrites the edge builder's output for method-level precision.
/// `is_ref[i]`: `edges[i]` is a constant reference (Ruby constant, Rails
/// inference, template constant). Edges are grouped by `from` in file order.
pub fn finish(edges: Vec<Edge>, is_ref: Vec<bool>, files: &BTreeMap<String, FileEntry>, names: &Interner, plan: &Plan, limit: usize) -> Output {
    let entries: Vec<(&str, &FileEntry)> = files.iter().map(|(p, e)| (p.as_str(), e)).collect();
    let index: HashMap<&str, u32> = entries.iter().enumerate().map(|(i, (p, _))| (*p, i as u32)).collect();

    // Group edges by `from` (contiguous: the builder walks files in order).
    let mut groups: Vec<(usize, usize)> = vec![(0, 0); entries.len()];
    {
        let mut i = 0;
        while i < edges.len() {
            let from = index[edges[i].from.as_str()] as usize;
            let mut j = i;
            while j < edges.len() && edges[j].from == edges[i].from {
                j += 1;
            }
            groups[from] = (i, j);
            i = j;
        }
    }

    // Ancestors (inheritance edges, transitively) and available methods.
    let mut parents: Vec<Vec<u32>> = vec![Vec::new(); entries.len()];
    for (e, r) in edges.iter().zip(&is_ref) {
        if *r && e.inherit
            && let Some(&to) = index.get(e.to.as_str())
        {
            parents[index[e.from.as_str()] as usize].push(to);
        }
    }
    let ancestors: Vec<Vec<u32>> = (0..entries.len())
        .into_par_iter()
        .map(|i| {
            if parents[i].is_empty() {
                return Vec::new();
            }
            let mut seen: HashSet<u32> = HashSet::new();
            seen.insert(i as u32);
            let mut stack = parents[i].clone();
            let mut out = Vec::new();
            while let Some(p) = stack.pop() {
                if seen.insert(p) {
                    out.push(p);
                    stack.extend(parents[p as usize].iter().copied());
                }
            }
            out.sort_unstable();
            out
        })
        .collect();
    let avail_owned: Vec<Option<Vec<u32>>> = (0..entries.len())
        .into_par_iter()
        .map(|i| {
            if ancestors[i].is_empty() {
                return None;
            }
            let mut v: Vec<u32> = entries[i].1.methods.own.clone();
            for &a in &ancestors[i] {
                v.extend_from_slice(&entries[a as usize].1.methods.own);
            }
            v.sort_unstable();
            v.dedup();
            Some(v)
        })
        .collect();
    let avail = |i: usize| -> &[u32] { avail_owned[i].as_deref().unwrap_or(&entries[i].1.methods.own) };

    // Names that can be an affected scope of each file: its own scoped methods
    // (controller actions) and, through inherit edges, its ancestors'. An edge
    // `to_scope: m` with `m` outside this set is only ever followed when the
    // target is wholly affected, i.e. it is an `@body` edge; self edges to such
    // `m` are never followed and are not emitted.
    let scoped_own: Vec<Vec<u32>> = (0..entries.len())
        .into_par_iter()
        .map(|i| {
            let (_, e) = entries[i];
            let fs = &plan.files[i];
            let mut v: Vec<u32> = if fs.controller {
                e.facts.actions.iter().filter_map(|a| names.lookup(&a.name)).collect()
            } else {
                fs.def_scope.iter().flatten().copied().collect()
            };
            v.sort_unstable();
            v.dedup();
            v
        })
        .collect();
    let scoped_owned: Vec<Option<Vec<u32>>> = (0..entries.len())
        .into_par_iter()
        .map(|i| {
            if ancestors[i].is_empty() {
                return None;
            }
            let mut v = scoped_own[i].clone();
            for &a in &ancestors[i] {
                v.extend_from_slice(&scoped_own[a as usize]);
            }
            v.sort_unstable();
            v.dedup();
            Some(v)
        })
        .collect();
    let scoped = |i: usize| -> &[u32] { scoped_owned[i].as_deref().unwrap_or(&scoped_own[i]) };
    let to_scope_in = |file: usize, name: u32| -> String {
        if has(scoped(file), name) { names.get(name).to_string() } else { SCOPE_BODY.to_string() }
    };

    let results: Vec<(Vec<Edge>, Vec<Cat>, Option<Vec<ScopeRange>>)> = (0..entries.len())
        .into_par_iter()
        .map(|b| {
            let (lo, hi) = groups[b];
            let (path, entry) = entries[b];
            let fs = &plan.files[b];
            let mut out: Vec<(Edge, Cat)> = Vec::new();
            let opaque = entry.methods_failed;
            let scope_name = |file: usize, scope: u32, line: u32| -> Option<String> {
                let (_, e) = entries[file];
                let f = &plan.files[file];
                if f.controller {
                    return actions::action_at(&e.facts.actions, line).map(|a| a.name.clone());
                }
                if scope == NONE {
                    return None;
                }
                f.def_scope[scope as usize].map(|id| names.get(id).to_string())
            };

            // 1. Reference edges.
            let mut targets: Vec<Target> = Vec::new();
            for (i, e) in edges[lo..hi].iter().enumerate() {
                let mut e = e.clone();
                if !is_ref[lo + i] {
                    out.push((e, Cat::Other));
                    continue;
                }
                if e.inherit {
                    out.push((e, Cat::Inherit));
                    continue;
                }
                let Some(&to) = index.get(e.to.as_str()) else {
                    out.push((e, Cat::Other));
                    continue;
                };
                // Best edge per target: highest confidence, then kind, then line.
                match targets.iter_mut().find(|t| t.to == to) {
                    Some(t) if (t.confidence, std::cmp::Reverse(t.kind)) >= (e.confidence, std::cmp::Reverse(e.kind)) => {}
                    Some(t) => {
                        t.kind = e.kind;
                        t.confidence = e.confidence;
                        t.detail = e.detail.clone().unwrap_or_default();
                    }
                    None => targets.push(Target { to, kind: e.kind, confidence: e.confidence, detail: e.detail.clone().unwrap_or_default() }),
                }
                if !opaque {
                    e.to_scope = Some(SCOPE_BODY.to_string());
                }
                out.push((e, Cat::Ref));
            }
            let m = &entry.methods;
            let own_avail = avail(b);

            // 2. Method use through a referenced constant.
            let mut used: HashSet<(u32, u32)> = HashSet::new();
            for t in &targets {
                if t.to as usize == b {
                    continue;
                }
                let av = avail(t.to as usize);
                if av.is_empty() {
                    continue;
                }
                for x in &m.mentions {
                    if !has(av, x.name) {
                        continue;
                    }
                    used.insert((t.to, x.name));
                    // The inherit edges already carry a change in an ancestor's
                    // `m` to `t`, so no name-only edge to the ancestor either.
                    for &a in &ancestors[t.to as usize] {
                        if has(&entries[a as usize].1.methods.own, x.name) {
                            used.insert((a, x.name));
                        }
                    }
                    let name = names.get(x.name);
                    let mut e = Edge::new(path, entries[t.to as usize].0, t.kind, t.confidence)
                        .at(x.line)
                        .detail(truncate(&format!("{}#{name}", t.detail)))
                        .to_scope(&to_scope_in(t.to as usize, x.name));
                    e.from_scope = scope_name(b, x.scope, x.line);
                    out.push((e, Cat::Use));
                }
            }

            // 3. Same-file calls, including inherited methods ...
            let own_scoped = scoped(b);
            let self_edge = |out: &mut Vec<(Edge, Cat)>, from_scope: Option<String>, name: u32, line: u32, cat: Cat| {
                if !has(own_scoped, name) {
                    return;
                }
                let name = names.get(name);
                if from_scope.as_deref() == Some(name) {
                    return;
                }
                let mut e = Edge::new(path, path, EdgeKind::RubyConstant, Confidence::Exact).at(line).detail(truncate(&format!("#{name}"))).to_scope(name);
                e.from_scope = from_scope;
                out.push((e, cat));
            };
            for x in &m.mentions {
                if has(own_avail, x.name) {
                    self_edge(&mut out, scope_name(b, x.scope, x.line), x.name, x.line, Cat::SelfCall);
                }
            }
            // ... and mentions in ancestors that dispatch to this file's methods
            // (template-method pattern, overrides, callbacks in `included do`).
            for &a in &ancestors[b] {
                let am = &entries[a as usize].1.methods;
                let a_avail = avail(a as usize);
                for x in &am.mentions {
                    if has(&m.own, x.name) || (has(own_avail, x.name) && !has(a_avail, x.name)) {
                        self_edge(&mut out, scope_name(a as usize, x.scope, x.line), x.name, x.line, Cat::SelfAncestor);
                    }
                }
                // A dynamic call on `self` in an ancestor may reach any method of
                // this file: per dynamic scope `s` of the ancestor, `s` depends on
                // this file's own methods (self edges) and on all of every direct
                // parent (which any change in any ancestor reaches through the
                // inherit edges). Edges per inherited method would be
                // |descendants| x |inherited methods|: ~700k on gitlabhq.
                let mut done: HashSet<Option<String>> = HashSet::new();
                for d in am.dynamic.iter().filter(|d| d.on_self) {
                    let s = scope_name(a as usize, d.scope, d.line);
                    if !done.insert(s.clone()) {
                        continue;
                    }
                    for &id in &m.own {
                        self_edge(&mut out, s.clone(), id, d.line, Cat::SelfDynamic);
                    }
                    for (i, p) in edges[lo..hi].iter().enumerate() {
                        if is_ref[lo + i] && p.inherit {
                            let mut e = Edge::new(path, p.to.as_str(), p.kind, p.confidence)
                                .at(d.line)
                                .detail(truncate(&format!("{} (dynamic call in ancestor)", p.detail.as_deref().unwrap_or(""))));
                            e.from_scope = s.clone();
                            out.push((e, Cat::Dynamic));
                        }
                    }
                }
            }

            // Dynamic dispatch with an unknown receiver: the enclosing method
            // depends on all of every referenced file. On `self` (no receiver
            // or `self.`): on every method of this file (self edges) and on all
            // of each parent; inherited methods are reached through the parents,
            // which any change in an ancestor reaches via the inherit edges.
            let mut dyn_targets: Vec<&Edge> = Vec::new();
            if !m.dynamic.is_empty() {
                let mut seen: HashSet<(&str, EdgeKind, bool)> = HashSet::new();
                for (i, e) in edges[lo..hi].iter().enumerate() {
                    if is_ref[lo + i] && seen.insert((e.to.as_str(), e.kind, e.inherit)) {
                        dyn_targets.push(e);
                    }
                }
            }
            let mut done: HashSet<(Option<String>, bool)> = HashSet::new();
            for d in &m.dynamic {
                let s = scope_name(b, d.scope, d.line);
                if !done.insert((s.clone(), d.on_self)) {
                    continue;
                }
                for t in dyn_targets.iter().filter(|t| !d.on_self || t.inherit) {
                    let mut e = Edge::new(path, t.to.as_str(), t.kind, t.confidence).at(d.line).detail(truncate(&format!("{} (dynamic call)", t.detail.as_deref().unwrap_or(""))));
                    e.from_scope = s.clone();
                    out.push((e, Cat::Dynamic));
                }
                if d.on_self {
                    for &id in &m.own {
                        self_edge(&mut out, s.clone(), id, d.line, Cat::SelfDynamic);
                    }
                }
            }

            // 4. Name-only edges.
            if limit > 0 {
                for x in &m.mentions {
                    let name = names.get(x.name);
                    if name.len() < NAME_ONLY_MIN_LEN {
                        continue;
                    }
                    let definers = plan.definers(x.name);
                    if definers.is_empty() || definers.len() > limit {
                        continue;
                    }
                    for &d in definers {
                        if d as usize == b || used.contains(&(d, x.name)) {
                            continue;
                        }
                        let mut e = Edge::new(path, entries[d as usize].0, EdgeKind::RailsInferred, Confidence::Heuristic)
                            .at(x.line)
                            .detail(truncate(&format!("#{name} (by name)")))
                            .to_scope(&to_scope_in(d as usize, x.name));
                        e.from_scope = scope_name(b, x.scope, x.line);
                        out.push((e, Cat::NameOnly));
                    }
                }
            }

            let (edges, cats) = absorb(out);
            (edges, cats, file_scopes(entry, fs, names))
        })
        .collect();

    let mut stats: BTreeMap<String, u64> = BTreeMap::new();
    let mut out_edges = Vec::new();
    let mut scopes = BTreeMap::new();
    for ((path, entry), (edges, cats, sc)) in entries.iter().zip(results) {
        for (e, c) in edges.iter().zip(&cats) {
            let key = match c {
                Cat::Ref if e.to_scope.is_some() => "ruby_method_edges_body",
                Cat::Ref => "ruby_method_edges_ref_unscoped",
                Cat::Inherit => "ruby_method_edges_inherit",
                Cat::Use => "ruby_method_edges_use",
                Cat::SelfCall => "ruby_method_edges_self",
                Cat::SelfAncestor => "ruby_method_edges_self_ancestor",
                Cat::SelfDynamic => "ruby_method_edges_self_dynamic",
                Cat::NameOnly => "ruby_method_edges_name_only",
                Cat::Dynamic => "ruby_method_edges_dynamic",
                Cat::Other => "ruby_method_edges_other",
            };
            *stats.entry(key.to_string()).or_default() += 1;
        }
        out_edges.extend(edges);
        if !entry.methods.dynamic.is_empty() {
            *stats.entry("ruby_method_dynamic_files".into()).or_default() += 1;
        }
        *stats.entry("ruby_method_defs".into()).or_default() += entry.methods.defs.len() as u64;
        if let Some(sc) = sc.filter(|s| !s.is_empty()) {
            *stats.entry("ruby_method_scope_ranges".into()).or_default() += sc.len() as u64;
            scopes.insert(path.to_string(), sc);
        }
    }
    for (why, n) in &plan.denied {
        stats.insert(format!("ruby_method_denied_{}", why.key()), *n);
    }
    stats.insert("ruby_method_name_only_limit".into(), limit as u64);
    Output { edges: out_edges, scopes, stats }
}

/// `Facts.scopes` for one file: controller actions, or the methods that have
/// a scope, are defined once in the file and own every line of their range.
fn file_scopes(entry: &FileEntry, fs: &FileScope, names: &Interner) -> Option<Vec<ScopeRange>> {
    if fs.controller {
        return Some(entry.facts.actions.iter().map(|a| ScopeRange { name: a.name.clone(), start: a.start, end: a.end }).collect());
    }
    if entry.kind != Some(SourceKind::Ruby) {
        return None;
    }
    let defs = &entry.methods.defs;
    let mut per_name: HashMap<u32, u32> = HashMap::new();
    for d in defs {
        *per_name.entry(d.name).or_default() += 1;
    }
    let mut out: Vec<ScopeRange> = Vec::new();
    for (i, d) in defs.iter().enumerate() {
        let overlaps = (i > 0 && defs[i - 1].end >= d.start) || (i + 1 < defs.len() && defs[i + 1].start <= d.end);
        if fs.def_scope[i].is_some() && per_name[&d.name] == 1 && d.clean && !overlaps {
            out.push(ScopeRange { name: names.get(d.name).to_string(), start: d.start, end: d.end });
        }
    }
    out.sort_by_key(|r| r.start);
    Some(out)
}

/// Dedups (keeping the best confidence, then lowest line) and drops edges a
/// coarser twin already covers, as `Graph::build` would.
fn absorb(mut v: Vec<(Edge, Cat)>) -> (Vec<Edge>, Vec<Cat>) {
    v.sort_by(|(a, _), (b, _)| {
        (&a.to, a.kind, a.inherit, &a.from_scope, &a.to_scope, std::cmp::Reverse(a.confidence), a.line, &a.detail)
            .cmp(&(&b.to, b.kind, b.inherit, &b.from_scope, &b.to_scope, std::cmp::Reverse(b.confidence), b.line, &b.detail))
    });
    v.dedup_by(|(b, _), (a, _)| a.to == b.to && a.kind == b.kind && a.inherit == b.inherit && a.from_scope == b.from_scope && a.to_scope == b.to_scope);
    let present: HashSet<(&str, EdgeKind, Option<&str>, Option<&str>)> =
        v.iter().filter(|(e, _)| !e.inherit).map(|(e, _)| (e.to.as_str(), e.kind, e.from_scope.as_deref(), e.to_scope.as_deref())).collect();
    let keep: Vec<bool> = v
        .iter()
        .map(|(e, _)| {
            if e.inherit {
                return true;
            }
            let (to, k, fs, ts) = (e.to.as_str(), e.kind, e.from_scope.as_deref(), e.to_scope.as_deref());
            !((fs.is_some() && present.contains(&(to, k, None, ts)))
                || (ts.is_some() && present.contains(&(to, k, fs, None)))
                || (fs.is_some() && ts.is_some() && present.contains(&(to, k, None, None))))
        })
        .collect();
    let mut edges = Vec::with_capacity(v.len());
    let mut cats = Vec::with_capacity(v.len());
    for ((e, c), k) in v.into_iter().zip(keep) {
        if k {
            edges.push(e);
            cats.push(c);
        }
    }
    (edges, cats)
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
