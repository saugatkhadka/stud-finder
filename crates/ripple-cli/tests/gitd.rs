//! The daemon's cached change detection ([`GitTracker`]) must give exactly
//! the answer of the one-shot path ([`git::changed_files`]) after every kind
//! of change: edits, untracked / ignored files, renames, deletions, binary
//! files, index changes (`add`, `reset`, `rm --cached`, `add -N`,
//! `--assume-unchanged`), commits, checkouts, stashes, moved refs.
//!
//! A fake watcher feeds the tracker every work-tree path whose content
//! changed between steps, like the real watcher does.

mod common;

use common::*;
use ripple_cli::git::{self, DiffSpec};
use ripple_cli::gitd::GitTracker;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

/// Every file under `root` except `.git`, with its contents.
fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if e.file_name() == ".git" {
                continue;
            }
            let t = e.file_type().unwrap();
            if t.is_dir() {
                walk(root, &p, out);
            } else {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                out.insert(rel, if t.is_symlink() { std::fs::read_link(&p).unwrap().to_string_lossy().into_owned().into_bytes() } else { std::fs::read(&p).unwrap() });
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

struct Harness {
    root: PathBuf,
    tracker: GitTracker,
    seen: BTreeMap<String, Vec<u8>>,
    specs: Vec<DiffSpec>,
    step: usize,
}

impl Harness {
    fn new(root: &Path, specs: Vec<DiffSpec>) -> Self {
        let tracker = GitTracker::new(root);
        tracker.watching.store(true, Ordering::SeqCst);
        Self { root: root.to_path_buf(), tracker, seen: tree(root), specs, step: 0 }
    }

    /// Reports changed paths to the tracker (the fake watcher), then compares
    /// every spec. `extra` are paths reported as well (e.g. a directory).
    fn check_with(&mut self, what: &str, extra: &[&str]) {
        self.step += 1;
        let now = tree(&self.root);
        let mut changed: Vec<String> = now.iter().filter(|(p, c)| self.seen.get(*p) != Some(*c)).map(|(p, _)| p.clone()).collect();
        changed.extend(self.seen.keys().filter(|p| !now.contains_key(*p)).cloned());
        changed.extend(extra.iter().map(|s| s.to_string()));
        self.tracker.log.push(&changed);
        self.seen = now;
        for spec in &self.specs {
            let want = git::changed_files(&self.root, spec);
            let got = self.tracker.changed(spec);
            match (want, got) {
                (Ok(w), Ok(g)) => assert_eq!(g, w, "step {} `{what}`, {spec:?}", self.step),
                (Err(_), Err(_)) => {}
                (w, g) => panic!("step {} `{what}`, {spec:?}: reference {w:?} vs daemon {g:?}", self.step),
            }
            // Asking again is a cache hit with the same answer.
            if let Ok(w) = git::changed_files(&self.root, spec) {
                assert_eq!(self.tracker.changed(spec).unwrap(), w, "repeat, step {} `{what}`, {spec:?}", self.step);
            }
        }
    }
    fn check(&mut self, what: &str) {
        self.check_with(what, &[]);
    }
}

fn lines(n: usize, tag: &str) -> String {
    (0..n).map(|i| format!("line {i} of {tag}\n")).collect()
}

fn ruby() -> String {
    "class A\n  def one\n    1\n  end\n\n  def two\n    2\n  end\nend\n".to_string()
}

#[test]
fn tracker_matches_reference_through_many_changes() {
    let repo = Repo::new(&[
        ("app/a.rb", &ruby()),
        ("app/b.rb", "b\n"),
        ("lib/big.rb", &lines(40, "big")),
        ("lib/other.rb", &lines(30, "other")),
        ("dir/x.rb", "x\n"),
        ("sp ace.rb", "space\n"),
        ("ünï/cödé.rb", "unicode\n"),
        ("img.bin", "\0\x01\x02"),
        (".gitignore", "*.log\ntmp/\n"),
    ]);
    let g = |args: &[&str]| git(&repo.root, args);
    // A second commit, so HEAD~1 differs from HEAD.
    repo.write("app/a.rb", &ruby().replace("    2\n", "    22\n"));
    repo.write("app/d.rb", "d\n");
    repo.remove("app/b.rb");
    repo.commit("second");
    g(&["branch", "base"]);

    let specs = vec![DiffSpec::WorkingTree, DiffSpec::Staged, DiffSpec::Base("HEAD~1".into()), DiffSpec::Base("base".into()), DiffSpec::Base("HEAD".into())];
    let mut h = Harness::new(&repo.root, specs);
    h.check("clean");

    repo.write("app/a.rb", &ruby().replace("  def one\n    1\n", "  def uno\n    1\n    1.5\n"));
    h.check("edit removing a def line");
    repo.write("new.rb", "new\nfile\n");
    repo.write("x.log", "ignored\n");
    repo.write("tmp/cache/y.rb", "ignored too\n");
    h.check("untracked and ignored files");
    std::fs::rename(repo.root.join("lib/big.rb"), repo.root.join("lib/big2.rb")).unwrap();
    h.check("mv in the work tree");
    g(&["add", "-A"]);
    h.check("git add -A (rename staged)");
    repo.write("lib/big2.rb", &(lines(40, "big") + "one more\n"));
    h.check("small edit keeps the rename");
    repo.write("lib/big2.rb", &lines(40, "completely different"));
    h.check("big edit breaks the rename");
    g(&["reset", "-q"]);
    h.check("git reset");
    repo.write("img.bin", "\0\x05\x06\x07");
    h.check("binary edit");
    repo.remove("dir/x.rb");
    h.check("deleted tracked file");
    g(&["rm", "-q", "--cached", "app/d.rb"]);
    h.check("rm --cached");
    repo.write("ita.rb", "intent\nto add\n");
    h.check("untracked ita.rb");
    g(&["add", "-N", "ita.rb"]);
    h.check("add -N");
    g(&["rm", "-q", "--cached", "ita.rb"]);
    h.check("intent-to-add removed again");
    repo.write(".gitignore", "tmp/\n");
    h.check(".gitignore change un-ignores *.log");
    repo.write("sp ace.rb", "space\nedited\n");
    repo.write("ünï/cödé.rb", "unicode\nedited\n");
    h.check("odd file names");
    g(&["update-index", "--assume-unchanged", "app/a.rb"]);
    h.check("assume-unchanged");
    g(&["update-index", "--no-assume-unchanged", "app/a.rb"]);
    h.check("no-assume-unchanged");
    std::fs::create_dir_all(repo.root.join("n/m")).unwrap();
    std::fs::write(repo.root.join("n/m/z.rb"), "z\n").unwrap();
    std::fs::write(repo.root.join("n/top.rb"), "top\n").unwrap();
    h.check_with("new directory (reported as a directory too)", &["n"]);
    g(&["add", "-A"]);
    g(&["commit", "-q", "-m", "third"]);
    h.check("commit");
    repo.write("app/a.rb", &ruby());
    g(&["stash", "-q"]);
    h.check("stash");
    g(&["stash", "pop", "-q"]);
    h.check("stash pop");
    g(&["checkout", "-q", "HEAD~1", "--", "lib/other.rb"]);
    h.check("checkout a path from HEAD~1");
    g(&["pack-refs", "--all"]);
    h.check("pack-refs");
    let head = g(&["rev-parse", "HEAD"]);
    g(&["update-ref", "refs/heads/base", head.trim()]);
    h.check("the base ref moves");
    g(&["checkout", "-q", "-b", "feature"]);
    repo.write("feature.rb", "f\n");
    g(&["add", "feature.rb"]);
    g(&["commit", "-q", "-m", "feature"]);
    h.check("commit on a new branch");
    g(&["checkout", "-q", "--detach", "HEAD~1"]);
    h.check("detached HEAD");
    g(&["reset", "-q", "--hard", "feature"]);
    g(&["checkout", "-q", "feature"]);
    h.check("reset --hard and back to the branch");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("app/a.rb", repo.root.join("link.rb")).unwrap();
        h.check("untracked symlink");
    }
    repo.write("lib/other.rb", &lines(29, "other"));
    repo.write("lib/other.rb", &lines(31, "other"));
    h.check("two quick edits");
    let c = &h.tracker.counters;
    let (hits, partial, full) = (c.hits.load(Ordering::Relaxed), c.partial.load(Ordering::Relaxed), c.full.load(Ordering::Relaxed));
    eprintln!("tracker: {hits} hits, {partial} partial, {full} full");
    assert!(partial >= 40 && hits >= 100, "the incremental path was not exercised: {hits} hits, {partial} partial, {full} full");
}

