//! A tiny evaluator for the literal Ruby that appears in route files: strings,
//! symbols, arrays, hashes, interpolation over loop variables, and a handful of
//! `String` methods (`pluralize`, `==`, ...). Anything else evaluates to
//! [`Val::Unknown`] and the caller decides what that means.

use crate::inflect;
use ruby_prism::Node;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Str(String),
    Sym(String),
    Int(i64),
    Bool(bool),
    Nil,
    Array(Vec<Val>),
    Hash(Vec<(Val, Val)>),
    Regex,
    /// A lambda/proc literal (request constraints, rack endpoints).
    Lambda,
    /// `redirect(...)` (or a variable bound to one).
    Redirect,
    /// A constant reference (`Sidekiq::Web`, `SomeConstraint.new`).
    Const(String),
    Unknown,
}

impl Val {
    /// The text of a string or symbol.
    pub fn text(&self) -> Option<&str> {
        match self {
            Val::Str(s) | Val::Sym(s) => Some(s),
            _ => None,
        }
    }

    pub fn truthy(&self) -> Option<bool> {
        match self {
            Val::Bool(b) => Some(*b),
            Val::Nil => Some(false),
            Val::Unknown => None,
            _ => Some(true),
        }
    }

    /// Scalars become one-element lists, as `Array(x)` does.
    pub fn list(&self) -> Vec<Val> {
        match self {
            Val::Array(v) => v.clone(),
            Val::Nil => Vec::new(),
            other => vec![other.clone()],
        }
    }

    /// Texts of a symbol/string or a list of them (`only: [:index, :show]`).
    pub fn texts(&self) -> Vec<String> {
        self.list().iter().filter_map(|v| v.text().map(str::to_string)).collect()
    }
}

/// Positional arguments plus the trailing keyword hash of a call.
#[derive(Debug, Clone, Default)]
pub struct Args {
    pub pos: Vec<Val>,
    pub opts: Vec<(Val, Val)>,
    /// A `&block` argument or `**splat` we could not see through.
    pub opaque: bool,
}

impl Args {
    pub fn get(&self, key: &str) -> Option<&Val> {
        self.opts.iter().find(|(k, _)| matches!(k, Val::Sym(s) if s == key)).map(|(_, v)| v)
    }

    pub fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn text(&self, key: &str) -> Option<String> {
        self.get(key).and_then(|v| v.text().map(str::to_string))
    }

    pub fn take(&mut self, key: &str) -> Option<Val> {
        let i = self.opts.iter().position(|(k, _)| matches!(k, Val::Sym(s) if s == key))?;
        Some(self.opts.remove(i).1)
    }

    pub fn set(&mut self, key: &str, val: Val) {
        self.take(key);
        self.opts.push((Val::Sym(key.to_string()), val));
    }

    /// Merges `defaults` underneath this call's own options (`with_options`).
    pub fn merge_defaults(&mut self, defaults: &[(Val, Val)]) {
        for (k, v) in defaults {
            if !self.opts.iter().any(|(own, _)| own == k) {
                self.opts.push((k.clone(), v.clone()));
            }
        }
    }

    /// The `"path" => "controller#action"` pair of the hash-rocket form.
    pub fn string_key_pair(&self) -> Option<(String, Val)> {
        self.opts.iter().find_map(|(k, v)| match k {
            Val::Str(s) => Some((s.clone(), v.clone())),
            _ => None,
        })
    }

    pub fn remove_string_keys(&mut self) {
        self.opts.retain(|(k, _)| !matches!(k, Val::Str(_)));
    }
}

pub type Env = HashMap<String, Val>;

