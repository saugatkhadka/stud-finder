//! Streaming `git log --name-status` and rename following.
//!
//! One subprocess per build or update. Output is read token by token (`-z`)
//! so memory stays bounded by one commit, and the walk can be cancelled
//! between commits (the child is killed).

use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

/// One file entry of `--name-status` (relative to the commit's first parent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Entry {
    /// Modified, type change, unmerged...: the path at the commit.
    Modify(String),
    Add(String),
    Delete(String),
    /// Renamed (or copied) `from -> to`.
    Rename(String, String),
}

#[derive(Debug, Clone)]
pub(crate) struct RawCommit {
    pub oid: String,
    pub time: i64,
    pub entries: Vec<Entry>,
}

/// Error value returned when a walk was cancelled.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("history build cancelled")
    }
}
impl std::error::Error for Cancelled {}

thread_local! {
    static LOW_PRIORITY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs every `git` started from the calling thread from now on at nice 10.
/// A thread's own priority (macOS QoS, Linux per-thread nice) is not passed
/// on to the processes it starts, so the daemon's history thread calls this.
pub fn lower_child_priority() {
    LOW_PRIORITY.with(|l| l.set(true));
}

fn git_cmd(repo: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(repo).args(["-c", "core.quotepath=off", "-c", "log.showSignature=false"]);
    c.stdin(Stdio::null());
    #[cfg(unix)]
    if LOW_PRIORITY.with(|l| l.get()) {
        use std::os::unix::process::CommandExt;
        // SAFETY: setpriority is async-signal-safe and only affects the child.
        unsafe {
            c.pre_exec(|| {
                libc::setpriority(libc::PRIO_PROCESS, 0, 10);
                Ok(())
            });
        }
    }
    c
}

/// Runs a short git command and returns stdout.
pub(crate) fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = git_cmd(repo).args(args).output().context("running git")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// `git rev-parse --verify <rev>^{commit}`.
pub fn resolve_rev(repo: &Path, rev: &str) -> Result<String> {
    let spec = format!("{rev}^{{commit}}");
    Ok(String::from_utf8_lossy(&git(repo, &["rev-parse", "--verify", "-q", &spec])?).trim().to_string())
}

/// Is `a` an ancestor of (or equal to) `b`?
pub fn is_ancestor(repo: &Path, a: &str, b: &str) -> Result<bool> {
    let st = git_cmd(repo).args(["merge-base", "--is-ancestor", a, b]).stdout(Stdio::null()).stderr(Stdio::null()).status().context("running git")?;
    match st.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("git merge-base --is-ancestor {a} {b} failed"),
    }
}

/// Files tracked at `oid`.
pub(crate) fn tracked_files(repo: &Path, oid: &str) -> Result<HashSet<String>> {
    let out = git(repo, &["ls-tree", "-r", "-z", "--name-only", "--full-tree", oid])?;
    Ok(out.split(|&b| b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect())
}

struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Streams `git log --no-merges --name-status -M` for `revs` (newest first),
/// calling `f` once per commit. `max_count` 0 = unlimited.
pub(crate) fn walk(repo: &Path, revs: &[String], max_count: usize, since: Option<i64>, cancel: &AtomicBool, mut f: impl FnMut(RawCommit) -> Result<()>) -> Result<()> {
    let mut cmd = git_cmd(repo);
    cmd.args(["log", "--no-merges", "--name-status", "-M", "-z", "--no-color", "--format=%x01%H%x00%ct"]);
    if max_count > 0 {
        cmd.arg(format!("--max-count={max_count}"));
    }
    // git only takes `@<ts>` with at least 9 digits; smaller cut-offs (before
    // 1973) are no-ops on real history and filtered below anyway.
    if let Some(ts) = since.filter(|&t| t >= 100_000_000) {
        cmd.arg(format!("--since=@{ts}"));
    }
    let keep = |c: &RawCommit| since.is_none_or(|s| c.time >= s);
    cmd.args(revs).arg("--");
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = KillOnDrop(cmd.spawn().context("spawning git log")?);
    let stdout = child.0.stdout.take().context("git log stdout")?;
    let mut r = BufReader::with_capacity(1 << 16, stdout);
    let mut tok = Vec::new();
    let mut cur: Option<RawCommit> = None;
    // State within a commit: expecting the time, a status, or N paths.
    let mut want_time = false;
    let mut pending: Option<(u8, Vec<String>)> = None;
    loop {
        tok.clear();
        let n = r.read_until(0, &mut tok).context("reading git log")?;
        if n == 0 {
            break;
        }
        if tok.last() == Some(&0) {
            tok.pop();
        }
        let mut t: &[u8] = &tok;
        while let [b'\n', rest @ ..] = t {
            t = rest;
        }
        if let [1, oid @ ..] = t {
            if let Some(c) = cur.take() {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Cancelled.into());
                }
                if keep(&c) {
                    f(c)?;
                }
            }
            cur = Some(RawCommit { oid: String::from_utf8_lossy(oid).into_owned(), time: 0, entries: Vec::new() });
            want_time = true;
            pending = None;
            continue;
        }
        let Some(c) = cur.as_mut() else { continue };
        if want_time {
            c.time = std::str::from_utf8(t).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            want_time = false;
            continue;
        }
        if let Some((status, paths)) = pending.as_mut() {
            paths.push(String::from_utf8_lossy(t).into_owned());
            let need = if matches!(status, b'R' | b'C') { 2 } else { 1 };
            if paths.len() == need {
                let (status, mut paths) = pending.take().expect("pending");
                let e = match status {
                    b'A' => Entry::Add(paths.pop().expect("path")),
                    b'D' => Entry::Delete(paths.pop().expect("path")),
                    b'R' => {
                        let to = paths.pop().expect("to");
                        Entry::Rename(paths.pop().expect("from"), to)
                    }
                    // A copy leaves the source in place: the target is new.
                    b'C' => Entry::Add(paths.pop().expect("to")),
                    _ => Entry::Modify(paths.pop().expect("path")),
                };
                c.entries.push(e);
            }
            continue;
        }
        if let Some(&s) = t.first() {
            pending = Some((s, Vec::new()));
        }
    }
    if let Some(c) = cur.take() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        if keep(&c) {
            f(c)?;
        }
    }
    let status = child.0.wait().context("waiting for git log")?;
    if !status.success() {
        let mut err = String::new();
        if let Some(mut e) = child.0.stderr.take() {
            let _ = e.read_to_string(&mut err);
        }
        bail!("git log failed: {}", err.trim());
    }
    Ok(())
}

