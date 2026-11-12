//! Controller actions as edge scopes: which public action a line of a
//! controller sits in, and which action a view template belongs to.
//!
//! An action is a public instance `def` directly in the body of a class whose
//! name ends in `Controller`. A `def` after a bare `private`/`protected`, a
//! `private def`, a method named in `private :x`, a `def self.x`, anything in
//! `class << self`, a block (`included do`) or a module is not an action.
//! Callers treat "no action" as "the whole file" (sound).

use ruby_prism::Node;
use std::collections::HashSet;

/// One public action: name and its 1-based line range (inclusive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionSpan {
    pub name: String,
    pub start: u32,
    pub end: u32,
}

pub fn public_actions(src: &[u8]) -> Vec<ActionSpan> {
    let result = ruby_prism::parse(src);
    let mut out = Vec::new();
    container(&result.node(), &mut out);
    out
}

/// The action whose body contains `line`, if any.
pub fn action_at(actions: &[ActionSpan], line: u32) -> Option<&str> {
    actions.iter().find(|a| a.start <= line && line <= a.end).map(|a| a.name.as_str())
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn line_range(loc: &ruby_prism::Location<'_>) -> (u32, u32) {
    (loc.start_line().max(0) as u32, loc.end_line().max(0) as u32)
}

/// Program / module / statements: look for controller classes.
fn container(node: &Node<'_>, out: &mut Vec<ActionSpan>) {
    if let Some(p) = node.as_program_node() {
        for s in p.statements().body().iter() {
            container(&s, out);
        }
    } else if let Some(s) = node.as_statements_node() {
        for s in s.body().iter() {
            container(&s, out);
        }
    } else if let Some(m) = node.as_module_node() {
        if let Some(b) = m.body() {
            container(&b, out);
        }
    } else if let Some(c) = node.as_class_node() {
        let path = c.constant_path();
        let loc = path.location();
        let name = text(loc.as_slice());
        let is_controller = name.rsplit("::").next().is_some_and(|n| n.ends_with("Controller"));
        if let Some(b) = c.body() {
            if is_controller {
                class_body(&b, out);
            } else {
                container(&b, out);
            }
        }
    }
}

fn class_body(body: &Node<'_>, out: &mut Vec<ActionSpan>) {
    let Some(stmts) = body.as_statements_node() else { return };
    let mut public = true;
    let mut found: Vec<ActionSpan> = Vec::new();
    let mut hidden: HashSet<String> = HashSet::new();
    for stmt in stmts.body().iter() {
        if let Some(d) = stmt.as_def_node() {
            if d.receiver().is_none() && public {
                let (start, end) = line_range(&d.location());
                found.push(ActionSpan { name: text(d.name().as_slice()), start, end });
            }
        } else if let Some(call) = stmt.as_call_node() {
            if call.receiver().is_some() {
                continue;
            }
            let name = text(call.name().as_slice());
            if !matches!(name.as_str(), "private" | "protected" | "public") {
                continue;
            }
            match call.arguments() {
                None => public = name == "public",
                Some(args) => {
                    for a in args.arguments().iter() {
                        // `private def x` hides x; `private :a, :b` / `private "a"` hide those.
                        let target = if let Some(d) = a.as_def_node() {
                            Some(text(d.name().as_slice()))
                        } else if let Some(s) = a.as_symbol_node() {
                            Some(text(s.unescaped()))
                        } else {
                            a.as_string_node().map(|s| text(s.unescaped()))
                        };
                        if let Some(t) = target {
                            if name == "public" {
                                hidden.remove(&t);
                            } else {
                                hidden.insert(t);
                            }
                        }
                    }
                }
            }
        } else if stmt.as_class_node().is_some() || stmt.as_module_node().is_some() {
            container(&stmt, out);
        }
    }
    out.extend(found.into_iter().filter(|a| !hidden.contains(&a.name)));
}

/// `app/views/<controller>/<action>.<format>.<handler>` -> (`<prefix>`,
/// `<controller>`, `<action>`), where `<prefix>` is what precedes `app/views/`
/// (an engine root). Partials (`_x`) and layouts belong to no single action.
pub fn template_action(path: &str) -> Option<(&str, &str, &str)> {
    let i = path.find("app/views/")?;
    if i > 0 && !path[..i].ends_with('/') {
        return None;
    }
    let rest = &path[i + "app/views/".len()..];
    let (controller, file) = rest.rsplit_once('/')?;
    if controller.is_empty() || controller == "layouts" || controller.starts_with("layouts/") || file.starts_with('_') {
        return None;
    }
    let action = file.split('.').next()?;
    let ok = !action.is_empty() && action.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    ok.then_some((&path[..i], controller, action))
}

/// Actions that render a template of the given name by Rails convention:
/// the action itself, plus `create` for `new` and `update` for `edit`
/// (re-rendering the form on a failed save).
pub fn rendering_actions(action: &str) -> Vec<&str> {
    match action {
        "new" => vec!["new", "create"],
        "edit" => vec!["edit", "update"],
        a => vec![a],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(src: &str) -> Vec<(String, u32, u32)> {
        public_actions(src.as_bytes()).into_iter().map(|a| (a.name, a.start, a.end)).collect()
    }

    #[test]
    fn public_instance_methods_only() {
        let src = r#"
module Admin
  class UsersController < ApplicationController
    before_action :load

    def index
      render inertia: "Admin/Users/Index"
    end

    def self.helper; end
    private def secret; end
    def show = render(inertia: "Admin/Users/Show")
    def hidden_later; end
    private :hidden_later

    class << self
      def klass_method; end
    end

    protected

    def prot; end

    public

    def back_to_public; end

    private

    def load; end
  end
end
"#;
        assert_eq!(names(src), [("index".into(), 6, 8), ("show".into(), 12, 12), ("back_to_public".into(), 26, 26)]);
        let a = public_actions(src.as_bytes());
        assert_eq!(action_at(&a, 7), Some("index"));
        assert_eq!(action_at(&a, 30), None);
    }

    #[test]
    fn non_controllers_and_concerns_have_no_actions() {
        assert!(names("module Shared\n  extend ActiveSupport::Concern\n  included do\n    def x; end\n  end\n  def y; end\nend\n").is_empty());
        assert!(names("class Thing\n  def x; end\nend\n").is_empty());
        assert_eq!(names("class Api::V1::PostsController < Base\n  def index; end\nend\n"), [("index".into(), 2, 2)]);
    }

    #[test]
    fn templates_map_to_actions() {
        assert_eq!(template_action("app/views/admin/users/show.html.erb"), Some(("", "admin/users", "show")));
        assert_eq!(template_action("engines/x/app/views/posts/index.html.haml"), Some(("engines/x/", "posts", "index")));
        assert_eq!(template_action("app/views/posts/_form.html.erb"), None);
        assert_eq!(template_action("app/views/layouts/application.html.erb"), None);
        assert_eq!(template_action("app/views/show.html.erb"), None);
        assert_eq!(rendering_actions("edit"), ["edit", "update"]);
    }
}