#[test]
fn tracker_matches_reference_in_a_subdirectory_root() {
    let repo = Repo::new(&[("sub/a.rb", "a\n"), ("sub/b.rb", &lines(20, "b")), ("top.rb", "t\n")]);
    repo.write("sub/a.rb", "a\nmore\n");
    repo.commit("two");
    let sub = repo.root.join("sub");
    let mut h = Harness::new(&sub, vec![DiffSpec::WorkingTree, DiffSpec::Staged, DiffSpec::Base("HEAD~1".into())]);
    h.check("clean");
    repo.write("sub/a.rb", "edited\n");
    repo.write("sub/new.rb", "n\n");
    repo.write("top.rb", "outside the root\n");
    h.check("edits");
    git(&repo.root, &["add", "-A"]);
    h.check("add");
    std::fs::rename(sub.join("b.rb"), sub.join("c.rb")).unwrap();
    h.check("rename");
}

#[test]
fn tracker_matches_reference_in_a_linked_worktree() {
    let repo = Repo::new(&[("a.rb", "a\n"), ("b.rb", &lines(20, "b"))]);
    repo.write("a.rb", "a2\n");
    repo.commit("two");
    let wt_dir = tempfile::tempdir().unwrap();
    let wt = wt_dir.path().canonicalize().unwrap().join("wt");
    git(&repo.root, &["worktree", "add", "-q", "-b", "wt-branch", wt.to_str().unwrap()]);
    let mut h = Harness::new(&wt, vec![DiffSpec::WorkingTree, DiffSpec::Base("main".into()), DiffSpec::Base("HEAD~1".into())]);
    h.check("clean worktree");
    std::fs::write(wt.join("a.rb"), "in the worktree\n").unwrap();
    std::fs::write(wt.join("new.rb"), "n\n").unwrap();
    h.check("edits");
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-q", "-m", "wt"]);
    h.check("commit in the worktree");
    // `main` moves in the main checkout: merge-base(main, HEAD) changes.
    repo.write("b.rb", "changed on main\n");
    repo.commit("main moves");
    h.check("main moved");
}

