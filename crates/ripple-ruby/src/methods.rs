//! Per-file method facts for method-level scopes, from the ruby-prism tree:
//!
//! - **defs**: outermost `def`s (any receiver, inside `class << self`,
//!   blocks such as `included do`), with lines and byte offsets. A `def`
//!   nested in another belongs to the outer one.
//! - **mentions**: every method-call name (any receiver or none, including
//!   `a.b += 1`, `a.b ||= 1`, `a.b, c = ...`), every symbol literal, every
//!   identifier-like string literal, each with its enclosing outermost `def`
//!   (or none at class/module/top level, including class-level blocks and
//!   lambdas). Constructors (`new`, `create`, ...) also mention
//!   `initialize`. The literal arguments of visibility modifiers
//!   (`private :x`) are not mentions.
//! - **dynamic** sites: `send`/`public_send`/`__send__`/`try`/`method`/
//!   `instance_method` with a non-literal name, any `eval`,
//!   `class_eval`/`instance_eval`/`module_eval` with arguments (a string),
//!   and `def method_missing`; each with its enclosing def and whether the
//!   receiver is `self` (or none). `define_method` with a computed name
//!   defines rather than calls, and `*_exec` runs a block whose code is
//!   visible (its mentions count where it is written): neither is dynamic.
//! - **inherit ranges**: byte ranges of superclass constants and of constant
//!   arguments to receiverless/`self.` `include`/`extend`/`prepend`, so the
//!   Rubydex constant references there can be told apart.
//!
//! Names are interned per file (`names`); `State::apply` maps them to
//! workspace-wide ids. Everything here depends only on the file's content.

use ruby_prism::{
    CallAndWriteNode, CallNode, CallOperatorWriteNode, CallOrWriteNode, CallTargetNode, ClassNode,
    DefNode, Node, StringNode, SymbolNode, Visit,
};
use std::collections::HashMap;

