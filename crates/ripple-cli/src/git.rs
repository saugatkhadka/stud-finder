//! Computing the changed-file set from git.
//!
//! * working tree (default): tracked changes vs `HEAD` (staged + unstaged) plus untracked files.
//! * `--staged`: the index vs `HEAD`.
//! * `--base REF`: the working tree (plus untracked files) vs `merge-base(REF, HEAD)`,
//!   i.e. what a PR from this branch would change, including uncommitted edits.
//!
//! Renames are detected (`-M`) and reported as one `renamed` entry with
//! `old_path`; deletions are reported with status `deleted`.

use crate::files::run_git;
use anyhow::{Context, Result};
use ripple_policy::{ChangedFile, FileStatus};
use std::collections::BTreeMap;
use std::path::Path;

/// `git hash-object -t tree /dev/null`: diff target for repos without commits.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

#[derive(Debug, Clone)]
pub enum DiffSpec {
    WorkingTree,
    Staged,
    Base(String),
}

fn head_or_empty(root: &Path) -> String {
    match run_git(root, &["rev-parse", "--verify", "-q", "HEAD"]) {
        Ok(out) if !out.is_empty() => String::from_utf8_lossy(&out).trim().to_string(),
        _ => EMPTY_TREE.to_string(),
    }
}

pub fn merge_base(root: &Path, base: &str) -> Result<String> {
    let out = run_git(root, &["merge-base", base, "HEAD"]).with_context(|| format!("finding the merge base of `{base}` and HEAD (in CI, check out with fetch-depth: 0)"))?;
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

/// The changed files for `spec`, sorted by path.
pub fn changed_files(root: &Path, spec: &DiffSpec) -> Result<Vec<ChangedFile>> {
    let (target, cached, untracked) = match spec {
        DiffSpec::WorkingTree => (head_or_empty(root), false, true),
        DiffSpec::Staged => (head_or_empty(root), true, false),
        DiffSpec::Base(r) => (merge_base(root, r)?, false, true),
    };
    let mut args = vec!["diff", "-z", "--raw", "--numstat", "-M", "--relative", "--no-ext-diff"];
    if cached {
        args.push("--cached");
    }
    args.push(&target);
    args.push("--");
    let raw = run_git(root, &args)?;
    let mut files = parse_raw_numstat(&raw);

    let mut args = vec!["diff", "-U0", "--no-color", "-M", "--relative", "--no-ext-diff"];
    if cached {
        args.push("--cached");
    }
    args.push(&target);
    args.push("--");
    if let Ok(patch) = run_git(root, &args) {
        for (path, line) in first_lines(&patch) {
            if let Some(f) = files.get_mut(&path) {
                f.first_line = Some(line);
            }
        }
    }

    if untracked {
        let out = run_git(root, &["ls-files", "-z", "-o", "--exclude-standard"])?;
        for p in out.split(|b| *b == 0).filter(|s| !s.is_empty()) {
            let path = String::from_utf8_lossy(p).into_owned();
            let mut f = ChangedFile::new(path.clone(), FileStatus::Added);
            if let Some(n) = count_lines(&root.join(&path)) {
                f.added = Some(n);
                f.deleted = Some(0);
                f.first_line = Some(1);
            }
            files.entry(path).or_insert(f);
        }
    }
    Ok(files.into_values().collect())
}

fn count_lines(path: &Path) -> Option<u32> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > 8 << 20 {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    let mut n = bytes.iter().filter(|b| **b == b'\n').count();
    if bytes.last().is_some_and(|b| *b != b'\n') {
        n += 1;
    }
    Some(n as u32)
}

/// Parses `git diff -z --raw --numstat` output.
pub(crate) fn parse_raw_numstat(out: &[u8]) -> BTreeMap<String, ChangedFile> {
    let tokens: Vec<String> = out.split(|b| *b == 0).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
    let mut files: BTreeMap<String, ChangedFile> = BTreeMap::new();
    let mut i = 0;
    let next = |i: &mut usize| -> Option<String> {
        let t = tokens.get(*i).cloned();
        *i += 1;
        t
    };
    while i < tokens.len() {
        let tok = next(&mut i).unwrap_or_default();
        let tok = tok.trim_start_matches('\n');
        if tok.is_empty() {
            continue;
        }
        if let Some(raw) = tok.strip_prefix(':') {
            let status = raw.split_whitespace().last().unwrap_or("M");
            let letter = status.chars().next().unwrap_or('M');
            let (path, old) = if matches!(letter, 'R' | 'C') {
                let src = next(&mut i).unwrap_or_default();
                let dst = next(&mut i).unwrap_or_default();
                (dst, Some(src))
            } else {
                (next(&mut i).unwrap_or_default(), None)
            };
            let f = match (letter, old) {
                ('R', Some(old)) => ChangedFile::modified(path.clone()).renamed_from(old),
                ('A', _) | ('C', _) => ChangedFile::new(path.clone(), FileStatus::Added),
                ('D', _) => ChangedFile::new(path.clone(), FileStatus::Deleted),
                _ => ChangedFile::modified(path.clone()),
            };
            files.insert(path, f);
        } else {
            let mut parts = tok.splitn(3, '\t');
            let added = parts.next().and_then(|s| s.parse::<u32>().ok());
            let deleted = parts.next().and_then(|s| s.parse::<u32>().ok());
            let rest = parts.next().unwrap_or("");
            let path = if rest.is_empty() {
                let _src = next(&mut i);
                next(&mut i).unwrap_or_default()
            } else {
                rest.to_string()
            };
            if let Some(f) = files.get_mut(&path) {
                f.added = added;
                f.deleted = deleted;
            }
        }
    }
    files
}

/// First changed line in the new version of each file, from `git diff -U0`.
pub(crate) fn first_lines(patch: &[u8]) -> Vec<(String, u32)> {
    let text = String::from_utf8_lossy(patch);
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("+++ ") {
            let p = p.trim_matches('"');
            current = p.strip_prefix("b/").map(str::to_string);
        } else if let Some(h) = line.strip_prefix("@@ ") {
            let Some(path) = current.take() else { continue };
            // "-a,b +c,d @@"
            let plus = h.split_whitespace().find(|t| t.starts_with('+')).unwrap_or("+1");
            let mut nums = plus[1..].split(',');
            let start: u32 = nums.next().and_then(|s| s.parse().ok()).unwrap_or(1);
            out.push((path, start.max(1)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_raw_and_numstat() {
        let out = b":100644 100644 aaa bbb M\0app/a.rb\0:100644 100644 aaa bbb R087\0old/b.rb\0new/b.rb\0:100644 000000 aaa 000 D\0gone.rb\0:000000 100644 000 bbb A\0img.png\0\
3\t1\tapp/a.rb\0\
2\t2\t\0old/b.rb\0new/b.rb\0\
0\t10\tgone.rb\0\
-\t-\timg.png\0";
        let files = parse_raw_numstat(out);
        let v: Vec<&ChangedFile> = files.values().collect();
        assert_eq!(v.len(), 4);
        let a = &files["app/a.rb"];
        assert_eq!((a.status, a.added, a.deleted), (FileStatus::Modified, Some(3), Some(1)));
        let b = &files["new/b.rb"];
        assert_eq!((b.status, b.old_path.as_deref(), b.added), (FileStatus::Renamed, Some("old/b.rb"), Some(2)));
        assert_eq!(files["gone.rb"].status, FileStatus::Deleted);
        let img = &files["img.png"];
        assert_eq!((img.status, img.added), (FileStatus::Added, None));
    }

    #[test]
    fn first_changed_lines() {
        let patch = b"diff --git a/x.rb b/x.rb\n--- a/x.rb\n+++ b/x.rb\n@@ -3,0 +4,2 @@\n+a\n+b\n@@ -9 +11 @@\n-c\n+d\ndiff --git a/y.rb b/y.rb\n--- a/y.rb\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n";
        assert_eq!(first_lines(patch), [("x.rb".to_string(), 4)]);
    }
}
