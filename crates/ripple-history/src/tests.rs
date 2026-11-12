//! Tests on synthetic repositories created in a temp dir.

use super::*;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::AtomicBool;

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    n: u32,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let r = Self { _dir: dir, root, n: 0 };
        r.git(&["init", "-q", "-b", "main"]);
        r
    }
    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_DATE", format!("@{} +0000", 1_700_000_000 + self.n * 60))
            .env("GIT_COMMITTER_DATE", format!("@{} +0000", 1_700_000_000 + self.n * 60))
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
    /// Writes each file (content changes on every call) and commits.
    fn commit(&mut self, files: &[&str]) -> String {
        self.n += 1;
        for f in files {
            let p = self.root.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            // Long stable body so renames with small edits are detected.
            let body = format!("{}\nline for {f}\n{}\n", "stable text\n".repeat(20), self.n);
            std::fs::write(p, body).unwrap();
        }
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", &format!("c{}", self.n)]);
        self.head()
    }
    fn mv(&mut self, from: &str, to: &str) -> String {
        self.n += 1;
        std::fs::create_dir_all(self.root.join(to).parent().unwrap()).unwrap();
        self.git(&["mv", from, to]);
        self.git(&["commit", "-q", "-m", &format!("mv {from} {to}")]);
        self.head()
    }
    fn rm(&mut self, f: &str) -> String {
        self.n += 1;
        self.git(&["rm", "-q", f]);
        self.git(&["commit", "-q", "-m", &format!("rm {f}")]);
        self.head()
    }
    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }
}

fn no() -> AtomicBool {
    AtomicBool::new(false)
}

fn opts() -> Options {
    Options { max_commits: 0, since: None, max_files_per_commit: 5, min_together: 1, keep_per_file: 48, ignore: vec!["vendor/**".into()] }
}

/// Everything with support >= 1, no filters.
fn all() -> PartnerQuery {
    PartnerQuery { limit: 100, min_together: 1, min_confidence: 0.0, min_lift: 0.0, score: Score::Confidence, noisy_or: false }
}

fn names(ps: &[Partner]) -> Vec<(&str, u32, u32)> {
    ps.iter().map(|p| (p.path.as_str(), p.together, p.of)).collect()
}

#[test]
fn counts_confidence_and_churn() {
    let mut r = Repo::new();
    r.commit(&["a.rb", "b.rb"]);
    r.commit(&["a.rb", "b.rb"]);
    r.commit(&["a.rb", "b.rb", "c.rb"]);
    r.commit(&["a.rb"]);
    r.commit(&["c.rb", "d.rb"]);
    let h = History::build(&r.root, "HEAD", &opts(), &no()).unwrap();
    assert_eq!(h.commit_count(), 5);
    assert_eq!(h.file_count(), 4);
    let p = h.partners_with(&["a.rb"], &all(), |_| false);
    assert_eq!(names(&p), [("b.rb", 3, 4), ("c.rb", 1, 4)]);
    assert_eq!(p[0].explain(), "changed together with `a.rb` in 3 of the last 4 commits that touched it");
    assert!((p[0].score - 0.75).abs() < 1e-6);
    // b: 3 of 5 commits => base rate 0.6, confidence 0.75 => lift 1.25.
    assert!((p[0].lift - 1.25).abs() < 1e-6);
    assert_eq!(p[0].last_together, 1_700_000_000 + 3 * 60);
    let c = h.churn("a.rb").unwrap();
    assert_eq!((c.changes, c.pair_changes, c.last_changed), (4, 4, 1_700_000_000 + 4 * 60));
    assert_eq!(h.churn("nope.rb"), None);
    // Query files are never partners; several seeds: best explanation wins and seeds are counted.
    let p = h.partners_with(&["a.rb", "d.rb"], &all(), |_| false);
    assert_eq!(names(&p), [("c.rb", 1, 1), ("b.rb", 3, 4)], "c via d: 1 of 1");
    assert_eq!((p[0].via.as_str(), p[0].seeds), ("d.rb", 2));
    // Filters and exclude.
    let q = PartnerQuery { min_together: 2, ..all() };
    assert_eq!(names(&h.partners_with(&["a.rb"], &q, |_| false)), [("b.rb", 3, 4)]);
    assert!(h.partners_with(&["a.rb"], &all(), |p| p == "b.rb").iter().all(|p| p.path != "b.rb"));
    let q = PartnerQuery { min_lift: 2.0, ..all() };
    assert!(h.partners_with(&["a.rb"], &q, |_| false).iter().all(|p| p.path != "b.rb"), "b changes in most commits");
    // Default stored minimum (2) drops singletons.
    let h2 = History::build(&r.root, "HEAD", &Options { min_together: 2, ..opts() }, &no()).unwrap();
    assert_eq!(names(&h2.partners_with(&["a.rb"], &all(), |_| false)), [("b.rb", 3, 4)]);
}

