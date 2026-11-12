//! Controller action ranges, so that edges out of a controller can name the
//! action they sit in (`Edge::from_scope`).
//!
//! Soundness comes first: an action is only reported when code inside it can
//! run for that action's requests *only*. Anything else is left out, which
//! means "the whole file" to the edge builder. An action is a public instance
//! method defined directly in the body of a class whose name ends in
//! `Controller`, and it is dropped when:
//! - it is not public (bare `private`/`protected`, `private :x`, `private def x`),
//!   or the class changes visibility in a way we cannot read (`private *NAMES`);
//! - its name is defined more than once in the file (any class/module/`self.`);
//! - its name is mentioned anywhere outside its own body: a receiverless or
//!   `self.` call, a symbol or a string (`before_action :x`, `helper_method :x`,
//!   `send(:x)`, `alias_method`), except the values of `only:`/`except:`/
//!   `action:` options and `render` arguments, which never invoke a method;
//! - the file dispatches dynamically on the controller itself (receiverless or
//!   `self.` `send`/`public_send`/`__send__`/`method`/`try` with a non-literal
//!   name): then no action is reported at all (`policy.public_send(x)` is fine);
//! - it is a framework hook or helper-looking name (see [`NOT_ACTIONS`],
//!   names ending in `?`/`!`/`=`, names starting with `_`);
//! - its first or last line is shared with other code of the class body
//!   (`def a; X; end; def b; Y; end` on one line), since edges carry lines only.
//!
//! [`Action::template_scoped`] additionally says whether the action's implicit
//! template (`app/views/<prefix>/<action>.*`) belongs to it alone: false when
//! the name is rendered from elsewhere in the file (`render :show` in `create`).

use ruby_prism::{CallNode, ClassNode, DefNode, Node, StatementsNode, Visit};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub name: String,
    /// 1-based inclusive line range of the `def`.
    pub start: u32,
    pub end: u32,
    pub template_scoped: bool,
}

/// Public methods Rails or common gems call on every request (or from other
/// actions/views), so they are not actions even when public.
const NOT_ACTIONS: &[&str] = &[
    "initialize",
    "default_url_options",
    "url_options",
    "process",
    "process_action",
    "dispatch",
    "render",
    "render_to_body",
    "render_to_string",
    "redirect_to",
    "respond_with",
    "rescue_with_handler",
    "append_info_to_payload",
    "method_missing",
    "action_missing",
    "current_user",
    "current_ability",
    "pundit_user",
    "policy_scope",
    "authorize",
    "serialization_scope",
    "after_sign_in_path_for",
    "after_sign_out_path_for",
    "after_sign_up_path_for",
    "after_inactive_sign_up_path_for",
    "after_update_path_for",
    "after_resetting_password_path_for",
    "after_confirmation_path_for",
    "after_omniauth_failure_path_for",
    "signed_in_root_path",
    "stored_location_for",
    "verify_authenticity_token",
    "handle_unverified_request",
    "form_authenticity_token",
    "request_forgery_protection_token",
    "set_locale",
    "default_render",
    "action_methods",
    "params",
    "session",
    "cookies",
    "flash",
    "request",
    "response",
    "headers",
    "logger",
];

const DYNAMIC_SEND: &[&[u8]] = &[b"send", b"public_send", b"__send__", b"method", b"try", b"try!"];

#[derive(Debug)]
struct Candidate {
    name: String,
    start: u32,
    end: u32,
    public: bool,
    /// Shares a line with other code of the class body.
    conflict: bool,
    controller_class: bool,
}

#[derive(Default)]
struct Collector {
    candidates: Vec<Candidate>,
    /// def name -> number of definitions anywhere in the file.
    defs: HashMap<String, u32>,
    /// (name, line) of every potential invocation.
    mentions: Vec<(String, u32)>,
    /// (name, line) of every name passed to `render`.
    rendered: Vec<(String, u32)>,
    dynamic: bool,
}