#[test]
fn tracker_matches_reference_without_commits() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("a.rb"), "a\n").unwrap();
    let mut h = Harness::new(&root, vec![DiffSpec::WorkingTree, DiffSpec::Staged, DiffSpec::Base("main".into())]);
    h.check("unborn, untracked");
    git(&root, &["add", "a.rb"]);
    h.check("unborn, staged");
}

/// End to end through the daemon with the real file watcher: `impact` with
/// `diff` equals `impact` with the client-computed `changed` list.
#[test]
fn daemon_diff_requests_match_client_side_diff() {
    let repo = Repo::new(&[("lib/a.txt", "base\n"), ("lib/b.txt", "use lib/a.txt\n"), ("lib/c.txt", "use lib/b.txt\n"), ("app/d.txt", "nothing\n")]);
    repo.write("lib/c.txt", "use lib/b.txt\n# second\n");
    repo.commit("two");
    let d = Daemon::start(&repo.root);
    let specs = [DiffSpec::WorkingTree, DiffSpec::Staged, DiffSpec::Base("HEAD~1".into())];
    let compare = |what: &str| {
        for spec in &specs {
            let req = ripple_cli::protocol::DiffRequest::from_spec(spec);
            let (via_daemon, _) = d.call("impact", json!({"all": true, "diff": req}));
            let changed = git::changed_files(&repo.root, spec).unwrap();
            let (via_client, _) = d.call("impact", json!({"all": true, "changed": changed}));
            assert_eq!(via_daemon["changed"], via_client["changed"], "{what} {spec:?}");
            assert_eq!(impacted_of(&via_daemon), impacted_of(&via_client), "{what} {spec:?}");
        }
    };
    compare("clean");
    // No sleeps: the request's cookie round trip must make the edit visible.
    for i in 0..20 {
        repo.write("lib/a.txt", &format!("base\nedit {i}\n"));
        compare(&format!("edit {i}"));
        if i % 5 == 0 {
            repo.write(&format!("lib/new{i}.txt"), "use lib/c.txt\n");
            compare(&format!("untracked {i}"));
        }
    }
    git(&repo.root, &["add", "-A"]);
    compare("git add -A");
    git(&repo.root, &["reset", "-q"]);
    compare("git reset");
    repo.commit("three");
    compare("commit");
    let (stats, _) = d.call("stats", json!(null));
    let gitc = &stats["daemon"]["git"];
    assert_eq!(gitc["watching"], true, "{stats}");
    assert!(gitc["partial_updates"].as_u64().unwrap() > 0, "edits should be partial updates: {gitc}");
    assert!(gitc["cache_hits"].as_u64().unwrap() > 0, "{gitc}");
}

#[test]
fn diff_excludes_files() {
    let repo = Repo::new(&[("a.txt", "a\n")]);
    let d = Daemon::start(&repo.root);
    let r = d.client().call("impact", &json!({"all": true, "diff": {}, "files": ["a.txt"]})).unwrap();
    assert_eq!(r.result.unwrap_err().code, "bad_params");
    let r = d.client().call("impact", &json!({"all": true, "diff": {"base": "no-such-ref"}})).unwrap();
    let e = r.result.unwrap_err();
    assert_eq!(e.code, "git_failed");
    assert!(e.message.contains("merge base of `no-such-ref`"), "{}", e.message);
    // The daemon still answers afterwards.
    let (r, _) = d.call("impact", json!({"all": true, "diff": {}}));
    assert_eq!(r["changed"].as_array().unwrap().len(), 0);
}
