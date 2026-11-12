//! Per-file method-name facts behind the hop-1 mention filter.
//!
//! The graph is file-level. When a seed file changed only in some methods
//! `M` (from diff hunks or `--scope`), [`crate::Graph::impact_scoped`] keeps a
//! direct dependent reached through a Ruby reference edge only if that
//! dependent *mentions* a name in `M` (calls it with or without a receiver,
//! passes it as a symbol, ...). Matching is by name: Ruby has no static types.
//!
//! Names are identified by [`name_id`], a 32-bit hash, so per-file facts are
//! self-contained (no workspace-wide interner) and can be cached per file. A
//! collision can only make a file look as if it mentions a name it does not,
//! which keeps an extra dependent: the filter stays conservative.

use crate::model::RelPath;
use std::sync::Arc;

/// "Outside every method": the class body / top level of the file.
pub const NO_DEF: u32 = u32::MAX;

/// The id of a method name (FNV-1a, 32 bit).
pub fn name_id(name: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in name.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// What one file defines and mentions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileMentions {
    /// Distinct `(name, def)` pairs, sorted: every method name the file
    /// mentions, with the index into [`Self::defs`] of the outermost method
    /// it is mentioned in, or [`NO_DEF`] for the class body / top level
    /// (callbacks, validations, `delegate`, DSL symbols, lambdas, ...).
    pub pairs: Box<[(u32, u32)]>,
    /// Name id of each outermost `def`, in source order.
    pub defs: Box<[u32]>,
    /// Indices into `defs` of methods invoked implicitly (a fixed list of
    /// hooks and conversions, operators, setters): a change that reaches one
    /// is a change to the whole file. Sorted.
    pub implicit: Box<[u32]>,
    /// Indices into `defs` (or [`NO_DEF`]) containing a dispatch on `self`
    /// with a computed name (`send(x)`, `method_missing`): they may call any
    /// method of the object. Sorted.
    pub self_dynamic: Box<[u32]>,
    /// A dispatch with a computed name on another receiver (or, in a
    /// template, any computed dispatch): the mentions do not account for
    /// everything the file uses, so it always passes the filter.
    pub opaque: bool,
    /// A controller: its methods are actions called by the router, so the
    /// workspace-wide "never mentioned" rule does not apply to them.
    pub controller: bool,
}

impl FileMentions {
    /// Does the file mention any of `names` (sorted)?
    pub fn mentions_any(&self, names: &[u32]) -> bool {
        let p = &self.pairs;
        names.iter().any(|&n| {
            let i = p.partition_point(|&(x, _)| x < n);
            i < p.len() && p[i].0 == n
        })
    }
}

/// All files' [`FileMentions`], as handed from an extractor to the graph
/// through [`crate::Facts::mentions`].
#[derive(Debug, Clone, Default)]
pub struct MentionTable {
    /// Per file, sorted by path. A file absent here has unknown mentions
    /// (it always passes the filter, and as a seed it is wholly changed).
    pub files: Vec<(RelPath, Arc<FileMentions>)>,
    /// Names of methods that nothing in the workspace mentions outside their
    /// own definition (called by convention: GraphQL, Pundit, ...): a change
    /// that reaches one (outside a controller) is a change to the whole file.
    /// Sorted.
    pub unmentioned: Arc<[u32]>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_lookup() {
        assert_eq!(name_id(""), 0x811c_9dc5);
        assert_ne!(name_id("refund!"), name_id("refund?"));
        let mut pairs = vec![(name_id("a"), 0), (name_id("b"), NO_DEF)];
        pairs.sort_unstable();
        let f = FileMentions { pairs: pairs.into(), ..Default::default() };
        let mut q = vec![name_id("z"), name_id("b")];
        q.sort_unstable();
        assert!(f.mentions_any(&q));
        assert!(!f.mentions_any(&[name_id("z")]));
    }
}