pub fn collect(root: &Node) -> Vec<Action> {
    let mut c = Collector::default();
    c.visit(root);
    if c.dynamic {
        return Vec::new();
    }
    let outside = |list: &[(String, u32)], a: &Candidate| {
        list.iter()
            .any(|(n, l)| *n == a.name && !(a.start <= *l && *l <= a.end))
    };
    let mut out: Vec<Action> = c
        .candidates
        .iter()
        .filter(|a| {
            a.public
                && a.controller_class
                && !a.conflict
                && c.defs.get(&a.name).copied() == Some(1)
                && plausible_action(&a.name)
                && !outside(&c.mentions, a)
        })
        .map(|a| Action {
            name: a.name.clone(),
            start: a.start,
            end: a.end,
            template_scoped: !outside(&c.rendered, a),
        })
        .collect();
    out.sort_by_key(|a| a.start);
    // Ranges come from sibling statements, so they cannot overlap; be safe anyway.
    let overlaps: Vec<bool> = (0..out.len())
        .map(|i| {
            (i > 0 && out[i - 1].end >= out[i].start)
                || (i + 1 < out.len() && out[i].end >= out[i + 1].start)
        })
        .collect();
    let mut i = 0;
    out.retain(|_| {
        i += 1;
        !overlaps[i - 1]
    });
    out
}

fn plausible_action(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('_')
        && !name.ends_with(['?', '!', '='])
        && !NOT_ACTIONS.contains(&name)
}

/// The action whose body contains `line`, if any.
pub fn action_at(actions: &[Action], line: u32) -> Option<&Action> {
    if line == 0 {
        return None;
    }
    let i = actions.partition_point(|a| a.start <= line);
    let a = actions.get(i.checked_sub(1)?)?;
    (line <= a.end).then_some(a)
}

fn line(loc: &ruby_prism::Location) -> u32 {
    u32::try_from(loc.start_line()).unwrap_or(0)
}

fn end_line(loc: &ruby_prism::Location) -> u32 {
    u32::try_from(loc.end_line()).unwrap_or(0)
}

fn utf8(bytes: &[u8]) -> Option<String> {
    std::str::from_utf8(bytes).ok().map(String::from)
}

