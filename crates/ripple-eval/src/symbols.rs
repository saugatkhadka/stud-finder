//! Text-level Ruby symbol facts for the `breaks` oracle: which methods and
//! constants a file defines, which of them a commit removed, and whether a
//! line of code mentions a name. Line-based and deliberately independent of
//! ripple's own extractors, so the oracle does not inherit their blind spots.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    /// Public instance method outside a controller.
    Method,
    /// Instance method after `private` / `protected`.
    PrivateMethod,
    /// `def self.x` / `class << self`.
    ClassMethod,
    /// Public instance method of a controller.
    Action,
    /// `class X` / `module X` / `X = Struct.new`.
    Constant,
}

impl SymbolKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SymbolKind::Method => "method",
            SymbolKind::PrivateMethod => "private_method",
            SymbolKind::ClassMethod => "class_method",
            SymbolKind::Action => "action",
            SymbolKind::Constant => "constant",
        }
    }
    pub const ALL: [SymbolKind; 5] = [SymbolKind::Method, SymbolKind::PrivateMethod, SymbolKind::ClassMethod, SymbolKind::Action, SymbolKind::Constant];
}

/// One `def` of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Def {
    pub name: String,
    pub singleton: bool,
    pub private: bool,
    /// 1-based.
    pub line: u32,
}

/// A symbol a commit removed from a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    pub name: String,
    pub kind: SymbolKind,
    /// Line of the definition in the parent's blob (1-based).
    pub line: u32,
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Strips leading visibility / decorator keywords (`private def x`).
fn strip_modifiers(mut s: &str) -> &str {
    const MODS: [&str; 7] = ["private_class_method", "public_class_method", "module_function", "protected", "private", "public", "memoize"];
    loop {
        let before = s;
        for m in MODS {
            if let Some(rest) = s.strip_prefix(m)
                && rest.starts_with([' ', '\t'])
            {
                s = rest.trim_start();
            }
        }
        if s == before {
            return s;
        }
    }
}

/// Name of the method defined by a `def` line (after `def `), with whether it is a singleton.
fn def_name(rest: &str) -> Option<(String, bool)> {
    let rest = rest.trim_start();
    let (rest, singleton) = match rest.find('.') {
        // `def self.x`, `def Foo.x`: a receiver made of identifier characters.
        Some(i) if i > 0 && rest[..i].bytes().all(is_ident) && !rest[..i].is_empty() => (&rest[i + 1..], true),
        _ => (rest, false),
    };
    let b = rest.as_bytes();
    if b.is_empty() {
        return None;
    }
    if b[0].is_ascii_alphabetic() || b[0] == b'_' {
        let mut i = 0;
        while i < b.len() && is_ident(b[i]) {
            i += 1;
        }
        // `name?` / `name!` (not `name!=`), setters `name=(` / `name= v` (not `name ==`).
        if i < b.len() && (b[i] == b'?' || b[i] == b'!') && b.get(i + 1) != Some(&b'=') {
            i += 1;
        } else if i < b.len() && b[i] == b'=' && !matches!(b.get(i + 1), Some(b'=' | b'~' | b'>')) {
            i += 1;
        }
        return Some((rest[..i].to_string(), singleton));
    }
    // Operator: everything up to `(`, whitespace or end.
    let end = rest.find(|c: char| c == '(' || c.is_whitespace()).unwrap_or(rest.len());
    (end > 0).then(|| (rest[..end].to_string(), singleton))
}

pub fn is_operator(name: &str) -> bool {
    !name.as_bytes().first().is_some_and(|&b| b.is_ascii_alphabetic() || b == b'_')
}

pub fn is_setter(name: &str) -> bool {
    name.ends_with('=') && !is_operator(name)
}

/// Every `def` of a Ruby source, in order. Tracks `private`/`protected`
/// sections (reset by `public` and by a new `class`/`module`) and
/// `class << self` blocks by indentation.
pub fn ruby_defs(src: &str) -> Vec<Def> {
    let mut out = Vec::new();
    let mut private = false;
    let mut eigen_indent: Option<usize> = None;
    let mut in_doc = false;
    for (i, raw) in src.lines().enumerate() {
        if raw.starts_with("=begin") {
            in_doc = true;
            continue;
        }
        if in_doc {
            in_doc = !raw.starts_with("=end");
            continue;
        }
        let indent = raw.len() - raw.trim_start().len();
        let t = raw.trim();
        if t.starts_with('#') || t.is_empty() {
            continue;
        }
        if let Some(ei) = eigen_indent
            && indent <= ei
            && t == "end"
        {
            eigen_indent = None;
            continue;
        }
        if t.starts_with("class << self") {
            eigen_indent = Some(indent);
            continue;
        }
        if t.starts_with("class ") || t.starts_with("module ") {
            private = false;
            continue;
        }
        match t {
            "private" | "protected" => {
                private = true;
                continue;
            }
            "public" => {
                private = false;
                continue;
            }
            _ => {}
        }
        let explicit_private = t.starts_with("private ") || t.starts_with("protected ");
        let s = strip_modifiers(t);
        let Some(rest) = s.strip_prefix("def") else { continue };
        if !rest.starts_with([' ', '\t']) {
            continue;
        }
        if let Some((name, singleton)) = def_name(rest) {
            let singleton = singleton || eigen_indent.is_some();
            out.push(Def { name, singleton, private: !singleton && (private || explicit_private), line: i as u32 + 1 });
        }
    }
    out
}

