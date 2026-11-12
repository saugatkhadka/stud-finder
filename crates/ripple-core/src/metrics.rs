//! Per-file and per-method size and complexity, computed by the extractors
//! during the parse they already do (no second parse, no external linter).
//!
//! # What is counted
//!
//! **Code lines** ([`FileMetrics::lines`]): lines with at least one byte that
//! is neither whitespace nor inside a comment, as reported by the parser
//! (prism for Ruby, oxc for JS/TS). ERB/HAML/Slim templates and the
//! non-script parts of `.vue` files count non-blank lines.
//!
//! **Decisions** (cyclomatic-style, one per branch point):
//!
//! - Ruby (aimed at RuboCop `Metrics/CyclomaticComplexity`): `if`, `elsif`,
//!   `unless`, ternary `?:` and their modifier forms; `while`, `until`
//!   (also modifier / `begin..end while`); `for`; each `when` of a `case`;
//!   each `in` of a `case ... in`; a `begin`/`def`/block body with `rescue`
//!   clauses counts **once** however many clauses it has, and `x rescue y`
//!   once; `&&`, `||`, `and`, `or`; `||=` and `&&=` (any target); every `&.`
//!   call; a block (`do`/`{}` or `&blk`/`&:sym`) passed to a known
//!   iterating method (`each`, `map`, `select`, `each_with_object`, ... the
//!   RuboCop list in [`RUBY_ITERATING_METHODS`]). Not counted: `case`
//!   itself, `else`, `ensure`, `loop`, `retry`/`next`/`break`, pattern guards.
//!   Differences from RuboCop: no discount for repeated `&.` on the same
//!   local variable; a `def` nested in another `def` and the body of a
//!   `define_method` block count towards the enclosing method (or top level)
//!   instead of being their own method.
//! - JS/TS (aimed at ESLint `complexity`, default "classic" variant): `if`
//!   (each `else if` is another `if`), `?:`, each `case` with a test (not
//!   `default`), `for`, `for...in`, `for...of`, `while`, `do...while`,
//!   `catch`, `&&`, `||`, `??`, `&&=`, `||=`, `??=`, every optional `?.`
//!   member access or call, and every default value (`function f(a = 1)`,
//!   `const { a = 1 } = o`, `[a = 1] = xs`). Every function (declaration,
//!   expression, arrow, class method, getter/setter) is its own method;
//!   decisions in a class field initializer or static block count towards
//!   the enclosing function or top level (ESLint scores those separately).
//!
//! **Per method**: `1 + decisions` inside it. Ruby methods are the outermost
//! `def`s (any receiver, `class << self`, inside blocks); JS methods are all
//! functions, nested ones scored separately (a decision belongs to the
//! innermost function).
//!
//! **Per file** ([`FileMetrics::complexity`]): the sum of its methods'
//! complexities plus the decisions outside every method (class bodies,
//! top-level code, DSL blocks). A file with no methods and no branches has
//! complexity 0.
//!
//! # Storage
//!
//! Each extractor publishes one [`MetricsTable`] through
//! [`crate::Facts::metrics`]; the engine hands them to the graph
//! ([`crate::Graph::set_metrics`]), shared, not copied. Tables are keyed by a
//! 64-bit hash of the path ([`path_hash`]) rather than the path, so a table
//! costs 20 bytes per file plus 8 per method and holds no strings.

use std::sync::Arc;

/// Size and complexity of one file. 12 bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct FileMetrics {
    /// Code lines (see the module docs).
    pub lines: u32,
    /// Sum of method complexities plus decisions outside methods.
    pub complexity: u32,
    /// Number of methods (Ruby `def`s, JS functions), saturating.
    pub methods: u16,
    /// Highest method complexity in the file (0 without methods), saturating.
    pub max_method: u16,
}

/// One method of a file (Ruby only: the outermost `def`s). 8 bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct MethodMetric {
    /// 1-based line of the `def`.
    pub start: u32,
    /// Lines spanned, `def` to `end` inclusive, minus one (so `end = start + span`).
    pub span: u16,
    /// `1 + decisions`, saturating.
    pub complexity: u16,
}

