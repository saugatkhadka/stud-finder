//! A read-only virtual file system over `ws.files`, handed to `oxc_resolver`.
//!
//! Resolution must agree with the engine's view of the repo: a file the engine
//! ignores (`dist/**`, `node_modules/**`, ...) must not be resolvable even though
//! it exists on disk, and resolution must not depend on directory walks we did
//! not do. So the resolver never touches the real directory tree; `metadata`
//! is answered from the sorted file set and only file *contents* (package.json)
//! are read from disk.

use oxc_resolver::{FileMetadata, FileSystem, ResolveError};
use std::collections::BTreeSet;
use std::io;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// The set of repo files, shared between the extractor (which mutates it on
/// `update`) and the resolver's file system (which reads it).
#[derive(Debug, Default)]
pub struct FileSet {
    files: BTreeSet<String>,
}

pub type SharedFileSet = Arc<RwLock<FileSet>>;

impl FileSet {
    pub fn from_files<'a>(files: impl IntoIterator<Item = &'a String>) -> Self {
        Self { files: files.into_iter().cloned().collect() }
    }

    pub fn contains(&self, rel: &str) -> bool {
        self.files.contains(rel)
    }

    pub fn insert(&mut self, rel: &str) -> bool {
        self.files.insert(rel.to_string())
    }

    pub fn remove(&mut self, rel: &str) -> bool {
        self.files.remove(rel)
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &String> {
        self.files.iter()
    }

    /// True when at least one file lives under directory `rel` (`""` is the root).
    pub fn is_dir(&self, rel: &str) -> bool {
        if rel.is_empty() {
            return !self.files.is_empty();
        }
        let prefix = format!("{rel}/");
        self.files.range::<str, _>((Bound::Included(prefix.as_str()), Bound::Unbounded)).next().is_some_and(|f| f.starts_with(&prefix))
    }

    /// All files under directory `dir` (`""` = whole repo), in sorted order.
    pub fn under<'a>(&'a self, dir: &str) -> impl Iterator<Item = &'a String> + 'a {
        let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
        self.files.range::<str, _>((Bound::Included(prefix.as_str()), Bound::Unbounded)).take_while(move |f| f.starts_with(&prefix))
    }
}

/// `FileSystem` implementation backed by a [`SharedFileSet`].
#[derive(Clone)]
pub struct VirtualFs {
    root: PathBuf,
    set: SharedFileSet,
}

impl VirtualFs {
    pub fn new(root: PathBuf, set: SharedFileSet) -> Self {
        Self { root, set }
    }

    fn rel<'p>(&self, path: &'p Path) -> Option<&'p str> {
        path.strip_prefix(&self.root).ok()?.to_str()
    }

    fn not_found() -> io::Error {
        io::Error::new(io::ErrorKind::NotFound, "not in ripple file set")
    }
}

impl FileSystem for VirtualFs {
    /// Required by the trait; the extractor always constructs via [`VirtualFs::new`].
    fn new() -> Self {
        Self { root: PathBuf::new(), set: SharedFileSet::default() }
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let rel = self.rel(path).ok_or_else(Self::not_found)?;
        if !self.set.read().map_err(|_| Self::not_found())?.contains(rel) {
            return Err(Self::not_found());
        }
        std::fs::read(path)
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        let bytes = self.read(path)?;
        String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    fn metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        let rel = self.rel(path).ok_or_else(Self::not_found)?;
        let set = self.set.read().map_err(|_| Self::not_found())?;
        if set.contains(rel) {
            Ok(FileMetadata::new(true, false, false))
        } else if set.is_dir(rel) {
            Ok(FileMetadata::new(false, true, false))
        } else {
            Err(Self::not_found())
        }
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        self.metadata(path)
    }

    fn read_link(&self, path: &Path) -> Result<PathBuf, ResolveError> {
        Err(ResolveError::NotFound(path.display().to_string()))
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        Ok(path.to_path_buf())
    }
}