/// Constants a Ruby source defines (`class A::B` → `B`), with their line.
pub fn ruby_constants(src: &str) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    for (i, raw) in src.lines().enumerate() {
        let t = raw.trim_start();
        let rest = t.strip_prefix("class ").or_else(|| t.strip_prefix("module "));
        let token: &str = match rest {
            Some(r) => {
                let r = r.trim_start();
                if r.starts_with("<<") {
                    continue;
                }
                let end = r.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == ':')).unwrap_or(r.len());
                &r[..end]
            }
            None => {
                // `Foo = Struct.new(...)`, `Foo = Class.new`, `Foo = Data.define`.
                let end = t.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(t.len());
                let (name, after) = t.split_at(end);
                let after = after.trim_start();
                let Some(v) = after.strip_prefix('=') else { continue };
                if v.starts_with('=') {
                    continue;
                }
                let v = v.trim_start();
                if !["Struct.new", "Class.new", "Module.new", "Data.define"].iter().any(|p| v.starts_with(p)) {
                    continue;
                }
                name
            }
        };
        let last = token.rsplit("::").next().unwrap_or("");
        if last.as_bytes().first().is_some_and(u8::is_ascii_uppercase) {
            out.push((last.to_string(), i as u32 + 1));
        }
    }
    out
}

/// Does `src` define method `name` by a macro rather than `def`
/// (`attr_reader :name`, `delegate :name`, `alias_method :name`, `scope :name`, associations)?
pub fn macro_defines(src: &str, name: &str) -> bool {
    const MACROS: [&str; 16] = [
        "attr_reader", "attr_writer", "attr_accessor", "delegate", "alias_method", "alias", "define_method", "scope", "has_many", "has_one", "belongs_to",
        "has_and_belongs_to_many", "attribute", "store_accessor", "def_delegators", "def_delegator",
    ];
    let base = name.trim_end_matches('=');
    src.lines().any(|l| {
        let t = l.trim_start();
        let Some(m) = MACROS.iter().find(|m| t.starts_with(**m) && t.as_bytes().get(m.len()).is_some_and(|&b| b == b' ' || b == b'(')) else {
            return false;
        };
        let rest = &t[m.len()..];
        if *m == "alias" {
            return rest.split_whitespace().next() == Some(base);
        }
        // Only as a symbol or string argument (`:name`, `"name"`), not inside a lambda body.
        let b = rest.as_bytes();
        mention_positions(rest, base, false).any(|i| i > 0 && matches!(b[i - 1], b':' | b'"' | b'\''))
    })
}

/// Methods and constants defined in `before` (the parent's blob) and gone
/// from `after` (the commit's blob; `None` = file deleted). A name still
/// defined anywhere in `after` (moved within the file, visibility changed,
/// `def self.x` ↔ `class << self`) is not removed. `controller`: public
/// instance methods are actions.
pub fn removed(before: &str, after: Option<&str>, controller: bool) -> Vec<Removed> {
    let after_src = after.unwrap_or("");
    let after_defs: std::collections::HashSet<String> = ruby_defs(after_src).into_iter().map(|d| d.name).collect();
    let after_consts: std::collections::HashSet<String> = ruby_constants(after_src).into_iter().map(|c| c.0).collect();
    let mut out: Vec<Removed> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for d in ruby_defs(before) {
        if after_defs.contains(&d.name) || !seen.insert(d.name.clone()) || macro_defines(after_src, &d.name) {
            continue;
        }
        let kind = if d.singleton {
            SymbolKind::ClassMethod
        } else if d.private {
            SymbolKind::PrivateMethod
        } else if controller {
            SymbolKind::Action
        } else {
            SymbolKind::Method
        };
        out.push(Removed { name: d.name, kind, line: d.line });
    }
    let mut seen = std::collections::HashSet::new();
    for (c, line) in ruby_constants(before) {
        if !after_consts.contains(&c) && seen.insert(c.clone()) {
            out.push(Removed { name: c, kind: SymbolKind::Constant, line });
        }
    }
    out.sort_by(|a, b| a.line.cmp(&b.line).then(a.name.cmp(&b.name)));
    out
}

