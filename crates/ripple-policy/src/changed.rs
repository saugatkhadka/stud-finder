use ripple_core::RelPath;
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
}

impl ChangedFile {
    pub fn new(path: impl Into<RelPath>, status: FileStatus) -> Self {
        Self { path: path.into(), status, old_path: None, added: None, deleted: None, first_line: None }
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
}
