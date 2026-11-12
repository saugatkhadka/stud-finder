use ripple_core::RelPath;
use ripple_core::model::ScopeRange;
use serde::{Deserialize, Serialize};

/// How a file changed relative to the comparison base.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    /// New file (including untracked files in the working tree).
    Added,
    #[default]
    Modified,
    /// The path no longer exists. Its former dependents are what breaks.
    Deleted,
    /// Moved from `old_path` (possibly with edits). Both paths seed the query.
    Renamed,
}

/// One hunk of a diff, new side only, in `git diff -U0` terms: the header
/// `@@ -a,b +start,count @@`.
///
/// * `count > 0`: new lines `start ..= start + count - 1` were added or replaced.
/// * `count == 0`: a pure deletion; the removed lines sat between new lines
///   `start` and `start + 1` (`start == 0`: at the top of the file).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    pub start: u32,
    pub count: u32,
    /// The old side of the hunk removed or rewrote a line that opens a
    /// definition (`def ...`). Such a hunk can merge or delete a method whose
    /// callers are not visible on the new side, so it always means "whole file".
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removes_definition: bool,
}

impl Hunk {
    pub fn new(start: u32, count: u32) -> Self {
        Self { start, count, removes_definition: false }
    }
}

/// One changed file handed to [`crate::evaluate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    /// Repo-relative path (the new path for renames).
    pub path: RelPath,
    #[serde(default)]
    pub status: FileStatus,
    /// Previous path, for renames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<RelPath>,
    /// Added lines, when a diff is available (`None` for binary files or no diff).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<u32>,
    /// First changed line in the new file (1-based), used for CI annotations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_line: Option<u32>,
    /// New-side hunks of the diff, when one is available (`None` for binary
    /// files, files without a textual diff, or no diff at all). Turned into
    /// [`Self::scopes`] by [`scopes_for_hunks`] once the file's scopes are known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hunks: Option<Vec<Hunk>>,
    /// The parts of the file that changed: `None` = the whole file (the
    /// conservative default), `Some(names)` = only these methods / actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
    /// `def` lines the diff added or removed (Ruby), so the query side can
    /// tell a body edit from a signature change, an added method and a
    /// removed or renamed one. Empty when there is no diff.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub def_lines: Vec<DefLine>,
    /// What kind of change this is, filled in by the query side from the
    /// hunks, `def_lines` and the file's scope ranges ([`classify_change`]).
    /// Not part of the wire format.
    #[serde(skip)]
    pub change: Option<Change>,
}

/// A line opening a method definition that the diff added or removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefLine {
    /// On the old side (removed), else on the new side (added).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
    /// New-side line: the added line itself; for a removed line, the
    /// new-side `start` of its hunk (which identifies the hunk).
    pub line: u32,
    /// The source line, trimmed.
    pub text: String,
}

/// How a changed file changed, from the diff hunks and the file's method
/// ranges. One `kind` (the most significant facet), plus every facet found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub kind: ChangeKind,
    /// Methods whose body changed (the `def` line itself did not).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<String>,
    /// Methods whose `def` line changed (arguments, receiver, modifiers).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signatures: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added_methods: Vec<String>,
    /// Methods whose `def` was removed and that the file no longer defines
    /// (removed or renamed; a method moved within the file is not here).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_methods: Vec<String>,
    /// A line outside every method changed (constants, DSL, callbacks, includes).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub class_body: bool,
    /// Why the change is treated as whole-file (`kind: whole_file` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whole_file: Option<WholeFileReason>,
}

/// Ordered by significance: `kind` is the greatest facet present.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// Only lines inside method bodies changed.
    Body,
    MethodAdded,
    /// Lines outside every method changed.
    ClassBody,
    /// A `def` line changed and the method kept its name.
    Signature,
    /// A method was removed or renamed.
    MethodRemoved,
    /// No finer information: see [`WholeFileReason`].
    #[default]
    WholeFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WholeFileReason {
    Added,
    Deleted,
    Renamed,
    /// No textual diff (explicit `--files`, binary, mode change).
    NoDiff,
    /// Not Ruby (JS/TS, templates, other files): no method ranges.
    UnsupportedLanguage,
}