/// Is the line a comment (Ruby, JS, ERB)?
pub fn is_comment(line: &str) -> bool {
    let t = line.trim_start();
    (t.starts_with('#') && !t.starts_with("#{")) || t.starts_with("//") || t.starts_with("/*") || t.starts_with("* ") || t == "*" || t.starts_with("<%#")
}

/// Byte offsets where `name` occurs as an identifier (not part of a longer
/// one, not `@name` / `$name`). `constant`: a constant name.
fn mention_positions<'a>(line: &'a str, name: &'a str, constant: bool) -> impl Iterator<Item = usize> + 'a {
    let b = line.as_bytes();
    let n = name.len();
    let ends_punct = name.ends_with(['?', '!']);
    line.match_indices(name).map(|(i, _)| i).filter(move |&i| {
        if i > 0 && (is_ident(b[i - 1]) || (!constant && matches!(b[i - 1], b'@' | b'$'))) {
            return false;
        }
        let q = b.get(i + n).copied();
        match q {
            Some(c) if is_ident(c) => false,
            Some(b'?' | b'!') if !constant && !ends_punct => b.get(i + n + 1) == Some(&b'='),
            _ => true,
        }
    })
}

/// How many times `line` mentions the symbol: a method name called or
/// passed as a symbol (`.name`, `name(`, `:name`, `&:name`, bare `name`), or
/// a constant name. Hash keys (`name: x`) and plain assignments (`name = x`)
/// do not count for a getter; a setter `name=` counts as `.name = x` or `:name=`.
pub fn mention_count(line: &str, name: &str, kind: SymbolKind) -> usize {
    if is_comment(line) {
        return 0;
    }
    if kind == SymbolKind::Constant {
        return mention_positions(line, name, true).count();
    }
    let setter = is_setter(name);
    let base = name.trim_end_matches('=');
    let b = line.as_bytes();
    mention_positions(line, base, false)
        .filter(|&i| {
            let after = &line[i + base.len()..];
            let at = after.trim_start();
            let assign = at.starts_with('=') && !matches!(at.as_bytes().get(1), Some(b'=' | b'~' | b'>'));
            // `name:` (hash key / keyword argument), but not `name::X`.
            let key = after.starts_with(':') && !after.starts_with("::");
            if setter {
                return assign && (i == 0 || b[i - 1] == b'.' || b[i - 1] == b':');
            }
            !assign && !key
        })
        .count()
}

/// [`mention_count`], except that a line defining the name (`def name`,
/// `class Name`) is a definition, not a reference: when the same name is
/// removed from several files at once, those files are not each other's dependents.
pub fn reference_count(line: &str, name: &str, kind: SymbolKind) -> usize {
    let defines = if kind == SymbolKind::Constant { ruby_constants(line).iter().any(|c| c.0 == name) } else { ruby_defs(line).iter().any(|d| d.name == name) };
    if defines { 0 } else { mention_count(line, name, kind) }
}

/// Constant names too generic to match by name (nested in many classes).
pub const CONSTANT_STOPLIST: [&str; 12] = ["ClassMethods", "InstanceMethods", "Base", "Error", "Errors", "Helpers", "Config", "Configuration", "Types", "Constants", "Version", "Concern"];