fn bytes(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// The source name of a constant read or path (`Sidekiq::Web`).
pub fn const_name(node: &Node<'_>) -> Option<String> {
    match node {
        Node::ConstantReadNode { .. } => Some(bytes(node.as_constant_read_node()?.name().as_slice())),
        Node::ConstantPathNode { .. } => {
            let n = node.as_constant_path_node()?;
            let name = bytes(n.name()?.as_slice());
            match n.parent() {
                Some(parent) => Some(format!("{}::{name}", const_name(&parent)?)),
                None => Some(name),
            }
        }
        _ => None,
    }
}

pub fn eval(node: &Node<'_>, env: &Env) -> Val {
    match node {
        Node::StringNode { .. } => node.as_string_node().map_or(Val::Unknown, |n| Val::Str(bytes(n.unescaped()))),
        Node::SymbolNode { .. } => node.as_symbol_node().map_or(Val::Unknown, |n| Val::Sym(bytes(n.unescaped()))),
        Node::IntegerNode { .. } => {
            let text = bytes(node.location().as_slice()).replace('_', "");
            text.parse().map_or(Val::Unknown, Val::Int)
        }
        Node::TrueNode { .. } => Val::Bool(true),
        Node::FalseNode { .. } => Val::Bool(false),
        Node::NilNode { .. } => Val::Nil,
        Node::RegularExpressionNode { .. } | Node::InterpolatedRegularExpressionNode { .. } => Val::Regex,
        Node::LambdaNode { .. } => Val::Lambda,
        Node::ArrayNode { .. } => {
            let Some(arr) = node.as_array_node() else { return Val::Unknown };
            let items: Vec<Val> = arr.elements().iter().map(|e| eval(&e, env)).collect();
            if items.contains(&Val::Unknown) { Val::Unknown } else { Val::Array(items) }
        }
        Node::HashNode { .. } => node.as_hash_node().map_or(Val::Unknown, |h| Val::Hash(eval_assocs(h.elements().iter(), env))),
        Node::KeywordHashNode { .. } => node.as_keyword_hash_node().map_or(Val::Unknown, |h| Val::Hash(eval_assocs(h.elements().iter(), env))),
        Node::InterpolatedStringNode { .. } => {
            let Some(n) = node.as_interpolated_string_node() else { return Val::Unknown };
            interpolate(n.parts().iter(), env).map_or(Val::Unknown, Val::Str)
        }
        Node::InterpolatedSymbolNode { .. } => {
            let Some(n) = node.as_interpolated_symbol_node() else { return Val::Unknown };
            interpolate(n.parts().iter(), env).map_or(Val::Unknown, Val::Sym)
        }
        Node::LocalVariableReadNode { .. } => {
            let Some(n) = node.as_local_variable_read_node() else { return Val::Unknown };
            env.get(&bytes(n.name().as_slice())).cloned().unwrap_or(Val::Unknown)
        }
        Node::ConstantReadNode { .. } | Node::ConstantPathNode { .. } => {
            let name = const_name(node).unwrap_or_default();
            env.get(&name).cloned().unwrap_or(Val::Const(name))
        }
        Node::ParenthesesNode { .. } => {
            let Some(body) = node.as_parentheses_node().and_then(|p| p.body()) else { return Val::Nil };
            last_statement(&body).map_or(Val::Unknown, |n| eval(&n, env))
        }
        Node::IfNode { .. } => {
            // Ternaries over loop variables (canvas: `context == "global" ? a : b`).
            let Some(n) = node.as_if_node() else { return Val::Unknown };
            match eval(&n.predicate(), env).truthy() {
                Some(true) => n.statements().and_then(|s| s.body().iter().last()).map_or(Val::Nil, |s| eval(&s, env)),
                Some(false) => match n.subsequent() {
                    Some(Node::ElseNode { .. }) => n
                        .subsequent()
                        .and_then(|e| e.as_else_node())
                        .and_then(|e| e.statements())
                        .and_then(|s| s.body().iter().last())
                        .map_or(Val::Nil, |s| eval(&s, env)),
                    Some(other) => eval(&other, env),
                    None => Val::Nil,
                },
                None => Val::Unknown,
            }
        }
        Node::CallNode { .. } => node.as_call_node().map_or(Val::Unknown, |c| eval_call(&c, env)),
        _ => Val::Unknown,
    }
}

fn last_statement<'pr>(body: &Node<'pr>) -> Option<Node<'pr>> {
    match body {
        Node::StatementsNode { .. } => body.as_statements_node()?.body().iter().last(),
        _ => None,
    }
}

fn eval_assocs<'pr>(elements: impl Iterator<Item = Node<'pr>>, env: &Env) -> Vec<(Val, Val)> {
    let mut out = Vec::new();
    for e in elements {
        if let Some(a) = e.as_assoc_node() {
            out.push((eval(&a.key(), env), eval(&a.value(), env)));
        } else {
            out.push((Val::Unknown, Val::Unknown));
        }
    }
    out
}

fn interpolate<'pr>(parts: impl Iterator<Item = Node<'pr>>, env: &Env) -> Option<String> {
    let mut out = String::new();
    for part in parts {
        match &part {
            Node::StringNode { .. } => out.push_str(&bytes(part.as_string_node()?.unescaped())),
            Node::EmbeddedStatementsNode { .. } => {
                let stmt = part.as_embedded_statements_node()?.statements()?.body().iter().last()?;
                match eval(&stmt, env) {
                    Val::Str(s) | Val::Sym(s) => out.push_str(&s),
                    Val::Int(i) => out.push_str(&i.to_string()),
                    _ => return None,
                }
            }
            _ => return None,
        }
    }
    Some(out)
}

