//! Listing the files that make up a workspace.

use anyhow::{Context, Result, bail};
use ripple_core::RelPath;
use ripple_policy::Policy;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// True when `root` is inside a git work tree (cheap: looks for `.git` upwards).
pub fn in_git_repo(root: &Path) -> bool {
    root.ancestors().any(|d| d.join(".git").exists())
}

/// Walks up from `start` to the directory containing `.git`, else one containing
/// a ripple config, else `start` itself. The result is canonical.
pub fn find_root(start: &Path) -> Result<PathBuf> {
    let start = start.canonicalize().with_context(|| format!("resolving {}", start.display()))?;
    if let Some(d) = start.ancestors().find(|d| d.join(".git").exists()) {
        return Ok(d.to_path_buf());
    }
    if let Some(d) = start.ancestors().find(|d| ripple_policy::find_config(d).is_some()) {
        return Ok(d.to_path_buf());
    }
    Ok(start)
}

pub(crate) fn git(root: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(root).arg("-c").arg("core.quotepath=off");
    c.stdin(Stdio::null()).stderr(Stdio::piped()).stdout(Stdio::piped());
    c
}

pub(crate) fn run_git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = git(root).args(args).output().context("running git (is it installed?)")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

fn split_z(bytes: &[u8]) -> impl Iterator<Item = String> + '_ {
    bytes.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned())
}

/// Which mechanism produced the listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listing {
    Git,
    Walk,
}

impl Listing {
    pub fn as_str(self) -> &'static str {
        match self {
            Listing::Git => "git",
            Listing::Walk => "walk",
        }
    }
}

/// All files of the workspace, sorted, minus `config.ignore`: tracked plus
/// untracked-but-not-ignored files when in git (files deleted from the work
/// tree are excluded), otherwise a gitignore-aware directory walk.
pub fn list_files(root: &Path, policy: &Policy) -> Result<(Vec<RelPath>, Listing)> {
    let (mut files, how) = if in_git_repo(root) {
        match list_git(root) {
            Ok(f) => (f, Listing::Git),
            Err(_) => (list_walk(root), Listing::Walk),
        }
    } else {
        (list_walk(root), Listing::Walk)
    };
    files.retain(|f| !policy.is_ignored(f));
    files.sort_unstable();
    files.dedup();
    Ok((files, how))
}

fn list_git(root: &Path) -> Result<Vec<RelPath>> {
    // Both listings run concurrently.
    let all = git(root).args(["ls-files", "-z", "-c", "-o", "--exclude-standard"]).spawn()?;
    let deleted = git(root).args(["ls-files", "-z", "-d"]).spawn()?;
    let all = all.wait_with_output()?;
    let deleted = deleted.wait_with_output()?;
    if !all.status.success() {
        bail!("git ls-files failed: {}", String::from_utf8_lossy(&all.stderr).trim());
    }
    let gone: HashSet<String> = split_z(&deleted.stdout).collect();
    Ok(split_z(&all.stdout).filter(|f| !gone.contains(f)).collect())
}

/// Gitignore-aware walk (also used to expand directories reported by the watcher).
pub fn list_walk(dir: &Path) -> Vec<RelPath> {
    walk_under(dir, dir)
}

/// Files under `dir`, as paths relative to `root`.
pub fn walk_under(root: &Path, dir: &Path) -> Vec<RelPath> {
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(dir)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .require_git(false)
        .parents(true)
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    for entry in walker.flatten() {
        if entry.file_type().is_some_and(|t| t.is_file())
            && let Ok(rel) = entry.path().strip_prefix(root)
                && let Some(s) = rel.to_str() {
                    out.push(s.to_string());
                }
    }
    out
}

/// The subset of `paths` git would ignore (empty outside git).
pub fn git_ignored(root: &Path, paths: &[RelPath]) -> HashSet<RelPath> {
    if paths.is_empty() || !in_git_repo(root) {
        return HashSet::new();
    }
    let run = || -> Result<HashSet<RelPath>> {
        let mut child = git(root).args(["check-ignore", "-z", "--stdin"]).stdin(Stdio::piped()).spawn()?;
        {
            let mut stdin = child.stdin.take().context("stdin")?;
            for p in paths {
                stdin.write_all(p.as_bytes())?;
                stdin.write_all(&[0])?;
            }
        }
        let out = child.wait_with_output()?;
        Ok(split_z(&out.stdout).collect())
    };
    run().unwrap_or_default()
}

/// Exact-spelling existence checks (case-insensitive file systems resolve
/// `Foo.rb` to `foo.rb`). Caches directory listings for one batch.
pub struct ExactCase<'a> {
    root: &'a Path,
    dirs: std::collections::HashMap<PathBuf, Option<HashSet<std::ffi::OsString>>>,
}

impl<'a> ExactCase<'a> {
    pub fn new(root: &'a Path) -> Self {
        Self { root, dirs: Default::default() }
    }

    /// True if every component of `rel` exists with exactly this spelling.
    pub fn exists(&mut self, rel: &str) -> bool {
        let mut dir = self.root.to_path_buf();
        for comp in Path::new(rel).components() {
            let std::path::Component::Normal(name) = comp else { return false };
            let entries = self
                .dirs
                .entry(dir.clone())
                .or_insert_with(|| std::fs::read_dir(&dir).ok().map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name()).collect()));
            match entries {
                Some(set) if set.contains(name) => dir.push(name),
                _ => return false,
            }
        }
        true
    }
}