impl MethodMetric {
    /// 1-based line of the closing `end`, inclusive.
    pub fn end(&self) -> u32 {
        self.start + u32::from(self.span)
    }
}

/// 64-bit FNV-1a of a repo-relative path: the key of a [`MetricsTable`].
pub fn path_hash(path: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Methods RuboCop treats as iterating (`Metrics::Utils::IteratingBlock`):
/// a block or block-pass given to one is a decision. Sorted.
pub const RUBY_ITERATING_METHODS: &[&str] = &[
    "all?", "any?", "bsearch", "bsearch_index", "chain", "chunk", "chunk_while", "collect", "collect!",
    "collect_concat", "combination", "count", "cycle", "delete_if", "detect", "drop", "drop_while", "each",
    "each_cons", "each_entry", "each_index", "each_key", "each_pair", "each_slice", "each_value",
    "each_with_index", "each_with_object", "entries", "fetch", "fetch_values", "filter", "filter_map", "find",
    "find_all", "find_index", "first", "flat_map", "grep", "grep_v", "group_by", "has_key?", "inject",
    "keep_if", "lazy", "map", "map!", "max", "max_by", "merge", "merge!", "min", "min_by", "minmax",
    "minmax_by", "none?", "one?", "partition", "permutation", "product", "reduce", "reject", "reject!",
    "repeat", "repeated_combination", "repeated_permutation", "reverse_each", "select", "select!",
    "slice_after", "slice_before", "slice_when", "sort", "sort!", "sort_by", "sort_by!", "sum", "take",
    "take_while", "tally", "to_h", "transform_keys", "transform_keys!", "transform_values",
    "transform_values!", "uniq", "with_index", "with_object", "zip",
];

/// Is `name` in [`RUBY_ITERATING_METHODS`]?
pub fn ruby_iterating(name: &[u8]) -> bool {
    RUBY_ITERATING_METHODS.binary_search_by(|m| m.as_bytes().cmp(name)).is_ok()
}

/// How many files share one entry of [`MetricsTable::method_base`].
const BASE_EVERY: usize = 32;

/// Metrics of the files one extractor analysed. Immutable once built.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetricsTable {
    /// [`path_hash`] per file, sorted, unique.
    hashes: Box<[u64]>,
    /// Parallel to `hashes`.
    files: Box<[FileMetrics]>,
    /// Per-method metrics, file after file in `hashes` order; file `i` owns
    /// `files[i].methods` of them. Empty when the extractor reports none.
    methods: Box<[MethodMetric]>,
    /// Offset into `methods` of file `k * BASE_EVERY`.
    method_base: Box<[u32]>,
}

