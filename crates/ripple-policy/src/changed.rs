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
}

impl ChangedFile {
    pub fn new(path: impl Into<RelPath>, status: FileStatus) -> Self {
        Self { path: path.into(), status, old_path: None, added: None, deleted: None, first_line: None, hunks: None, scopes: None }
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
}