/// The method name a source line defines, if it opens a Ruby method:
/// `def x`, `def self.x`, `def Foo.x`, `def x=(v)`, `def ==(o)`, `def x = 1`,
/// and with leading modifiers (`private def x`, `memoize def x`).
pub fn def_name(line: &str) -> Option<String> {
    let mut rest = line.trim_start();
    for _ in 0..3 {
        if let Some(r) = rest.strip_prefix("def")
            && (r.starts_with([' ', '\t', '(']))
        {
            let r = r.trim_start();
            // A receiver (`self.`, `Foo.`, `Foo::Bar.`) before the name.
            let r = match r.find('.') {
                Some(i) if i > 0 && r[..i].chars().all(|c| c.is_alphanumeric() || c == '_' || c == ':') => &r[i + 1..],
                _ => r,
            };
            let end = r.find(|c: char| c == '(' || c == ';' || c.is_whitespace()).unwrap_or(r.len());
            let name = &r[..end];
            return (!name.is_empty()).then(|| name.to_string());
        }
        // One lowercase modifier word (`private`, `protected`, `memoize`, ...).
        let w = rest.find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')).unwrap_or(rest.len());
        let after = &rest[w..];
        if w == 0 || !after.starts_with([' ', '\t']) {
            return None;
        }
        rest = after.trim_start();
    }
    None
}