impl MetricsTable {
    /// Builds a table from `(path, file metrics, method metrics)`. With
    /// `per_method` false the method lists are ignored (and may be empty).
    /// Method lists longer than `u16::MAX` are cut to the count stored in
    /// [`FileMetrics::methods`]. A path given twice keeps the last entry.
    pub fn build<'a, I>(entries: I, per_method: bool) -> Self
    where
        I: IntoIterator<Item = (&'a str, FileMetrics, &'a [MethodMetric])>,
    {
        let mut rows: Vec<(u64, FileMetrics, &'a [MethodMetric])> =
            entries.into_iter().map(|(p, m, ms)| (path_hash(p), m, ms)).collect();
        // Stable, so the last of equal hashes is the last given.
        rows.sort_by_key(|r| r.0);
        let mut keep: Vec<(u64, FileMetrics, &'a [MethodMetric])> = Vec::with_capacity(rows.len());
        for r in rows {
            match keep.last_mut() {
                Some(last) if last.0 == r.0 => *last = r,
                _ => keep.push(r),
            }
        }
        let mut methods = Vec::new();
        let mut method_base = Vec::new();
        if per_method {
            methods.reserve(keep.iter().map(|r| r.2.len().min(r.1.methods as usize)).sum());
            for (i, r) in keep.iter_mut().enumerate() {
                if i % BASE_EVERY == 0 {
                    method_base.push(methods.len() as u32);
                }
                let n = r.2.len().min(usize::from(u16::MAX));
                r.1.methods = n as u16;
                methods.extend_from_slice(&r.2[..n]);
            }
        }
        Self {
            hashes: keep.iter().map(|r| r.0).collect(),
            files: keep.iter().map(|r| r.1).collect(),
            methods: methods.into(),
            method_base: method_base.into(),
        }
    }

    pub fn len(&self) -> usize {
        self.hashes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }
    /// Whether per-method metrics are stored.
    pub fn has_methods(&self) -> bool {
        !self.method_base.is_empty()
    }

    fn index(&self, path: &str) -> Option<usize> {
        self.hashes.binary_search(&path_hash(path)).ok()
    }

    /// Metrics of `path`, if this table has it.
    pub fn get(&self, path: &str) -> Option<FileMetrics> {
        self.index(path).map(|i| self.files[i])
    }

    /// Methods of `path`, sorted by start line; empty when unknown.
    pub fn methods(&self, path: &str) -> &[MethodMetric] {
        match self.index(path) {
            Some(i) if self.has_methods() => {
                let k = i / BASE_EVERY;
                let start = self.method_base[k] as usize
                    + self.files[k * BASE_EVERY..i].iter().map(|f| usize::from(f.methods)).sum::<usize>();
                &self.methods[start..start + usize::from(self.files[i].methods)]
            }
            _ => &[],
        }
    }

    /// Every file's metrics, in hash order (for distributions and debugging).
    pub fn iter(&self) -> impl Iterator<Item = (u64, FileMetrics)> + '_ {
        self.hashes.iter().copied().zip(self.files.iter().copied())
    }

    /// Heap bytes held by the table.
    pub fn heap_bytes(&self) -> usize {
        self.hashes.len() * 8 + self.files.len() * 12 + self.methods.len() * 8 + self.method_base.len() * 4
    }
}

/// The first table among `tables` that knows `path`.
pub fn lookup<'t>(tables: &'t [Arc<MetricsTable>], path: &str) -> Option<(FileMetrics, &'t [MethodMetric])> {
    tables.iter().find_map(|t| {
        let i = t.index(path)?;
        Some((t.files[i], t.methods(path)))
    })
}

/// Code lines of `src`: lines with a byte that is not ASCII whitespace and
/// not inside one of `comments` (byte ranges `[start, end)`, sorted by start,
/// non-overlapping).
pub fn code_lines(src: &[u8], comments: &[(u32, u32)]) -> u32 {
    let mut code = 0u32;
    let mut has = false;
    let mut i = 0usize;
    let n = src.len();
    for &(cs, ce) in comments {
        let (cs, ce) = ((cs as usize).min(n), (ce as usize).min(n));
        if cs < i {
            continue;
        }
        scan(&src[i..cs], &mut code, &mut has);
        // Inside the comment only line breaks matter.
        for &b in &src[cs..ce] {
            if b == b'\n' {
                code += u32::from(has);
                has = false;
            }
        }
        i = ce.max(i);
    }
    scan(&src[i..], &mut code, &mut has);
    code + u32::from(has)
}

fn scan(s: &[u8], code: &mut u32, has: &mut bool) {
    for &b in s {
        if b == b'\n' {
            *code += u32::from(*has);
            *has = false;
        } else if !b.is_ascii_whitespace() {
            *has = true;
        }
    }
}

/// Non-blank lines of `src`.
pub fn nonblank_lines(src: &[u8]) -> u32 {
    code_lines(src, &[])
}

/// Accumulates decisions per method while a parser's tree is walked.
#[derive(Debug, Default, Clone)]
pub struct Counter {
    /// Decisions of the methods being visited, innermost last.
    stack: Vec<u32>,
    /// Decisions outside every method.
    pub top: u32,
    pub sum: u32,
    pub methods: u32,
    pub max: u32,
}