#[test]
fn renames_are_followed_and_dead_files_dropped() {
    let mut r = Repo::new();
    r.commit(&["app/old.rb", "app/b.rb"]);
    r.commit(&["app/old.rb", "app/b.rb", "app/gone.rb"]);
    r.mv("app/old.rb", "lib/new.rb");
    r.commit(&["lib/new.rb", "app/b.rb"]);
    r.rm("app/gone.rb");
    let h = History::build(&r.root, "HEAD", &opts(), &no()).unwrap();
    let p = h.partners_with(&["lib/new.rb"], &all(), |_| false);
    assert_eq!(names(&p), [("app/b.rb", 3, 4)], "history before the move is kept; gone.rb no longer exists");
    assert!(h.churn("app/old.rb").is_none() && h.churn("app/gone.rb").is_none());
    assert_eq!(h.churn("lib/new.rb").unwrap().changes, 4);
}

#[test]
fn recreated_path_does_not_inherit_history() {
    let mut r = Repo::new();
    r.commit(&["x.rb", "y.rb"]);
    r.commit(&["x.rb", "y.rb"]);
    r.rm("x.rb");
    r.commit(&["x.rb", "z.rb"]); // a new file at the same path
    let h = History::build(&r.root, "HEAD", &opts(), &no()).unwrap();
    assert_eq!(names(&h.partners_with(&["x.rb"], &all(), |_| false)), [("z.rb", 1, 1)]);
}

#[test]
fn sweeps_and_ignored_files_do_not_count() {
    let mut r = Repo::new();
    r.commit(&["a.rb", "b.rb", "vendor/x.rb"]);
    r.commit(&["a.rb", "b.rb", "vendor/x.rb"]);
    // 6 files > max_files_per_commit (5): a sweep.
    r.commit(&["a.rb", "c.rb", "d.rb", "e.rb", "f.rb", "g.rb"]);
    let h = History::build(&r.root, "HEAD", &opts(), &no()).unwrap();
    assert_eq!(names(&h.partners_with(&["a.rb"], &all(), |_| false)), [("b.rb", 2, 2)]);
    assert_eq!(h.churn("a.rb").map(|c| (c.changes, c.pair_changes)), Some((3, 2)), "the sweep still counts as churn");
    assert!(h.churn("vendor/x.rb").is_none());
    assert_eq!(h.pair_commit_count(), 2);
}

#[test]
fn window_limits_commits() {
    let mut r = Repo::new();
    r.commit(&["a.rb", "old.rb"]);
    r.commit(&["a.rb", "b.rb"]);
    r.commit(&["a.rb", "b.rb"]);
    let h = History::build(&r.root, "HEAD", &Options { max_commits: 2, ..opts() }, &no()).unwrap();
    assert_eq!(names(&h.partners_with(&["a.rb"], &all(), |_| false)), [("b.rb", 2, 2)]);
    let h = History::build(&r.root, "HEAD", &Options { since: Some(1_700_000_000 + 2 * 60), ..opts() }, &no()).unwrap();
    assert_eq!(h.commit_count(), 2);
}

/// Builds at every commit from `from` on by folding forward, and compares with full rebuilds.
fn assert_incremental_matches(r: &Repo, o: &Options, shas: &[String]) {
    let mut h = History::build(&r.root, &shas[0], o, &no()).unwrap();
    for sha in &shas[1..] {
        let (next, stats) = h.update(&r.root, sha, o, &no()).unwrap();
        assert!(stats.incremental, "{stats:?}");
        let full = History::build(&r.root, sha, o, &no()).unwrap();
        assert_eq!(next, full, "incremental != full at {sha}");
        assert_eq!(next.to_bytes(), full.to_bytes());
        h = next;
    }
}

#[test]
fn incremental_update_equals_full_rebuild() {
    let mut r = Repo::new();
    let mut shas = vec![r.commit(&["a.rb", "b.rb"])];
    shas.push(r.commit(&["a.rb", "b.rb", "c.rb"]));
    shas.push(r.commit(&["c.rb", "d.rb"]));
    shas.push(r.mv("a.rb", "lib/a.rb"));
    shas.push(r.commit(&["lib/a.rb", "b.rb"]));
    shas.push(r.rm("c.rb"));
    shas.push(r.commit(&["c.rb", "b.rb"])); // re-created
    shas.push(r.commit(&["a.rb", "b.rb", "c.rb", "d.rb", "e.rb", "f.rb", "g.rb"])); // sweep
    shas.push(r.mv("b.rb", "app/b.rb"));
    shas.push(r.commit(&["lib/a.rb", "app/b.rb", "vendor/v.rb"]));
    assert_incremental_matches(&r, &opts(), &shas);
    // A window smaller than the history slides.
    assert_incremental_matches(&r, &Options { max_commits: 4, ..opts() }, &shas);
    // Several commits at once.
    let h = History::build(&r.root, &shas[1], &opts(), &no()).unwrap();
    let (next, stats) = h.update(&r.root, shas.last().unwrap(), &opts(), &no()).unwrap();
    assert_eq!((stats.incremental, stats.walked), (true, shas.len() - 2));
    assert_eq!(next, History::build(&r.root, "HEAD", &opts(), &no()).unwrap());
}

