//! Rails DSL facts gathered with a direct ruby-prism pass: `require`s,
//! associations, literal `constantize`/`const_get`, `class_name:` style
//! options, and `render`/`layout` calls (used for controllers and mailers).
//!
//! The pass only records what is written in the file. Turning a fact into an
//! edge (inflection, lookup in the Rubydex graph, file resolution) happens at
//! edge-generation time, so that a change elsewhere (a new class, a changed
//! `inflections.rb`) is reflected without re-parsing this file.

use crate::views::{Render, RenderKind};
use ruby_prism::{CallNode, ClassNode, ModuleNode, Node, Visit};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Require {
    pub kind: RequireKind,
    pub path: String,
    pub line: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequireKind {
    Require,
    Relative,
    /// `require_dependency`: looked up in autoload roots too.
    Dependency,
}

/// How the class name is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassName {
    /// A literal constant path (`"Foo::Bar"`, `"::Foo"`).
    Literal(String),
    /// An association name that needs inflecting (`has_many :user_roles`).
    Association { name: String, plural: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferredRef {
    pub name: ClassName,
    /// Fully-qualified name of the innermost enclosing class/module ("" at top level).
    /// Candidates are tried the way ActiveRecord's `compute_type` does:
    /// `Scope::Inner::Name`, `Scope::Name`, `Name`.
    pub scope: String,
    /// Skip the scope prefixes (`"Foo".constantize` is always top-level).
    pub absolute: bool,
    /// Tried last: `Recv.const_get("X")` also finds top-level `X`, because
    /// `const_get` with `inherit = true` searches `Object` for modules.
    pub top_level_fallback: Option<String>,
    pub line: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RubyFacts {
    pub requires: Vec<Require>,
    pub inferred: Vec<InferredRef>,
    pub renders: Vec<Render>,
    pub assoc_polymorphic_skipped: u32,
    pub assoc_dynamic_skipped: u32,
    /// Controller actions by line range, sorted (controllers only; see [`crate::actions`]).
    pub actions: Vec<crate::actions::Action>,
}

/// Substrings that must appear for the prism pass to find anything; files
/// without any of them skip the second parse.
const TRIGGERS: &[&str] = &[
    "require",
    "belongs_to",
    "has_one",
    "has_many",
    "has_and_belongs_to_many",
    "constantize",
    "const_get",
    "class_name",
    "serializer",
    "source_type",
    "render",
    "layout",
    "factory",
];

/// `controller`: the file is a (non-concern) controller, whose action line
/// ranges are collected from the same parse.
pub fn collect(source: &str, controller: bool) -> RubyFacts {
    if !controller && !TRIGGERS.iter().any(|t| source.contains(t)) {
        return RubyFacts::default();
    }
    let result = ruby_prism::parse(source.as_bytes());
    let root = result.node();
    let mut v = Visitor {
        nesting: Vec::new(),
        facts: RubyFacts::default(),
    };
    v.visit(&root);
    if controller {
        v.facts.actions = crate::actions::collect(&root);
    }
    v.facts
}

struct Visitor {
    nesting: Vec<String>,
    facts: RubyFacts,
}

impl Visitor {
    fn scope(&self) -> String {
        self.nesting.last().cloned().unwrap_or_default()
    }

    fn enter(&mut self, name: Option<String>) {
        let full = match name {
            Some(n) if n.starts_with("::") => n[2..].to_string(),
            Some(n) => match self.nesting.last() {
                Some(outer) => format!("{outer}::{n}"),
                None => n,
            },
            // Dynamic name: keep the outer scope so nesting stays balanced.
            None => self.scope(),
        };
        self.nesting.push(full);
    }

    fn infer(&mut self, name: ClassName, absolute: bool, line: u32) {
        let scope = if absolute {
            String::new()
        } else {
            self.scope()
        };
        self.facts.inferred.push(InferredRef {
            name,
            scope,
            absolute,
            top_level_fallback: None,
            line,
        });
    }

    fn handle_call(&mut self, call: &CallNode) {
        let method = call.name().as_slice();
        let line = line_of(&call.location());
        let receiver = call.receiver();
        let receiverless = receiver.as_ref().is_none_or(|r| r.as_self_node().is_some());
        let args: Vec<Node> = call
            .arguments()
            .map(|a| a.arguments().iter().collect())
            .unwrap_or_default();

        match method {
            b"require" | b"require_relative" | b"require_dependency" if receiverless => {
                if let Some(path) = args.first().and_then(string_lit) {
                    let kind = match method {
                        b"require" => RequireKind::Require,
                        b"require_relative" => RequireKind::Relative,
                        _ => RequireKind::Dependency,
                    };
                    self.facts.requires.push(Require { kind, path, line });
                }
            }
            b"belongs_to" | b"has_one" | b"has_many" | b"has_and_belongs_to_many"
                if receiverless =>
            {
                self.association(method, &args, line);
            }
            b"constantize" | b"safe_constantize" => {
                if let Some(name) = receiver.as_ref().and_then(string_lit) {
                    self.infer(ClassName::Literal(name), true, line);
                }
            }
            b"const_get" => {
                let Some(name) = args
                    .first()
                    .and_then(|a| string_lit(a).or_else(|| symbol_lit(a)))
                else {
                    return;
                };
                match receiver.as_ref() {
                    None => self.infer(ClassName::Literal(name), false, line),
                    Some(r) if r.as_self_node().is_some() => {
                        self.infer(ClassName::Literal(name), false, line)
                    }
                    Some(r) => match const_name(r) {
                        Some(recv)
                            if matches!(
                                recv.as_str(),
                                "Object" | "::Object" | "Kernel" | "::Kernel"
                            ) =>
                        {
                            self.infer(ClassName::Literal(name), true, line)
                        }
                        Some(recv) => {
                            self.infer(ClassName::Literal(format!("{recv}::{name}")), false, line);
                            if let Some(last) = self.facts.inferred.last_mut() {
                                last.top_level_fallback = Some(name);
                            }
                        }
                        None => {}
                    },
                }
            }
            b"render" if receiverless => self.render(&args, line),
            b"layout" if receiverless => {
                if let Some(name) = args.first().and_then(string_lit) {
                    self.facts.renders.push(Render {
                        kind: RenderKind::Layout,
                        name,
                        line,
                    });
                }
            }
            _ => {
                // `class_name: "Foo"` on any DSL (associations are handled above),
                // `serializer:`/`each_serializer:` as strings, `factory :x, class: "Foo"`.
                for (key, value) in keyword_args(&args) {
                    let applies = match key.as_str() {
                        "class_name" | "serializer" | "each_serializer" => true,
                        "class" => method == b"factory",
                        _ => false,
                    };
                    if applies && let Some(name) = string_lit(&value) {
                        self.infer(ClassName::Literal(name), false, line);
                    }
                }
            }
        }
    }

    fn association(&mut self, method: &[u8], args: &[Node], line: u32) {
        let Some(assoc) = args
            .first()
            .and_then(|a| symbol_lit(a).or_else(|| string_lit(a)))
        else {
            return;
        };
        let kw = keyword_args(args);
        let get = |k: &str| kw.iter().find(|(key, _)| key == k).map(|(_, v)| v);
        if get("polymorphic").is_some_and(|v| v.as_true_node().is_some()) {
            self.facts.assoc_polymorphic_skipped += 1;
            return;
        }
        // An explicit class always overrides the name; if it is not a literal
        // we cannot know it, so we skip rather than guess from the name.
        for key in ["class_name", "source_type"] {
            if let Some(v) = get(key) {
                match string_lit(v).or_else(|| symbol_lit(v)) {
                    Some(name) => self.infer(ClassName::Literal(name), false, line),
                    None => self.facts.assoc_dynamic_skipped += 1,
                }
                return;
            }
        }
        let plural = matches!(method, b"has_many" | b"has_and_belongs_to_many");
        let name = match get("source").and_then(|v| symbol_lit(v).or_else(|| string_lit(v))) {
            Some(source) if get("through").is_some() => source,
            _ => assoc,
        };
        self.infer(ClassName::Association { name, plural }, false, line);
    }

    /// `render "x"`, `render partial: "x"`, `render template: "x"`, `render layout: "x"`.
    fn render(&mut self, args: &[Node], line: u32) {
        if let Some(name) = args.first().and_then(string_lit) {
            self.facts.renders.push(Render {
                kind: RenderKind::Template,
                name,
                line,
            });
        }
        for (key, value) in keyword_args(args) {
            let kind = match key.as_str() {
                "partial" => RenderKind::Partial,
                "template" => RenderKind::Template,
                "layout" => RenderKind::Layout,
                _ => continue,
            };
            if let Some(name) = string_lit(&value) {
                self.facts.renders.push(Render { kind, name, line });
            }
        }
    }
}

impl<'pr> Visit<'pr> for Visitor {
    fn visit_class_node(&mut self, node: &ClassNode<'pr>) {
        self.enter(const_name(&node.constant_path()));
        ruby_prism::visit_class_node(self, node);
        self.nesting.pop();
    }

    fn visit_module_node(&mut self, node: &ModuleNode<'pr>) {
        self.enter(const_name(&node.constant_path()));
        ruby_prism::visit_module_node(self, node);
        self.nesting.pop();
    }

    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        self.handle_call(node);
        ruby_prism::visit_call_node(self, node);
    }
}

fn line_of(loc: &ruby_prism::Location) -> u32 {
    u32::try_from(loc.start_line()).unwrap_or(0)
}

fn utf8(bytes: &[u8]) -> Option<String> {
    std::str::from_utf8(bytes).ok().map(String::from)
}

/// A plain (non-interpolated) string literal.
pub(crate) fn string_lit(node: &Node) -> Option<String> {
    node.as_string_node().and_then(|s| utf8(s.unescaped()))
}

fn symbol_lit(node: &Node) -> Option<String> {
    node.as_symbol_node().and_then(|s| utf8(s.unescaped()))
}

/// `Foo`, `Foo::Bar`, `::Foo` as written; `None` for dynamic paths.
fn const_name(node: &Node) -> Option<String> {
    if let Some(read) = node.as_constant_read_node() {
        return utf8(read.name().as_slice());
    }
    let path = node.as_constant_path_node()?;
    let name = utf8(path.name()?.as_slice())?;
    match path.parent() {
        None => Some(format!("::{name}")),
        Some(parent) => Some(format!("{}::{name}", const_name(&parent)?)),
    }
}

/// `key: value` pairs with symbol keys from a trailing keyword hash (or hash literal).
fn keyword_args<'pr>(args: &[Node<'pr>]) -> Vec<(String, Node<'pr>)> {
    let Some(last) = args.last() else {
        return Vec::new();
    };
    let elements = if let Some(kh) = last.as_keyword_hash_node() {
        kh.elements()
    } else if let Some(h) = last.as_hash_node() {
        h.elements()
    } else {
        return Vec::new();
    };
    elements
        .iter()
        .filter_map(|e| {
            let assoc = e.as_assoc_node()?;
            let key = symbol_lit(&assoc.key())?;
            Some((key, assoc.value()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn associations_and_literals() {
        let f = collect(
            "module Admin\n  class Post < ApplicationRecord\n    belongs_to :author, class_name: 'User'\n    belongs_to :subject, polymorphic: true\n    has_many :comments\n    has_one :thing, class_name: name_for(:x)\n    has_many :tags, through: :taggings, source: :label\n  end\nend\n\"Foo::Bar\".constantize\nObject.const_get(:Baz)\nrequire_relative '../x'\n",
            false,
        );
        assert_eq!(f.assoc_polymorphic_skipped, 1);
        assert_eq!(f.assoc_dynamic_skipped, 1);
        let names: Vec<_> = f
            .inferred
            .iter()
            .map(|i| (i.name.clone(), i.scope.as_str(), i.line))
            .collect();
        assert_eq!(
            names,
            vec![
                (ClassName::Literal("User".into()), "Admin::Post", 3),
                (
                    ClassName::Association {
                        name: "comments".into(),
                        plural: true
                    },
                    "Admin::Post",
                    5
                ),
                (
                    ClassName::Association {
                        name: "label".into(),
                        plural: true
                    },
                    "Admin::Post",
                    7
                ),
                (ClassName::Literal("Foo::Bar".into()), "", 10),
                (ClassName::Literal("Baz".into()), "", 11),
            ]
        );
        assert_eq!(
            f.requires,
            vec![Require {
                kind: RequireKind::Relative,
                path: "../x".into(),
                line: 12
            }]
        );
    }
}