impl Counter {
    /// One decision in the innermost open method, or at top level.
    pub fn decision(&mut self) {
        match self.stack.last_mut() {
            Some(d) => *d += 1,
            None => self.top += 1,
        }
    }
    pub fn enter(&mut self) {
        self.stack.push(0);
    }
    /// Closes the innermost method; returns its complexity (`1 + decisions`).
    pub fn leave(&mut self) -> u32 {
        let c = 1 + self.stack.pop().unwrap_or(0);
        self.sum += c;
        self.methods += 1;
        self.max = self.max.max(c);
        c
    }
    /// Adds another block's counts (`.vue` files with several scripts).
    pub fn absorb(&mut self, o: &Counter) {
        self.top += o.top;
        self.sum += o.sum;
        self.methods += o.methods;
        self.max = self.max.max(o.max);
    }
    pub fn file(&self, lines: u32) -> FileMetrics {
        FileMetrics { lines, complexity: self.sum + self.top, methods: sat16(self.methods), max_method: sat16(self.max) }
    }
}

pub fn sat16(v: u32) -> u16 {
    u16::try_from(v).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_skip_blank_and_comment_only() {
        let src = "a = 1\n\n  # c\nb /* x */\n/* one\n two */ c\n/* three\n*/\n";
        let span = |s: &str| {
            let i = src.find(s).unwrap() as u32;
            (i, i + s.len() as u32)
        };
        let comments = [span("# c"), span("/* x */"), span("/* one\n two */"), span("/* three\n*/")];
        // `a = 1`, `b`, ` c`; the rest is blank or comment.
        assert_eq!(code_lines(src.as_bytes(), &comments), 3);
        assert_eq!(code_lines(src.as_bytes(), &comments[..2]), 6);
        assert_eq!(nonblank_lines(src.as_bytes()), 7);
        assert_eq!(nonblank_lines(b"x"), 1);
        assert_eq!(nonblank_lines(b""), 0);
    }

    #[test]
    fn table_lookup_and_methods() {
        let m = |s, c| MethodMetric { start: s, span: 1, complexity: c };
        let a = [m(1, 2), m(5, 1)];
        let b: [MethodMetric; 0] = [];
        let c = [m(3, 7)];
        let fm = |n: u16, mx: u16| FileMetrics { lines: 10, complexity: 9, methods: n, max_method: mx };
        let mut rows: Vec<(String, FileMetrics, Vec<MethodMetric>)> =
            vec![("a.rb".into(), fm(2, 2), a.to_vec()), ("b.rb".into(), fm(0, 0), b.to_vec()), ("c.rb".into(), fm(1, 7), c.to_vec())];
        for i in 0..100 {
            rows.push((format!("f{i}.rb"), fm(1, 1), vec![m(i, 1)]));
        }
        let t = MetricsTable::build(rows.iter().map(|(p, f, ms)| (p.as_str(), *f, ms.as_slice())), true);
        assert_eq!(t.len(), 103);
        assert_eq!(t.methods("a.rb"), &a);
        assert_eq!(t.methods("b.rb"), &b);
        assert_eq!(t.methods("c.rb"), &c);
        for i in 0..100 {
            assert_eq!(t.methods(&format!("f{i}.rb")), &[m(i, 1)]);
        }
        assert_eq!(t.get("c.rb").unwrap().max_method, 7);
        assert!(t.get("zzz.rb").is_none() && t.methods("zzz.rb").is_empty());
        let no = MetricsTable::build(rows.iter().map(|(p, f, ms)| (p.as_str(), *f, ms.as_slice())), false);
        assert!(!no.has_methods() && no.methods("a.rb").is_empty() && no.get("a.rb").unwrap().methods == 2);
        assert_eq!(t.heap_bytes(), 103 * 20 + 103 * 8 + 4 * 4);
    }

    #[test]
    fn iterating_list_sorted() {
        assert!(RUBY_ITERATING_METHODS.windows(2).all(|w| w[0] < w[1]));
        assert!(ruby_iterating(b"each") && ruby_iterating(b"map!") && !ruby_iterating(b"tap"));
    }

    #[test]
    fn counter_nesting() {
        let mut c = Counter::default();
        c.decision();
        c.enter();
        c.decision();
        c.enter();
        c.decision();
        c.decision();
        assert_eq!(c.leave(), 3);
        assert_eq!(c.leave(), 2);
        assert_eq!(c.file(4), FileMetrics { lines: 4, complexity: 6, methods: 2, max_method: 3 });
    }
}