#[test]
fn update_rebuilds_when_not_an_ancestor_or_options_change() {
    let mut r = Repo::new();
    r.commit(&["a.rb", "b.rb"]);
    let base = r.head();
    r.git(&["checkout", "-q", "-b", "side"]);
    let side = r.commit(&["a.rb", "s.rb"]);
    r.git(&["checkout", "-q", "main"]);
    let main = r.commit(&["a.rb", "m.rb"]);
    let h = History::build(&r.root, &side, &opts(), &no()).unwrap();
    let (next, stats) = h.update(&r.root, &main, &opts(), &no()).unwrap();
    assert!(!stats.incremental);
    assert_eq!(next, History::build(&r.root, &main, &opts(), &no()).unwrap());
    assert!(!next.contains_commit(&side) && next.contains_commit(&main) && next.contains_commit(&base));
    let (_, stats) = next.update(&r.root, &main, &Options { max_files_per_commit: 9, ..opts() }, &no()).unwrap();
    assert_eq!(stats.reason.as_deref(), Some("options changed"));
}

#[test]
fn save_load_roundtrip_is_deterministic() {
    let mut r = Repo::new();
    r.commit(&["a.rb", "b.rb", "日本.rb"]);
    r.commit(&["a.rb", "b.rb"]);
    let h = History::build(&r.root, "HEAD", &Options { since: Some(5), ..opts() }, &no()).unwrap();
    let again = History::build(&r.root, "HEAD", &Options { since: Some(5), ..opts() }, &no()).unwrap();
    assert_eq!(h.to_bytes(), again.to_bytes());
    let file = r.root.join(".git/ripple/history.bin");
    h.save(&file).unwrap();
    let loaded = History::load(&file).unwrap();
    assert_eq!(loaded, h);
    assert_eq!(loaded.head(), r.head());
    assert_eq!(names(&loaded.partners_with(&["日本.rb"], &all(), |_| false)), [("a.rb", 1, 1), ("b.rb", 1, 1)]);
    // Truncated / corrupt / foreign files are errors, never panics.
    let bytes = std::fs::read(&file).unwrap();
    for cut in [0, 10, bytes.len() / 2, bytes.len() - 1] {
        std::fs::write(&file, &bytes[..cut]).unwrap();
        assert!(History::load(&file).is_err());
    }
    let mut bad = bytes.clone();
    bad[40] ^= 0xff;
    std::fs::write(&file, &bad).unwrap();
    assert!(History::load(&file).is_err());
}

#[test]
fn empty_repo_history_and_cancellation() {
    let mut r = Repo::new();
    r.commit(&[]);
    let h = History::build(&r.root, "HEAD", &opts(), &no()).unwrap();
    assert_eq!((h.commit_count(), h.file_count()), (1, 0));
    assert!(h.partners(&["a.rb"], 5).is_empty());
    assert_eq!(History::load_bytes_for_test(&h.to_bytes()), h);
    r.commit(&["a.rb"]);
    let err = History::build(&r.root, "HEAD", &opts(), &AtomicBool::new(true)).unwrap_err();
    assert!(err.downcast_ref::<Cancelled>().is_some(), "{err:#}");
}

impl History {
    fn load_bytes_for_test(b: &[u8]) -> Self {
        store::decode(b).unwrap()
    }
}

#[test]
fn recent_changes_count_the_last_days_of_the_window() {
    let day = 86_400;
    let t = 1_700_000_000;
    let h = History::from_commits(
        "h",
        &Options::default(),
        &[("c3", t, &["a.rb", "b.rb"]), ("c2", t - 10 * day, &["a.rb"]), ("c1", t - (crate::RECENT_DAYS + 1) * day, &["a.rb", "b.rb"])],
    );
    let a = h.churn("a.rb").unwrap();
    assert_eq!((a.changes, a.recent_changes), (3, 2));
    assert_eq!(h.churn("b.rb").unwrap().recent_changes, 1);
    // Derived again on load.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("h.bin");
    h.save(&p).unwrap();
    assert_eq!(History::load(&p).unwrap(), h);
}