/// Every method name `text` (a whole Ruby file) defines, sorted.
pub fn def_names(text: &str) -> Vec<String> {
    let mut v: Vec<String> = text.lines().filter_map(def_name).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Classifies a changed file (see [`Change`]).
///
/// * `ranges`: the new file's method ranges (`None` = none known).
/// * `new_defs`: every method name the new file defines (sorted), when the
///   caller could read it; otherwise the names in `ranges` and the added
///   `def` lines stand in.
pub fn classify_change(f: &ChangedFile, ranges: Option<&[ScopeRange]>, new_defs: Option<&[String]>) -> Change {
    let mut c = Change::default();
    let removed_names: Vec<(String, &str)> = f.def_lines.iter().filter(|d| d.removed).filter_map(|d| Some((def_name(&d.text)?, d.text.as_str()))).collect();
    let added_names: Vec<(String, &str)> = f.def_lines.iter().filter(|d| !d.removed).filter_map(|d| Some((def_name(&d.text)?, d.text.as_str()))).collect();
    let defined = |n: &str| -> bool {
        match new_defs {
            Some(d) => d.binary_search_by(|x| x.as_str().cmp(n)).is_ok(),
            None => added_names.iter().any(|(a, _)| a == n) || ranges.is_some_and(|r| r.iter().any(|s| s.name == n)),
        }
    };
    let push = |v: &mut Vec<String>, n: &str| {
        if !v.iter().any(|x| x == n) {
            v.push(n.to_string());
        }
    };
    // Removed methods are reported whatever the status (a deleted file
    // removes every method it had).
    for (n, _) in &removed_names {
        if f.status == FileStatus::Deleted || !defined(n) {
            push(&mut c.removed_methods, n);
        }
    }
    let finish = |mut c: Change| {
        for v in [&mut c.methods, &mut c.signatures, &mut c.added_methods, &mut c.removed_methods] {
            v.sort_unstable();
            v.dedup();
        }
        c
    };
    let whole = match f.status {
        FileStatus::Added => Some(WholeFileReason::Added),
        FileStatus::Deleted => Some(WholeFileReason::Deleted),
        FileStatus::Renamed => Some(WholeFileReason::Renamed),
        FileStatus::Modified => None,
    };
    if let Some(reason) = whole {
        if f.status == FileStatus::Added {
            for (n, _) in &added_names {
                push(&mut c.added_methods, n);
            }
        }
        c.kind = ChangeKind::WholeFile;
        c.whole_file = Some(reason);
        return finish(c);
    }
    let Some(hunks) = f.hunks.as_deref().filter(|h| !h.is_empty()) else {
        // `--scope` without a diff: the caller says which methods changed.
        if let Some(s) = f.scopes.as_ref().filter(|s| !s.is_empty()) {
            c.kind = ChangeKind::Body;
            c.methods = s.clone();
            return finish(c);
        }
        c.kind = ChangeKind::WholeFile;
        c.whole_file = Some(WholeFileReason::NoDiff);
        return finish(c);
    };
    let ranges = ranges.unwrap_or(&[]);
    if ranges.is_empty() && f.def_lines.is_empty() && crate::lang::Language::of(&f.path) != crate::lang::Language::Ruby {
        c.kind = ChangeKind::WholeFile;
        c.whole_file = Some(WholeFileReason::UnsupportedLanguage);
        return finish(c);
    }
    for h in hunks {
        let (a, b) = (h.start, h.start + h.count.max(1) - 1);
        let added_here = h.count > 0 && f.def_lines.iter().any(|d| !d.removed && (a..=b).contains(&d.line));
        let removed_here = h.removes_definition || f.def_lines.iter().any(|d| d.removed && d.line == h.start);
        let def_line_here = h.count > 0 && ranges.iter().any(|m| (a..=b).contains(&m.start));
        let inside: Vec<&ScopeRange> = hunk_scopes(h, ranges).collect();
        for m in &inside {
            push(&mut c.methods, &m.name);
        }
        if def_line_here && !added_here && !removed_here {
            // A `def` line changed in a way `def_name` did not see (or the
            // diff has no `def_lines`): its method's signature.
            for m in ranges.iter().filter(|m| (a..=b).contains(&m.start)) {
                push(&mut c.signatures, &m.name);
            }
        }
        if inside.is_empty() && !added_here && !removed_here && !def_line_here {
            c.class_body = true;
        }
    }
    for (n, text) in &added_names {
        let before: Vec<&str> = removed_names.iter().filter(|(r, _)| r == n).map(|(_, t)| *t).collect();
        if before.is_empty() {
            push(&mut c.added_methods, n);
        } else if before.contains(text) {
            // The same `def` line removed and added: moved or rewritten.
            push(&mut c.methods, n);
        } else {
            push(&mut c.signatures, n);
        }
    }
    for (n, _) in &removed_names {
        // Removed here but still defined, with no added `def` of that name:
        // e.g. one of two definitions dropped. The definition changed.
        if defined(n) && !added_names.iter().any(|(a, _)| a == n) {
            push(&mut c.signatures, n);
        }
    }
    c.methods.retain(|m| !c.signatures.contains(m) && !c.added_methods.contains(m));
    c.kind = if !c.removed_methods.is_empty() {
        ChangeKind::MethodRemoved
    } else if !c.signatures.is_empty() {
        ChangeKind::Signature
    } else if c.class_body {
        ChangeKind::ClassBody
    } else if !c.added_methods.is_empty() {
        ChangeKind::MethodAdded
    } else {
        ChangeKind::Body
    };
    finish(c)
}

impl ChangedFile {
    pub fn new(path: impl Into<RelPath>, status: FileStatus) -> Self {
        Self { path: path.into(), status, old_path: None, added: None, deleted: None, first_line: None, hunks: None, scopes: None, def_lines: Vec::new(), change: None }
    }
    pub fn modified(path: impl Into<RelPath>) -> Self {
        Self::new(path, FileStatus::Modified)
    }
    pub fn lines(mut self, added: u32, deleted: u32) -> Self {
        self.added = Some(added);
        self.deleted = Some(deleted);
        self
    }
    pub fn renamed_from(mut self, old: impl Into<RelPath>) -> Self {
        self.status = FileStatus::Renamed;
        self.old_path = Some(old.into());
        self
    }
    /// Only these scopes changed (see [`Self::scopes`]).
    pub fn in_scopes<S: Into<String>>(mut self, scopes: impl IntoIterator<Item = S>) -> Self {
        self.scopes = Some(scopes.into_iter().map(Into::into).collect());
        self
    }
}

/// The scope(s) of one hunk: every range that strictly contains it. Empty =
/// the hunk touches something outside every method (or a `def` line itself).
fn hunk_scopes<'a>(h: &Hunk, ranges: &'a [ScopeRange]) -> impl Iterator<Item = &'a ScopeRange> + 'a {
    let h = *h;
    ranges.iter().filter(move |m| {
        if h.count == 0 {
            // Removed lines sat between new lines p and p + 1: both must be in
            // the method and p must not be before its `def` line.
            let p = h.start;
            m.start <= p && p < m.end
        } else {
            // Changing the `def` line itself (rename / signature) breaks
            // callers we cannot see under the new name: not "inside".
            let (a, b) = (h.start, h.start + h.count - 1);
            m.start < a && b <= m.end
        }
    })
}