/// "No enclosing def": the mention sits at class/module/top level.
pub const NONE: u32 = u32::MAX;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Def {
    /// Name id (local until `State::apply`, then workspace-wide).
    pub name: u32,
    /// 1-based inclusive lines.
    pub start: u32,
    pub end: u32,
    /// Byte offsets `[start_off, end_off)`.
    pub start_off: u32,
    pub end_off: u32,
    /// Nothing but whitespace before the `def` on its first line and nothing
    /// but whitespace or a comment after its `end` on its last line, so every
    /// line in the range belongs to this method alone.
    pub clean: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mention {
    pub name: u32,
    /// Index into `defs`, or [`NONE`].
    pub scope: u32,
    /// First line of this (name, scope) pair.
    pub line: u32,
    /// Counts towards "is this name mentioned anywhere" (rule 5). Crude
    /// template mentions are used for edges only.
    pub counted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dynamic {
    /// Index into `defs`, or [`NONE`].
    pub scope: u32,
    /// Receiverless or `self.`: may call any method of this object.
    pub on_self: bool,
    pub line: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MethodFacts {
    /// Local name table (emptied once the ids are global).
    pub names: Vec<String>,
    /// Sorted by `start_off`; non-overlapping.
    pub defs: Vec<Def>,
    /// One per distinct (name, scope), with the first line.
    pub mentions: Vec<Mention>,
    /// Per name: number of counted mentions outside a def of the same name.
    pub counts: Vec<(u32, u32)>,
    /// Distinct (scope, on_self).
    pub dynamic: Vec<Dynamic>,
    pub inherit_ranges: Vec<(u32, u32)>,
    /// Distinct def names, sorted (filled with global ids by `State::apply`).
    pub own: Vec<u32>,
}

/// Calls that run `initialize` on a new instance of the receiver.
const CONSTRUCTORS: &[&[u8]] = &[
    b"new",
    b"create",
    b"create!",
    b"build",
    b"find_or_create_by",
    b"find_or_create_by!",
    b"find_or_initialize_by",
    b"first_or_create",
    b"first_or_create!",
    b"first_or_initialize",
    b"create_or_find_by",
    b"create_or_find_by!",
    b"create_with",
];

const VISIBILITY: &[&[u8]] = &[
    b"private",
    b"public",
    b"protected",
    b"private_class_method",
    b"public_class_method",
    b"module_function",
    b"private_constant",
    b"public_constant",
];

/// Dispatch by a name given as the first argument.
const BY_NAME: &[&[u8]] = &[
    b"send",
    b"public_send",
    b"__send__",
    b"try",
    b"try!",
    b"method",
    b"public_method",
    b"instance_method",
    b"public_instance_method",
];

/// Define a method; a string-literal name is a mention, a computed one is not dynamic.
const DEFINE: &[&[u8]] = &[b"define_method", b"define_singleton_method"];

const EVAL_WITH_ARGS: &[&[u8]] = &[b"class_eval", b"instance_eval", b"module_eval"];

const MIXINS: &[&[u8]] = &[b"include", b"extend", b"prepend"];

pub fn collect(root: &Node, source: &[u8]) -> MethodFacts {
    let mut c = Collector { source, cur: NONE, ..Collector::default() };
    c.visit(root);
    c.finish()
}

pub fn is_constructor(name: &str) -> bool {
    CONSTRUCTORS.contains(&name.as_bytes())
}

/// Facts for a template: its mentions (all at top level), no defs.
pub fn from_template(t: &crate::templates::TemplateFacts) -> MethodFacts {
    let mut names: Vec<String> = Vec::new();
    let mut index: HashMap<&str, u32> = HashMap::new();
    let mut mentions = Vec::new();
    for (list, counted) in [(&t.mentions, true), (&t.crude, false)] {
        for (name, line) in list {
            let id = *index.entry(name.as_str()).or_insert_with(|| {
                names.push(name.clone());
                (names.len() - 1) as u32
            });
            mentions.push(Mention { name: id, scope: NONE, line: *line, counted });
        }
    }
    mentions.sort_unstable_by_key(|m| (m.name, m.line, !m.counted));
    // Counted first per name when lines tie; keep one per name, counted if any.
    let mut out: Vec<Mention> = Vec::with_capacity(mentions.len());
    for m in mentions {
        match out.last_mut() {
            Some(last) if last.name == m.name => last.counted |= m.counted,
            _ => out.push(m),
        }
    }
    let counts = out.iter().filter(|m| m.counted).map(|m| (m.name, 1)).collect();
    let dynamic = if t.dynamic { vec![Dynamic { scope: NONE, on_self: true, line: 0 }] } else { Vec::new() };
    MethodFacts { names, defs: Vec::new(), mentions: out, counts, dynamic, inherit_ranges: Vec::new(), own: Vec::new() }
}

/// Identifier-like: `foo`, `foo?`, `foo!`, `foo=`, `_x1`. Used for strings.
pub fn identifier_like(s: &[u8]) -> bool {
    let Some((&first, rest)) = s.split_first() else { return false };
    if !(first.is_ascii_lowercase() || first == b'_') {
        return false;
    }
    let body = match rest.last() {
        Some(b'?' | b'!' | b'=') => &rest[..rest.len() - 1],
        _ => rest,
    };
    body.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

#[derive(Default)]
struct Collector<'s> {
    source: &'s [u8],
    names: Vec<String>,
    index: HashMap<Vec<u8>, u32>,
    defs: Vec<Def>,
    /// (name, scope, line, counted)
    raw: Vec<(u32, u32, u32, bool)>,
    dynamic: Vec<Dynamic>,
    inherit_ranges: Vec<(u32, u32)>,
    cur: u32,
}

fn line(loc: &ruby_prism::Location) -> u32 {
    u32::try_from(loc.start_line()).unwrap_or(0)
}

fn literal(node: &Node) -> Option<Vec<u8>> {
    if let Some(s) = node.as_symbol_node() {
        return Some(s.unescaped().to_vec());
    }
    node.as_string_node().map(|s| s.unescaped().to_vec())
}

fn is_constant(node: &Node) -> bool {
    node.as_constant_read_node().is_some() || node.as_constant_path_node().is_some()
}

impl Collector<'_> {
    fn intern(&mut self, name: &[u8]) -> u32 {
        if let Some(&i) = self.index.get(name) {
            return i;
        }
        let i = self.names.len() as u32;
        self.names.push(String::from_utf8_lossy(name).into_owned());
        self.index.insert(name.to_vec(), i);
        i
    }

    fn mention(&mut self, name: &[u8], line: u32) {
        if name.is_empty() {
            return;
        }
        let id = self.intern(name);
        self.raw.push((id, self.cur, line, true));
    }

    fn dynamic(&mut self, on_self: bool, line: u32) {
        self.dynamic.push(Dynamic { scope: self.cur, on_self, line });
    }

    fn clean(&self, start: usize, end: usize) -> bool {
        let s = self.source;
        let line_start = s[..start.min(s.len())].iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        let before_ok = s[line_start..start.min(s.len())].iter().all(|b| *b == b' ' || *b == b'\t');
        let line_end = s[end.min(s.len())..].iter().position(|b| *b == b'\n').map_or(s.len(), |i| end + i);
        let after = &s[end.min(s.len())..line_end];
        let trimmed: Vec<u8> = after.iter().copied().skip_while(|b| *b == b' ' || *b == b'\t' || *b == b'\r').collect();
        before_ok && (trimmed.is_empty() || trimmed[0] == b'#')
    }

    fn finish(mut self) -> MethodFacts {
        self.dynamic.sort_by_key(|d| (d.scope, d.on_self, d.line));
        self.dynamic.dedup_by_key(|d| (d.scope, d.on_self));
        // Counted mentions outside a def of the same name ("its own definition").
        let mut counts: HashMap<u32, u32> = HashMap::new();
        for &(name, scope, _, counted) in &self.raw {
            if counted && !(scope != NONE && self.defs[scope as usize].name == name) {
                *counts.entry(name).or_default() += 1;
            }
        }
        let mut counts: Vec<(u32, u32)> = counts.into_iter().collect();
        counts.sort_unstable();
        self.raw.sort_unstable_by_key(|&(n, s, l, c)| (n, s, l, !c));
        self.raw.dedup_by_key(|r| (r.0, r.1));
        let mentions = self.raw.iter().map(|&(name, scope, line, counted)| Mention { name, scope, line, counted }).collect();
        let mut own: Vec<u32> = self.defs.iter().map(|d| d.name).collect();
        own.sort_unstable();
        own.dedup();
        MethodFacts {
            names: self.names,
            defs: self.defs,
            mentions,
            counts,
            dynamic: self.dynamic,
            inherit_ranges: self.inherit_ranges,
            own,
        }
    }
}

impl<'pr> Visit<'pr> for Collector<'_> {
    fn visit_def_node(&mut self, node: &DefNode<'pr>) {
        let name = node.name().as_slice();
        if self.cur != NONE {
            if name == b"method_missing" {
                self.dynamic(true, line(&node.location()));
            }
            ruby_prism::visit_def_node(self, node);
            return;
        }
        let loc = node.location();
        let id = self.intern(name);
        let (start_off, end_off) = (loc.start(), loc.end());
        let clean = self.clean(start_off as usize, end_off as usize);
        self.defs.push(Def {
            name: id,
            start: line(&loc),
            end: u32::try_from(loc.end_line()).unwrap_or(0),
            start_off,
            end_off,
            clean,
        });
        self.cur = (self.defs.len() - 1) as u32;
        if name == b"method_missing" {
            self.dynamic(true, line(&loc));
        }
        ruby_prism::visit_def_node(self, node);
        self.cur = NONE;
    }

    fn visit_class_node(&mut self, node: &ClassNode<'pr>) {
        if let Some(sup) = node.superclass()
            && is_constant(&sup)
        {
            let l = sup.location();
            self.inherit_ranges.push((l.start(), l.end()));
        }
        ruby_prism::visit_class_node(self, node);
    }

    fn visit_call_node(&mut self, node: &CallNode<'pr>) {
        let name = node.name().as_slice();
        let receiver = node.receiver();
        let on_self = receiver.as_ref().is_none_or(|r| r.as_self_node().is_some());
        let at = line(&node.location());
        self.mention(name, at);
        if CONSTRUCTORS.contains(&name) {
            self.mention(b"initialize", at);
        }
        if name == b"dup" || name == b"clone" {
            self.mention(b"initialize_copy", at);
        }
        let args: Vec<Node> = node.arguments().map(|a| a.arguments().iter().collect()).unwrap_or_default();
        if receiver.is_none() && VISIBILITY.contains(&name) {
            // `private :x, "y"` / `private %i[x y]`: the names are not invocations.
            for arg in &args {
                if literal(arg).is_some() {
                    continue;
                }
                if let Some(arr) = arg.as_array_node()
                    && arr.elements().iter().all(|e| literal(&e).is_some())
                {
                    continue;
                }
                self.visit(arg);
            }
            if let Some(b) = node.block() {
                self.visit(&b);
            }
            return;
        }
        if on_self && MIXINS.contains(&name) {
            for arg in &args {
                if is_constant(arg) {
                    let l = arg.location();
                    self.inherit_ranges.push((l.start(), l.end()));
                }
            }
        }
        if BY_NAME.contains(&name) {
            match args.first() {
                Some(first) => match first.as_string_node() {
                    Some(s) => self.mention(s.unescaped(), at),
                    None if first.as_symbol_node().is_some() => {}
                    None => self.dynamic(on_self, at),
                },
                None => {}
            }
        } else if DEFINE.contains(&name) {
            if let Some(s) = args.first().and_then(|a| a.as_string_node()) {
                self.mention(s.unescaped(), at);
            }
        } else if name == b"eval" || (EVAL_WITH_ARGS.contains(&name) && !args.is_empty()) {
            self.dynamic(on_self, at);
        }
        // Names that select actions or templates are not invocations:
        // `before_action :x, only: [:show]`, `render :new`, `redirect_to action: :index`.
        if let Some(r) = &receiver {
            self.visit(r);
        }
        let render = receiver.is_none() && name == b"render";
        for (i, arg) in args.iter().enumerate() {
            if render && i == 0 && literal(arg).is_some() {
                continue;
            }
            let hash = arg.as_keyword_hash_node().map(|h| h.elements()).or_else(|| arg.as_hash_node().map(|h| h.elements()));
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
                let selector = key.as_symbol_node().is_some_and(|k| matches!(k.unescaped(), b"only" | b"except" | b"action" | b"template"));
                let value = assoc.value();
                let literal_list = literal(&value).is_some()
                    || value.as_array_node().is_some_and(|a| a.elements().iter().all(|e| literal(&e).is_some()));
                if !(selector && literal_list) {
                    self.visit(&key);
                    self.visit(&value);
                }
            }
        }
        if let Some(b) = node.block() {
            self.visit(&b);
        }
    }

    fn visit_call_operator_write_node(&mut self, node: &CallOperatorWriteNode<'pr>) {
        let at = line(&node.location());
        self.mention(node.read_name().as_slice(), at);
        self.mention(node.write_name().as_slice(), at);
        ruby_prism::visit_call_operator_write_node(self, node);
    }

    fn visit_call_or_write_node(&mut self, node: &CallOrWriteNode<'pr>) {
        let at = line(&node.location());
        self.mention(node.read_name().as_slice(), at);
        self.mention(node.write_name().as_slice(), at);
        ruby_prism::visit_call_or_write_node(self, node);
    }

    fn visit_call_and_write_node(&mut self, node: &CallAndWriteNode<'pr>) {
        let at = line(&node.location());
        self.mention(node.read_name().as_slice(), at);
        self.mention(node.write_name().as_slice(), at);
        ruby_prism::visit_call_and_write_node(self, node);
    }

    fn visit_call_target_node(&mut self, node: &CallTargetNode<'pr>) {
        self.mention(node.name().as_slice(), line(&node.location()));
        ruby_prism::visit_call_target_node(self, node);
    }

    fn visit_symbol_node(&mut self, node: &SymbolNode<'pr>) {
        self.mention(node.unescaped(), line(&node.location()));
    }

    fn visit_string_node(&mut self, node: &StringNode<'pr>) {
        let s = node.unescaped();
        if identifier_like(s) {
            self.mention(s, line(&node.location()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(src: &str) -> MethodFacts {
        let r = ruby_prism::parse(src.as_bytes());
        collect(&r.node(), src.as_bytes())
    }

    fn mentions(f: &MethodFacts) -> Vec<String> {
        let mut v: Vec<String> = f
            .mentions
            .iter()
            .map(|m| {
                let scope = if m.scope == NONE { "-".to_string() } else { f.names[f.defs[m.scope as usize].name as usize].clone() };
                format!("{}@{}", f.names[m.name as usize], scope)
            })
            .collect();
        v.sort();
        v
    }

    #[test]
    fn defs_and_enclosing_scopes() {
        let src = "class A < B\n  include C, D::E\n  before_save :audit\n  scope :x, -> { where(y: 1).refund! }\n  def refund!\n    charge!(Foo.new)\n    def inner; zap; end\n  end\n  def self.build; new; end\n  class << self\n    def k; end\n  end\n  private :refund!\nend\n";
        let f = facts(src);
        let defs: Vec<(String, u32, u32, bool)> = f.defs.iter().map(|d| (f.names[d.name as usize].clone(), d.start, d.end, d.clean)).collect();
        assert_eq!(
            defs,
            vec![("refund!".into(), 5, 8, true), ("build".into(), 9, 9, true), ("k".into(), 11, 11, true)]
        );
        let m = mentions(&f);
        for want in ["audit@-", "refund!@-", "where@-", "y@-", "charge!@refund!", "new@refund!", "initialize@refund!", "zap@refund!", "new@build", "initialize@build", "include@-", "before_save@-"] {
            assert!(m.contains(&want.to_string()), "{want} in {m:?}");
        }
        // `private :refund!` does not mention it: only the lambda at line 4 does.
        let refund = f.mentions.iter().find(|x| f.names[x.name as usize] == "refund!").unwrap();
        assert_eq!(refund.line, 4);
        assert_eq!(f.inherit_ranges.len(), 3);
        assert!(f.dynamic.is_empty());
    }

    #[test]
    fn dynamic_sites_and_send_literals() {
        let src = "class A\n  def a\n    send(:b)\n    public_send(\"c\")\n    obj.try(:d)\n  end\n  def e\n    send(name)\n  end\n  def f\n    other.public_send(x)\n  end\n  def method_missing(n)\n  end\n  def g\n    request.method\n    message.send\n  end\n  def h\n    define_method(\"#{x}_y\") { 1 }\n    instance_exec(1, &blk)\n  end\nend\nobj.instance_eval(code)\n";
        let f = facts(src);
        let m = mentions(&f);
        for want in ["b@a", "c@a", "d@a"] {
            assert!(m.contains(&want.to_string()), "{want} in {m:?}");
        }
        let d: Vec<(u32, bool)> = f.dynamic.iter().map(|d| (d.scope, d.on_self)).collect();
        assert_eq!(d, vec![(1, true), (2, false), (3, true), (NONE, false)]);
    }

    #[test]
    fn operator_writes_and_counts() {
        let src = "def total\n  self.total ||= 1\n  x.count += 1\n  total\nend\ntotal\n";
        let f = facts(src);
        let m = mentions(&f);
        for want in ["total@total", "total=@total", "count@total", "count=@total", "total@-"] {
            assert!(m.contains(&want.to_string()), "{want} in {m:?}");
        }
        let total = f.names.iter().position(|n| n == "total").unwrap() as u32;
        assert_eq!(f.counts.iter().find(|c| c.0 == total).map(|c| c.1), Some(1), "only the top-level call counts");
    }

    #[test]
    fn one_line_defs_are_not_clean() {
        let f = facts("class A\n  def a; end; def b; end\n  private def c\n  end\n  def d = 1 # ok\nend\n");
        let clean: Vec<bool> = f.defs.iter().map(|d| d.clean).collect();
        assert_eq!(clean, vec![false, false, false, true]);
    }
}
