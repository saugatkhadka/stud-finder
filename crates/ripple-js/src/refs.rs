//! Per-file parse results: what a file references, independent of where those
//! references resolve to. This is what the incremental cache stores, keyed by
//! content hash, so it must not depend on the file set or on resolver config.

use ripple_core::JsCallSite;

/// One module reference found in a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    /// 1-based line of the reference in the file on disk.
    pub line: u32,
    pub kind: RefKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefKind {
    /// A single specifier: `import`, `export … from`, `require()`, `import()`.
    Import {
        specifier: String,
        /// `import()` / `React.lazy` rather than a static import.
        dynamic: bool,
        /// Only types flow through it (`import type`, `import("x").T` in a type position).
        type_only: bool,
    },
    /// `import.meta.glob(patterns)` or a template-literal `import(\`./x/${y}.js\`)`
    /// (rewritten to `./x/*.js`): expands against the file set.
    Glob { patterns: Vec<String> },
    /// `require.context(dir, recursive, filter)`.
    Context { dir: String, recursive: bool, filter: Option<String> },
}

/// Everything extracted from one file.
#[derive(Debug, Clone, Default)]
pub struct Parsed {
    pub hash: u64,
    pub refs: Vec<Ref>,
    pub calls: Vec<JsCallSite>,
    /// Parallel to `calls`: the context of a call site whose URL use is
    /// neither clearly navigation nor data (`None` = classified).
    pub unclassified: Vec<Option<String>>,
    /// First syntax error, if any. The refs/calls above are still best-effort.
    pub error: Option<String>,
    /// The file could not be read at all (then `error` holds the I/O message).
    pub unreadable: bool,
}