/// Maps a diff onto the changed file's scopes (methods / controller actions).
/// Conservative: `None` (whole file) unless every hunk lies strictly inside
/// some scope.
///
/// * added, deleted or renamed files, files without hunks (binary, mode-only,
///   no diff) and files without scope ranges are whole-file changes;
/// * a hunk with new lines `[a, b]` belongs to scope `m` iff `m.start < a` and
///   `b <= m.end` (touching the `def` line means whole file);
/// * a pure deletion after new line `p` belongs to `m` iff `m.start <= p` and
///   `p + 1 <= m.end`;
/// * a hunk that removes a `def` line, or belongs to no scope, makes it whole file.
///
/// Otherwise the result is the sorted set of scope names hit.
pub fn scopes_for_hunks(status: FileStatus, hunks: Option<&[Hunk]>, ranges: Option<&[ScopeRange]>) -> Option<Vec<String>> {
    if status != FileStatus::Modified {
        return None;
    }
    let (hunks, ranges) = (hunks?, ranges?);
    if hunks.is_empty() || ranges.is_empty() {
        return None;
    }
    let mut out: Vec<String> = Vec::new();
    for h in hunks {
        if h.removes_definition {
            return None;
        }
        // A `def` line inside the hunk (an inner method's, too) is a rename or
        // signature change: whole file.
        if h.count > 0 && ranges.iter().any(|m| (h.start..h.start + h.count).contains(&m.start)) {
            return None;
        }
        let before = out.len();
        out.extend(hunk_scopes(h, ranges).map(|m| m.name.clone()));
        if out.len() == before {
            return None;
        }
    }
    out.sort_unstable();
    out.dedup();
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(name: &str, start: u32, end: u32) -> ScopeRange {
        ScopeRange { name: name.into(), start, end }
    }

    /// ```text
    ///  1 class Purchase
    ///  2   has_many :refunds
    ///  3
    ///  4   def refund!
    ///  5     a
    ///  6     b
    ///  7   end
    ///  8
    ///  9   def charge!
    /// 10     c
    /// 11   end
    /// 12 end
    /// ```
    fn ranges() -> Vec<ScopeRange> {
        vec![r("refund!", 4, 7), r("charge!", 9, 11)]
    }

    fn map(hunks: &[(u32, u32)]) -> Option<Vec<String>> {
        let hunks: Vec<Hunk> = hunks.iter().map(|&(s, c)| Hunk::new(s, c)).collect();
        scopes_for_hunks(FileStatus::Modified, Some(&hunks), Some(&ranges()))
    }

    fn names(v: &[&str]) -> Option<Vec<String>> {
        Some(v.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn edits_inside_a_method() {
        assert_eq!(map(&[(5, 1)]), names(&["refund!"]));
        assert_eq!(map(&[(5, 2)]), names(&["refund!"]));
        // The closing `end` belongs to the method.
        assert_eq!(map(&[(5, 3)]), names(&["refund!"]));
        assert_eq!(map(&[(7, 1)]), names(&["refund!"]));
        assert_eq!(map(&[(10, 1), (5, 1)]), names(&["charge!", "refund!"]));
        assert_eq!(map(&[(5, 1), (6, 1)]), names(&["refund!"]), "deduplicated");
    }

    #[test]
    fn def_line_or_outside_means_whole_file() {
        assert_eq!(map(&[(4, 1)]), None, "the def line itself (rename / signature)");
        assert_eq!(map(&[(4, 2)]), None, "starts on the def line");
        assert_eq!(map(&[(6, 3)]), None, "runs past the end");
        assert_eq!(map(&[(8, 1)]), None, "between methods");
        assert_eq!(map(&[(2, 1)]), None, "class body");
        assert_eq!(map(&[(5, 1), (2, 1)]), None, "one hunk outside is enough");
        assert_eq!(map(&[(13, 1)]), None, "appended at the end of the file");
        assert_eq!(map(&[(1, 12)]), None);
    }

    #[test]
    fn pure_deletions() {
        // `+p,0`: the removed lines were between new lines p and p+1.
        assert_eq!(map(&[(4, 0)]), names(&["refund!"]), "right after the def line");
        assert_eq!(map(&[(5, 0)]), names(&["refund!"]), "in the middle");
        assert_eq!(map(&[(6, 0)]), names(&["refund!"]), "just before the closing end");
        assert_eq!(map(&[(7, 0)]), None, "right after the closing end: between methods");
        assert_eq!(map(&[(3, 0)]), None, "just before the def line");
        assert_eq!(map(&[(8, 0)]), None, "between methods, just before the next def");
        assert_eq!(map(&[(0, 0)]), None, "top of the file");
        assert_eq!(map(&[(12, 0)]), None, "end of the file");
        assert_eq!(map(&[(10, 0)]), names(&["charge!"]));
    }

    #[test]
    fn removing_a_def_line_is_whole_file() {
        // `def b` merged into `a`: the new side looks like an edit inside `a`.
        let h = [Hunk { start: 5, count: 0, removes_definition: true }];
        assert_eq!(scopes_for_hunks(FileStatus::Modified, Some(&h), Some(&ranges())), None);
    }

    #[test]
    fn nested_ranges_report_every_container() {
        let ranges = vec![r("outer", 1, 10), r("inner", 3, 6)];
        let h = [Hunk::new(4, 1)];
        assert_eq!(scopes_for_hunks(FileStatus::Modified, Some(&h), Some(&ranges)), names(&["inner", "outer"]));
        let h = [Hunk::new(3, 1)];
        assert_eq!(scopes_for_hunks(FileStatus::Modified, Some(&h), Some(&ranges)), None, "inner's def line");
        let h = [Hunk::new(2, 1)];
        assert_eq!(scopes_for_hunks(FileStatus::Modified, Some(&h), Some(&ranges)), names(&["outer"]));
    }

    #[test]
    fn no_information_is_whole_file() {
        let h = [Hunk::new(5, 1)];
        for status in [FileStatus::Added, FileStatus::Deleted, FileStatus::Renamed] {
            assert_eq!(scopes_for_hunks(status, Some(&h), Some(&ranges())), None, "{status:?}");
        }
        assert_eq!(scopes_for_hunks(FileStatus::Modified, None, Some(&ranges())), None, "binary / no diff");
        assert_eq!(scopes_for_hunks(FileStatus::Modified, Some(&[]), Some(&ranges())), None, "no hunks (mode change)");
        assert_eq!(scopes_for_hunks(FileStatus::Modified, Some(&h), None), None, "file has no scopes");
        assert_eq!(scopes_for_hunks(FileStatus::Modified, Some(&h), Some(&[])), None);
    }

    // ---- change classification -------------------------------------------

    fn def(removed: bool, line: u32, text: &str) -> DefLine {
        DefLine { removed, line, text: text.into() }
    }

    fn file(hunks: &[(u32, u32)], defs: Vec<DefLine>) -> ChangedFile {
        let mut f = ChangedFile::modified("app/models/purchase.rb");
        f.hunks = Some(hunks.iter().map(|&(s, c)| Hunk::new(s, c)).collect());
        f.def_lines = defs;
        f
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn def_names_cover_ruby_forms() {
        assert_eq!(def_name("  def refund!(amount)").as_deref(), Some("refund!"));
        assert_eq!(def_name("def self.find_by_token(t)").as_deref(), Some("find_by_token"));
        assert_eq!(def_name("def Purchase.build").as_deref(), Some("build"));
        assert_eq!(def_name("  private def helper").as_deref(), Some("helper"));
        assert_eq!(def_name("protected def a = 1").as_deref(), Some("a"));
        assert_eq!(def_name("def total=(v)").as_deref(), Some("total="));
        assert_eq!(def_name("def ==(o)").as_deref(), Some("=="));
        assert_eq!(def_name("define_method(:x)"), None);
        assert_eq!(def_name("# def commented"), None);
        assert_eq!(def_name("undef x"), None);
        assert_eq!(def_names("def b\nend\ndef a\nend\ndef self.a; end\n"), strs(&["a", "b"]));
    }

    #[test]
    fn body_change() {
        let c = classify_change(&file(&[(5, 1), (10, 1)], vec![]), Some(&ranges()), None);
        assert_eq!((c.kind, c.methods.clone(), c.class_body), (ChangeKind::Body, strs(&["charge!", "refund!"]), false));
        assert!(c.removed_methods.is_empty() && c.signatures.is_empty() && c.whole_file.is_none());
    }

    #[test]
    fn class_body_change() {
        let c = classify_change(&file(&[(2, 1), (5, 1)], vec![]), Some(&ranges()), None);
        assert_eq!((c.kind, c.methods.clone(), c.class_body), (ChangeKind::ClassBody, strs(&["refund!"]), true));
    }

    #[test]
    fn signature_change() {
        let f = file(&[(4, 1)], vec![def(true, 4, "def refund!(amount)"), def(false, 4, "def refund!(amount, reason)")]);
        let c = classify_change(&f, Some(&ranges()), Some(&strs(&["charge!", "refund!"])));
        assert_eq!((c.kind, c.signatures.clone()), (ChangeKind::Signature, strs(&["refund!"])));
        assert!(c.removed_methods.is_empty() && c.methods.is_empty());
        // A def line changed without def_lines (e.g. `--files` with ranges only) is a signature too.
        let c = classify_change(&file(&[(4, 1)], vec![]), Some(&ranges()), None);
        assert_eq!((c.kind, c.signatures.clone()), (ChangeKind::Signature, strs(&["refund!"])));
    }

    #[test]
    fn rename_is_a_removed_method() {
        let mut f = file(&[(4, 1)], vec![def(true, 4, "def refund!"), def(false, 4, "def refund_all!")]);
        f.hunks.as_mut().unwrap()[0].removes_definition = true;
        let new_ranges = vec![r("refund_all!", 4, 7), r("charge!", 9, 11)];
        let c = classify_change(&f, Some(&new_ranges), Some(&strs(&["charge!", "refund_all!"])));
        assert_eq!(c.kind, ChangeKind::MethodRemoved);
        assert_eq!((c.removed_methods.clone(), c.added_methods.clone()), (strs(&["refund!"]), strs(&["refund_all!"])));
        // Without the new file's text the added def and the ranges stand in.
        let c = classify_change(&f, Some(&new_ranges), None);
        assert_eq!(c.removed_methods, strs(&["refund!"]));
    }

    #[test]
    fn moved_within_the_file_is_not_removed() {
        // refund! (old lines 4-7) deleted, then re-added verbatim after charge!.
        let f = file(&[(3, 0), (9, 4)], vec![def(true, 3, "def refund!"), def(false, 9, "def refund!")]);
        let new_ranges = vec![r("charge!", 4, 6), r("refund!", 9, 12)];
        let c = classify_change(&f, Some(&new_ranges), Some(&strs(&["charge!", "refund!"])));
        assert_eq!((c.kind, c.methods.clone()), (ChangeKind::Body, strs(&["refund!"])));
        assert!(c.removed_methods.is_empty() && c.added_methods.is_empty() && !c.class_body);
    }

    #[test]
    fn class_methods_and_private_methods() {
        // `def self.lookup` removed, `private def helper` removed; both gone from the file.
        let f = file(&[(8, 0), (3, 0)], vec![def(true, 3, "def self.lookup(id)"), def(true, 8, "private def helper")]);
        let c = classify_change(&f, Some(&ranges()), Some(&strs(&["charge!", "refund!"])));
        assert_eq!((c.kind, c.removed_methods.clone()), (ChangeKind::MethodRemoved, strs(&["helper", "lookup"])));
        // An added private method.
        let f = file(&[(12, 3)], vec![def(false, 12, "private def audit")]);
        let c = classify_change(&f, Some(&ranges()), Some(&strs(&["audit", "charge!", "refund!"])));
        assert_eq!((c.kind, c.added_methods.clone()), (ChangeKind::MethodAdded, strs(&["audit"])));
    }

    #[test]
    fn whole_file_changes() {
        let mut f = file(&[], vec![def(true, 0, "def refund!"), def(true, 0, "def self.lookup")]);
        f.status = FileStatus::Deleted;
        f.hunks = None;
        let c = classify_change(&f, None, None);
        assert_eq!((c.kind, c.whole_file, c.removed_methods.clone()), (ChangeKind::WholeFile, Some(WholeFileReason::Deleted), strs(&["lookup", "refund!"])));

        let mut f = file(&[(1, 5)], vec![def(false, 2, "def a")]);
        f.status = FileStatus::Added;
        let c = classify_change(&f, None, None);
        assert_eq!((c.kind, c.whole_file, c.added_methods.clone()), (ChangeKind::WholeFile, Some(WholeFileReason::Added), strs(&["a"])));

        let mut f = file(&[(5, 1)], vec![]);
        f.path = "app/javascript/a.tsx".into();
        assert_eq!(classify_change(&f, None, None).whole_file, Some(WholeFileReason::UnsupportedLanguage));

        let mut f = file(&[], vec![]);
        f.hunks = None;
        assert_eq!(classify_change(&f, Some(&ranges()), None).whole_file, Some(WholeFileReason::NoDiff));
        // `--scope` without a diff: the caller's scopes are the changed bodies.
        let f = f.in_scopes(["charge!"]);
        let c = classify_change(&f, Some(&ranges()), None);
        assert_eq!((c.kind, c.methods.clone()), (ChangeKind::Body, strs(&["charge!"])));
    }
}