fn literal_name(node: &Node) -> Option<String> {
    if let Some(s) = node.as_symbol_node() {
        return utf8(s.unescaped());
    }
    node.as_string_node().and_then(|s| utf8(s.unescaped()))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Vis {
    Public,
    Hidden,
}

impl Collector {
    /// Reads one class body: visibility state and `def` candidates.
    fn class_body(&mut self, class: &ClassNode, body: &StatementsNode) {
        let controller_class = class_basename(&class.constant_path())
            .is_some_and(|n| n.ends_with("Controller"));
        let stmts: Vec<Node> = body.body().iter().collect();
        let spans: Vec<(u32, u32)> = stmts
            .iter()
            .map(|s| (line(&s.location()), end_line(&s.location())))
            .collect();
        let class_loc = class.location();
        let (class_start, class_end) = (line(&class_loc), end_line(&class_loc));
        let first = self.candidates.len();
        let mut vis = Vis::Public;
        let mut overrides: Vec<(String, Vis)> = Vec::new();
        let mut opaque = false;
        for (i, stmt) in stmts.iter().enumerate() {
            let conflict = (i > 0 && spans[i - 1].1 >= spans[i].0)
                || (i + 1 < stmts.len() && spans[i + 1].0 <= spans[i].1)
                || spans[i].0 == class_start
                || spans[i].1 == class_end;
            if let Some(def) = stmt.as_def_node() {
                self.candidate(&def, vis == Vis::Public, conflict, controller_class);
                continue;
            }
            let Some(call) = stmt.as_call_node() else { continue };
            if call.receiver().is_some() {
                continue;
            }
            let v = match call.name().as_slice() {
                b"public" => Vis::Public,
                b"private" | b"protected" | b"module_function" => Vis::Hidden,
                _ => continue,
            };
            let args: Vec<Node> = call
                .arguments()
                .map(|a| a.arguments().iter().collect())
                .unwrap_or_default();
            if args.is_empty() {
                vis = v;
                continue;
            }
            for arg in &args {
                if let Some(def) = arg.as_def_node() {
                    self.candidate(&def, v == Vis::Public, conflict, controller_class);
                } else if let Some(name) = literal_name(arg) {
                    overrides.push((name, v));
                } else {
                    opaque = true;
                }
            }
        }
        for c in &mut self.candidates[first..] {
            if opaque {
                c.public = false;
            }
            for (name, v) in &overrides {
                if *name == c.name {
                    c.public = *v == Vis::Public;
                }
            }
        }
    }

    fn candidate(&mut self, def: &DefNode, public: bool, conflict: bool, controller_class: bool) {
        if def.receiver().is_some() {
            return;
        }
        let Some(name) = utf8(def.name().as_slice()) else { return };
        let loc = def.location();
        self.candidates.push(Candidate {
            name,
            start: line(&loc),
            end: end_line(&loc),
            public,
            conflict,
            controller_class,
        });
    }

    fn call(&mut self, call: &CallNode) {
        let method = call.name().as_slice();
        let receiver = call.receiver();
        let receiverless = receiver.as_ref().is_none_or(|r| r.as_self_node().is_some());
        let at = line(&call.location());
        let args: Vec<Node> = call
            .arguments()
            .map(|a| a.arguments().iter().collect())
            .unwrap_or_default();
        if receiverless && let Some(name) = utf8(method) {
            self.mentions.push((name, at));
        }
        if receiverless
            && DYNAMIC_SEND.contains(&method)
            && args.first().is_some_and(|a| literal_name(a).is_none())
        {
            self.dynamic = true;
        }
        let render = receiverless && method == b"render";
        if let Some(r) = &receiver {
            self.visit(r);
        }
        for (i, arg) in args.iter().enumerate() {
            if render && i == 0 && let Some(name) = literal_name(arg) {
                self.rendered.push((last_segment(&name), at));
                continue;
            }
            let hash = arg
                .as_keyword_hash_node()
                .map(|h| h.elements())
                .or_else(|| arg.as_hash_node().map(|h| h.elements()));
            let Some(elements) = hash else {
                self.visit(arg);
                continue;
            };
            for el in elements.iter() {
                let Some(assoc) = el.as_assoc_node() else {
                    self.visit(&el);
                    continue;
                };
                let key = assoc.key();
                let key_name = key.as_symbol_node().and_then(|s| utf8(s.unescaped()));
                let value = assoc.value();
                match key_name.as_deref() {
                    Some("only" | "except") => {
                        // Lists of action names: not invocations. Anything non-literal is visited.
                        self.visit_names_except_literals(&value);
                    }
                    Some("action" | "template") if render => {
                        if let Some(name) = literal_name(&value) {
                            self.rendered.push((last_segment(&name), at));
                        } else {
                            self.visit(&value);
                        }
                    }
                    Some("action") => self.visit_names_except_literals(&value),
                    _ => {
                        self.visit(&key);
                        self.visit(&value);
                    }
                }
            }
        }
        if let Some(b) = call.block() {
            self.visit(&b);
        }
    }

    fn visit_names_except_literals(&mut self, value: &Node) {
        if literal_name(value).is_some() {
            return;
        }
        if let Some(arr) = value.as_array_node() {
            for e in arr.elements().iter() {
                if literal_name(&e).is_none() {
                    self.visit(&e);
                }
            }
            return;
        }
        self.visit(value);
    }
}

fn last_segment(name: &str) -> String {
    let file = name.rsplit('/').next().unwrap_or(name);
    file.split('.').next().unwrap_or(file).to_string()
}

fn class_basename(node: &Node) -> Option<String> {
    if let Some(read) = node.as_constant_read_node() {
        return utf8(read.name().as_slice());
    }
    let path = node.as_constant_path_node()?;
    utf8(path.name()?.as_slice())
}

impl<'pr> Visit<'pr> for Collector {
    fn visit_class_node(&mut self, node: &ClassNode<'pr>) {
        if let Some(stmts) = node.body().and_then(|b| b.as_statements_node()) {
            self.class_body(node, &stmts);
        }
        ruby_prism::visit_class_node(self, node);
    }

    fn visit_def_node(&mut self, node: &DefNode<'pr>) {
        if let Some(name) = utf8(node.name().as_slice()) {
            *self.defs.entry(name).or_default() += 1;
        }
        ruby_prism::visit_def_node(self, node);
    }

    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        self.call(node);
    }

    fn visit_symbol_node(&mut self, node: &ruby_prism::SymbolNode<'pr>) {
        if let Some(name) = utf8(node.unescaped()) {
            self.mentions.push((name, line(&node.location())));
        }
    }

    fn visit_string_node(&mut self, node: &ruby_prism::StringNode<'pr>) {
        if let Some(name) = utf8(node.unescaped()) {
            self.mentions.push((name, line(&node.location())));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actions(src: &str) -> Vec<(String, u32, u32, bool)> {
        let r = ruby_prism::parse(src.as_bytes());
        collect(&r.node())
            .into_iter()
            .map(|a| (a.name, a.start, a.end, a.template_scoped))
            .collect()
    }

    fn names(src: &str) -> Vec<String> {
        actions(src).into_iter().map(|a| a.0).collect()
    }

    #[test]
    fn public_methods_until_private() {
        let src = "class FooController < ApplicationController\n  before_action :load, only: [:show, :edit]\n\n  def index\n    X\n  end\n\n  def show\n  end\n\n  private\n\n  def load\n  end\nend\n";
        assert_eq!(
            actions(src),
            vec![("index".into(), 4, 6, true), ("show".into(), 8, 9, true)]
        );
    }

    #[test]
    fn visibility_forms() {
        let src = "class FooController\n  def a; end\n  private def b; end\n  def c; end\n  private :c\n  def self.d; end\n  class << self\n    def e; end\n  end\n  protected\n  def f; end\n  public\n  def g; end\nend\n";
        assert_eq!(names(src), vec!["a", "g"]);
    }

    #[test]
    fn opaque_visibility_drops_class() {
        let src = "class FooController\n  def a\n  end\n  private(*HIDDEN)\nend\n";
        assert!(names(src).is_empty());
    }

    #[test]
    fn mentioned_or_duplicated_names_are_dropped() {
        let src = "class FooController\n  before_action :a\n  def a\n  end\n  def b\n    c\n  end\n  def c\n  end\n  def d\n    redirect_to action: :b\n  end\n  def e\n  end\nend\nclass Helper\n  def e; end\nend\n";
        // a: filter symbol; c: called from b; e: defined twice. b's `action:` mention does not count.
        assert_eq!(names(src), vec!["b", "d"]);
    }

    #[test]
    fn rendered_elsewhere_keeps_code_scope_but_not_template() {
        let src = "class FooController\n  def new\n  end\n  def create\n    render :new\n  end\nend\n";
        assert_eq!(
            actions(src),
            vec![("new".into(), 2, 3, false), ("create".into(), 4, 6, true)]
        );
    }

    #[test]
    fn dynamic_send_disables_everything() {
        let src = "class FooController\n  def a\n    send(params[:x])\n  end\nend\n";
        assert!(names(src).is_empty());
        let src = "class FooController\n  def a\n    policy.public_send(params[:x])\n  end\nend\n";
        assert_eq!(names(src), vec!["a"]);
    }

    #[test]
    fn non_controller_classes_and_hooks_and_shared_lines() {
        let src = "class Foo\n  def a; end\nend\nclass BarController\n  def default_url_options; end\n  def ok?; end\n  def x; end; def y; end\n  def z\n  end\nend\n";
        assert_eq!(names(src), vec!["z"]);
    }

    #[test]
    fn lookup() {
        let a = vec![
            Action { name: "a".into(), start: 3, end: 5, template_scoped: true },
            Action { name: "b".into(), start: 8, end: 8, template_scoped: true },
        ];
        let at = |l| action_at(&a, l).map(|a| a.name.as_str());
        assert_eq!((at(2), at(3), at(5), at(6), at(8), at(9), at(0)), (None, Some("a"), Some("a"), None, Some("b"), None, None));
    }
}