fn eval_call(call: &ruby_prism::CallNode<'_>, env: &Env) -> Val {
    let name = bytes(call.name().as_slice());
    let Some(recv) = call.receiver() else {
        return match name.as_str() {
            "redirect" => Val::Redirect,
            "lambda" | "proc" => Val::Lambda,
            _ => env.get(&name).cloned().unwrap_or(Val::Unknown),
        };
    };
    if name == "lambda" || name == "proc" || (name == "new" && matches!(recv, Node::ConstantReadNode { .. } | Node::ConstantPathNode { .. })) {
        if name == "new" {
            return Val::Const(const_name(&recv).unwrap_or_default());
        }
        return Val::Lambda;
    }
    if name == "union" && const_name(&recv).as_deref() == Some("Regexp") {
        return Val::Regex;
    }
    let target = eval(&recv, env);
    let arg = || call.arguments().and_then(|a| a.arguments().iter().next()).map(|a| eval(&a, env));
    match (target, name.as_str()) {
        (Val::Str(s) | Val::Sym(s), "pluralize") => Val::Str(inflect::pluralize(&s)),
        (Val::Str(s) | Val::Sym(s), "singularize") => Val::Str(inflect::singularize(&s)),
        (Val::Str(s) | Val::Sym(s), "to_s") => Val::Str(s),
        (Val::Str(s) | Val::Sym(s), "to_sym") => Val::Sym(s),
        (Val::Str(s) | Val::Sym(s), "downcase") => Val::Str(s.to_lowercase()),
        (Val::Str(s) | Val::Sym(s), "upcase") => Val::Str(s.to_uppercase()),
        (Val::Str(s) | Val::Sym(s), "underscore") => Val::Str(s.replace("::", "/").to_lowercase()),
        (Val::Array(v), "freeze") => Val::Array(v),
        (Val::Str(s), "freeze") => Val::Str(s),
        (Val::Array(v), "flatten") => Val::Array(v.into_iter().flat_map(|x| x.list()).collect()),
        (lhs, "==") => match arg() {
            Some(rhs) if lhs != Val::Unknown && rhs != Val::Unknown => Val::Bool(lhs == rhs),
            _ => Val::Unknown,
        },
        (lhs, "!=") => match arg() {
            Some(rhs) if lhs != Val::Unknown && rhs != Val::Unknown => Val::Bool(lhs != rhs),
            _ => Val::Unknown,
        },
        (Val::Array(v), "include?") => match arg() {
            Some(x) if x != Val::Unknown => Val::Bool(v.contains(&x)),
            _ => Val::Unknown,
        },
        (Val::Str(a), "+") => match arg() {
            Some(Val::Str(b)) => Val::Str(a + &b),
            _ => Val::Unknown,
        },
        (Val::Array(a), "+") => match arg() {
            Some(Val::Array(b)) => Val::Array(a.into_iter().chain(b).collect()),
            _ => Val::Unknown,
        },
        _ => Val::Unknown,
    }
}

/// Evaluates call arguments. A trailing `KeywordHashNode` (or a literal hash
/// in last position) becomes [`Args::opts`].
pub fn eval_args(args: Option<ruby_prism::ArgumentsNode<'_>>, env: &Env) -> Args {
    let mut out = Args::default();
    let Some(args) = args else { return out };
    let list: Vec<Node<'_>> = args.arguments().iter().collect();
    let n = list.len();
    for (i, a) in list.into_iter().enumerate() {
        match &a {
            Node::KeywordHashNode { .. } => {
                if let Some(h) = a.as_keyword_hash_node() {
                    for e in h.elements().iter() {
                        match e.as_assoc_node() {
                            Some(assoc) => out.opts.push((eval(&assoc.key(), env), eval(&assoc.value(), env))),
                            None => out.opaque = true,
                        }
                    }
                }
            }
            Node::HashNode { .. } if i + 1 == n => {
                if let Val::Hash(pairs) = eval(&a, env) {
                    out.opts.extend(pairs);
                }
            }
            Node::BlockArgumentNode { .. } | Node::SplatNode { .. } => out.opaque = true,
            _ => out.pos.push(eval(&a, env)),
        }
    }
    out
}