/// Follows renames while walking history newest-first: maps a path as it was
/// at some older commit to its path at the build commit, or to `None` when
/// that older file is not an ancestor of any file there (the path was later
/// re-created by an add or a rename onto it).
#[derive(Debug, Default)]
pub(crate) struct Renames {
    map: HashMap<String, Option<String>>,
}

impl Renames {
    pub fn resolve(&self, p: &str) -> Option<String> {
        match self.map.get(p) {
            Some(t) => t.clone(),
            None => Some(p.to_string()),
        }
    }

    /// The files of `c`, resolved to their current paths (unfiltered,
    /// possibly duplicated), then folds `c`'s renames and adds into the map
    /// for the older commits that follow.
    pub fn commit(&mut self, c: &RawCommit) -> Vec<String> {
        let mut files = Vec::with_capacity(c.entries.len());
        let mut dead = Vec::new();
        let mut alias = Vec::new();
        for e in &c.entries {
            match e {
                Entry::Modify(p) | Entry::Delete(p) => files.extend(self.resolve(p)),
                Entry::Add(p) => {
                    files.extend(self.resolve(p));
                    dead.push(p.clone());
                }
                Entry::Rename(from, to) => {
                    let target = self.resolve(to);
                    files.extend(target.clone());
                    if from != to {
                        dead.push(to.clone());
                        alias.push((from.clone(), target));
                    }
                }
            }
        }
        // Before this commit the added / renamed-to paths were other files;
        // the renamed-from paths were the files now at their targets. The
        // latter win when one commit does both (`a -> b`, `b -> c`).
        for p in dead {
            self.map.insert(p, None);
        }
        for (from, target) in alias {
            self.map.insert(from, target);
        }
        files
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(entries: Vec<Entry>) -> RawCommit {
        RawCommit { oid: String::new(), time: 0, entries }
    }

    #[test]
    fn renames_chain_and_recreated_paths() {
        let mut r = Renames::default();
        // Newest: b -> c. Then a -> b. Then a modified. Then x added again at "c"? (older "c" is another file).
        assert_eq!(r.commit(&c(vec![Entry::Rename("b".into(), "c".into())])), vec!["c"]);
        assert_eq!(r.commit(&c(vec![Entry::Rename("a".into(), "b".into())])), vec!["c"]);
        assert_eq!(r.commit(&c(vec![Entry::Modify("a".into()), Entry::Modify("c".into()), Entry::Modify("z".into())])), vec!["c", "z"]);
        // Swap in one commit: a -> b and b -> a.
        let mut r = Renames::default();
        r.commit(&c(vec![Entry::Rename("a".into(), "b".into()), Entry::Rename("b".into(), "a".into())]));
        assert_eq!((r.resolve("a"), r.resolve("b")), (Some("b".into()), Some("a".into())));
        // Added path: older history of that path is a different file.
        let mut r = Renames::default();
        r.commit(&c(vec![Entry::Add("n".into())]));
        assert_eq!(r.resolve("n"), None);
    }
}