/// Ultra-generic method names: matching them by name says nothing.
pub const STOPLIST: [&str; 40] = [
    "initialize", "call", "perform", "to_s", "to_h", "to_a", "to_json", "as_json", "to_param", "inspect", "id", "name", "new", "create", "update", "destroy", "save", "show", "index",
    "edit", "all", "find", "each", "map", "run", "execute", "process", "value", "data", "type", "key", "attributes", "params", "render", "valid?", "present?", "blank?", "url", "path", "hash",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[Removed]) -> Vec<(String, &'static str)> {
        v.iter().map(|r| (r.name.clone(), r.kind.as_str())).collect()
    }

    #[test]
    fn defs_with_visibility_singletons_and_punctuation() {
        let src = "class A < B\n  def self.build(x)\n  end\n  def ok?\n  end\n  def save!\n  end\n  def title=(v)\n  end\n  def ==(o)\n  end\n  def [](k)\n  end\n  def short = 1\n  class << self\n    def eigen\n    end\n  end\n  def after_eigen; end\n  private\n  def secret\n  end\n  public\n  def open; end\n  private def inline_private; end\nend\n";
        let d: Vec<(String, bool, bool)> = ruby_defs(src).into_iter().map(|d| (d.name, d.singleton, d.private)).collect();
        let want = [
            ("build", true, false),
            ("ok?", false, false),
            ("save!", false, false),
            ("title=", false, false),
            ("==", false, false),
            ("[]", false, false),
            ("short", false, false),
            ("eigen", true, false),
            ("after_eigen", false, false),
            ("secret", false, true),
            ("open", false, false),
            ("inline_private", false, true),
        ];
        assert_eq!(d, want.map(|(a, b, c)| (a.to_string(), b, c)));
        assert!(is_operator("==") && is_operator("[]") && !is_operator("title="));
        assert!(is_setter("title=") && !is_setter("=="));
    }

    #[test]
    fn rename_is_a_removal_move_and_visibility_are_not() {
        let before = "class A\n  def charge\n  end\n  def refund\n  end\n  def helper\n  end\nend\n";
        // charge renamed to charge!, refund moved below helper, helper made private.
        let after = "class A\n  def charge!\n  end\n  private\n  def helper\n  end\n  def refund\n  end\nend\n";
        assert_eq!(names(&removed(before, Some(after), false)), [("charge".to_string(), "method")]);
        // Same, in a controller: public methods are actions.
        assert_eq!(names(&removed(before, Some(after), true)), [("charge".to_string(), "action")]);
    }

    #[test]
    fn singleton_setter_predicate_bang() {
        let before = "module M\n  def self.lookup(x); end\n  def enabled?; end\n  def reset!; end\n  def title=(v); end\n  def title; end\nend\n";
        let after = "module M\n  def title; end\nend\n";
        let got = names(&removed(before, Some(after), false));
        assert_eq!(got, [("lookup", "class_method"), ("enabled?", "method"), ("reset!", "method"), ("title=", "method")].map(|(a, b)| (a.to_string(), b)));
        // `def self.x` → `class << self; def x` is not a removal.
        assert!(removed("class A\n  def self.x; end\nend\n", Some("class A\n  class << self\n    def x; end\n  end\nend\n"), false).is_empty());
        // A def replaced by attr_reader / delegate is not a removal.
        assert!(removed("class A\n  def total\n  end\nend\n", Some("class A\n  attr_reader :total\nend\n"), false).is_empty());
        assert!(removed("class A\n  def user\n  end\nend\n", Some("class A\n  delegate :user, to: :order\nend\n"), false).is_empty());
    }

    #[test]
    fn deleted_files_and_constants() {
        let before = "module Billing\n  class Charge < Base\n    Result = Struct.new(:ok)\n    def run; end\n  end\nend\n";
        let got = names(&removed(before, None, false));
        assert_eq!(got, [("Billing", "constant"), ("Charge", "constant"), ("Result", "constant"), ("run", "method")].map(|(a, b)| (a.to_string(), b)));
        // Renamed class in a kept file; nested `class A::B` keeps the last segment.
        let got = names(&removed("class Foo::OldName\nend\n", Some("class Foo::NewName\nend\n"), false));
        assert_eq!(got, [("OldName".to_string(), "constant")]);
        assert!(ruby_constants("class << self\nend\n").is_empty());
    }

    #[test]
    fn mentions() {
        use SymbolKind::*;
        let m = |l: &str, n: &str| mention_count(l, n, Method);
        assert_eq!(m("  order.charge(amount)", "charge"), 1);
        assert_eq!(m("  before_save :charge", "charge"), 1);
        assert_eq!(m("  items.map(&:charge)", "charge"), 1);
        assert_eq!(m("  charge", "charge"), 1);
        assert_eq!(m("  recharge; charges; charge_now; @charge", "charge"), 0);
        assert_eq!(m("  x.charge? || x.charge!", "charge"), 0, "different methods");
        assert_eq!(m("  x.charge? || x.charge!", "charge?"), 1);
        assert_eq!(m("  x.charge!=y", "charge"), 1);
        assert_eq!(m("  foo(charge: 1)", "charge"), 0, "hash key");
        assert_eq!(m("  charge = 1", "charge"), 0, "local assignment");
        assert_eq!(m("  charge == 1", "charge"), 1);
        assert_eq!(m("  # calls charge", "charge"), 0, "comment");
        assert_eq!(m("  x.title = 1", "title="), 1);
        assert_eq!(m("  x.title", "title="), 0);
        assert_eq!(m("  send(:title=, 1)", "title="), 1);
        assert_eq!(m("  x.title = 1", "title"), 0);
        assert_eq!(mention_count("  Billing::Charge.new", "Charge", Constant), 1);
        assert_eq!(mention_count("  ChargeRefund.new; Charges", "Charge", Constant), 0);
        assert_eq!(m("<%= link_to x.charge %>", "charge"), 1);
        assert!(STOPLIST.contains(&"call"));
        assert_eq!(reference_count("  def charge(x)", "charge", Method), 0);
        assert_eq!(reference_count("  def self.charge; charge2; end", "charge", Method), 0);
        assert_eq!(reference_count("  module ClassMethods", "ClassMethods", Constant), 0);
        assert_eq!(reference_count("  extend ClassMethods", "ClassMethods", Constant), 1);
    }
}
