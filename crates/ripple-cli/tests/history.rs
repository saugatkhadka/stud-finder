//! Git-history signals in the daemon (`historyd`): background build,
//! updates on HEAD moves, the sidecar, and one-shot mode.

mod common;

use common::*;
use ripple_cli::client::Backend;
use ripple_cli::historyd::HistoryInfo;
use ripple_cli::server::ServeOptions;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn repo() -> Repo {
    Repo::new(&[("lib/a.txt", "base\n"), ("lib/b.txt", "use lib/a.txt\n"), ("app/c.txt", "nothing\n")])
}

fn sidecar(root: &Path) -> PathBuf {
    root.join(".git/ripple/history.bin")
}

fn history_stats(d: &Daemon) -> Value {
    d.call("stats", json!(null)).0["history"].clone()
}

fn wait_history(d: &Daemon, what: &str, ok: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let h = history_stats(d);
        if ok(&h) {
            return h;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}: {h:#}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn impact_history(d: &Daemon, files: &[&str]) -> HistoryInfo {
    let (_, reply) = d.call_full("impact", json!({"files": files}));
    reply.history.expect("impact replies carry `history`")
}

fn head(root: &Path) -> String {
    git(root, &["rev-parse", "HEAD"]).trim().to_string()
}

#[test]
fn queries_are_answered_while_history_builds() {
    let repo = repo();
    let d = Daemon::start_with(&repo.root, ServeOptions { history_hold: Duration::from_secs(3), ..Default::default() });
    let t = Instant::now();
    let h = impact_history(&d, &["lib/a.txt"]);
    assert!(t.elapsed() < Duration::from_secs(2), "impact waited for history: {:?}", t.elapsed());
    assert_eq!(h.state, "building");
    assert!(h.churn.is_empty() && h.usually_changes_with.is_empty());
    let s = wait_history(&d, "ready", |h| h["state"] == "ready");
    assert_eq!(s["head"], head(&repo.root));
    assert_eq!(s["commits"], 1);
    assert_eq!(s["churn_available"], true);
    // One commit: far too short for co-change, churn is still on.
    assert_eq!(s["cochange"], false);
    assert!(s["cochange_reason"].as_str().unwrap().contains("too short"), "{s}");
    let h = impact_history(&d, &["lib/a.txt", "lib/new.txt"]);
    assert_eq!(h.state, "ready");
    assert_eq!(h.churn["lib/a.txt"].commits, 1);
    assert_eq!(h.churn["lib/a.txt"].recent_commits, 1);
    assert!(h.churn["lib/a.txt"].last_changed.is_some());
    assert_eq!(h.churn["lib/new.txt"].commits, 0, "every changed file is listed");
    assert!(h.lookup_us.is_some());
}

#[test]
fn head_move_updates_history() {
    let repo = repo();
    let d = Daemon::start(&repo.root);
    wait_history(&d, "ready", |h| h["state"] == "ready");
    assert_eq!(impact_history(&d, &["lib/a.txt"]).churn["lib/a.txt"].commits, 1);
    for i in 0..3 {
        repo.write("lib/a.txt", &format!("v{i}\n"));
        repo.commit(&format!("change {i}"));
    }
    let new_head = head(&repo.root);
    let s = wait_history(&d, "the new HEAD", |h| h["head"] == new_head.as_str() && h["updating"].is_null());
    assert_eq!(s["commits"], 4);
    assert_eq!(s["build"]["incremental"], true, "{s}");
    // Debounced: three commits in quick succession, one update.
    assert_eq!(s["counters"]["updates"], 1, "{s}");
    assert_eq!(s["counters"]["builds"], 1, "{s}");
    let h = impact_history(&d, &["lib/a.txt"]);
    assert_eq!(h.churn["lib/a.txt"].commits, 4);
    assert_eq!(h.head.as_deref(), Some(new_head.as_str()));

    // A rebase-like sequence with pauses longer than the debounce: one
    // update, once the sequence is over.
    let seq = repo.root.join(".git/rebase-merge");
    std::fs::create_dir_all(&seq).unwrap();
    for i in 0..3 {
        repo.write("app/c.txt", &format!("r{i}\n"));
        repo.commit(&format!("pick {i}"));
        std::thread::sleep(ripple_cli::historyd::DEBOUNCE + Duration::from_millis(200));
    }
    assert_eq!(history_stats(&d)["counters"]["updates"], 1, "no update while the sequence runs");
    std::fs::remove_dir_all(&seq).unwrap();
    let new_head = head(&repo.root);
    let s = wait_history(&d, "the rebased HEAD", |h| h["head"] == new_head.as_str() && h["updating"].is_null());
    assert_eq!(s["counters"]["updates"], 2, "{s}");
    assert_eq!(impact_history(&d, &["app/c.txt"]).churn["app/c.txt"].commits, 4);
}

#[test]
fn disabled_history_has_no_thread_and_no_file() {
    let repo = Repo::new(&[("lib/a.txt", "base\n"), ("ripple.yml", "version: 1\nhistory:\n  enabled: false\n")]);
    let d = Daemon::start(&repo.root);
    let s = history_stats(&d);
    assert_eq!(s["state"], "disabled");
    assert_eq!(s["counters"]["thread"], false);
    let h = impact_history(&d, &["lib/a.txt"]);
    assert_eq!(h.state, "disabled");
    assert!(h.churn.is_empty());
    repo.write("lib/a.txt", "v2\n");
    repo.commit("move HEAD");
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(history_stats(&d)["counters"]["thread"], false);
    assert!(!repo.root.join(".git/ripple").exists(), "no sidecar when disabled");
}

#[test]
fn sidecar_is_reused_across_restarts() {
    let repo = repo();
    {
        let d = Daemon::start(&repo.root);
        let s = wait_history(&d, "ready", |h| h["state"] == "ready");
        assert_eq!(s["build"]["from_sidecar"], false);
        assert_eq!(s["counters"]["builds"], 1);
    }
    assert!(sidecar(&repo.root).is_file());
    let d = Daemon::start(&repo.root);
    let s = wait_history(&d, "ready", |h| h["state"] == "ready");
    assert_eq!(s["build"]["from_sidecar"], true, "{s}");
    assert_eq!((s["counters"]["builds"].as_u64(), s["counters"]["updates"].as_u64()), (Some(0), Some(0)), "{s}");
    assert_eq!(s["head"], head(&repo.root));
    drop(d);
    // A sidecar behind HEAD is used at once, then updated incrementally.
    repo.write("app/c.txt", "v2\n");
    repo.commit("newer");
    let d = Daemon::start(&repo.root);
    let s = wait_history(&d, "the new HEAD", |h| h["head"] == head(&repo.root).as_str() && h["updating"].is_null());
    assert_eq!(s["build"]["from_sidecar"], true, "{s}");
    assert_eq!(s["build"]["incremental"], true, "{s}");
    assert_eq!(s["counters"]["builds"], 0, "{s}");
}

#[test]
fn corrupt_sidecar_is_rebuilt() {
    let repo = repo();
    let p = sidecar(&repo.root);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, b"RPLHIST\0 definitely not a history file").unwrap();
    let d = Daemon::start(&repo.root);
    let s = wait_history(&d, "ready", |h| h["state"] == "ready");
    assert_eq!(s["build"]["from_sidecar"], false);
    assert!(s["build"]["note"].as_str().unwrap_or("").contains("unreadable"), "{s}");
    let h = ripple_history::History::load(&p).expect("rewritten sidecar loads");
    assert_eq!(h.head(), head(&repo.root));
    // Truncated: same.
    drop(d);
    let bytes = std::fs::read(&p).unwrap();
    std::fs::write(&p, &bytes[..bytes.len() / 2]).unwrap();
    let d = Daemon::start(&repo.root);
    let s = wait_history(&d, "ready", |h| h["state"] == "ready");
    assert_eq!(s["build"]["from_sidecar"], false);
    assert_eq!(s["counters"]["builds"], 1);
}

#[test]
fn one_shot_uses_the_sidecar_only() {
    let repo = repo();
    let call = || {
        let mut b = Backend::open(&repo.root, true, false).unwrap();
        b.call("impact", &json!({"files": ["lib/a.txt"]})).unwrap().history.expect("history")
    };
    let h = call();
    assert_eq!(h.state, "unavailable");
    assert!(h.reason.as_deref().unwrap().contains("ripple history build"), "{h:?}");
    assert!(!sidecar(&repo.root).exists(), "one-shot never builds");
    ripple_cli::historyd::cli_build(&repo.root, None).unwrap();
    let h = call();
    assert_eq!(h.state, "ready");
    assert_eq!(h.churn["lib/a.txt"].commits, 1);
    assert!(!h.behind_head);
    // Behind HEAD: still used, and says so.
    repo.write("lib/a.txt", "v2\n");
    repo.commit("newer");
    let h = call();
    assert_eq!((h.state.as_str(), h.behind_head), ("ready", true));
    assert_eq!(h.churn["lib/a.txt"].commits, 1);
}

#[test]
fn rank_says_not_ready_while_history_builds() {
    let repo = Repo::new(&[("app/models/a.rb", "class A\n  def x\n    1 if true\n  end\nend\n"), ("app/models/b.rb", "class B\n  def y\n    A.new.x\n  end\nend\n")]);
    let d = Daemon::start_with(&repo.root, ServeOptions { history_hold: Duration::from_secs(3), ..Default::default() });
    let (r, _) = d.call("rank", json!({}));
    assert_eq!(r["ready"], false, "{r:#}");
    assert!(r["not_ready"].as_str().unwrap().contains("git history"), "{r:#}");
    assert_eq!(r["signals"]["hotspot"]["state"], "building");
    let text = ripple_cli::output::rank(&r);
    assert!(text.starts_with("ranking not ready: waiting for git history") && !text.contains("null"), "{text}");
    wait_history(&d, "ready", |h| h["state"] == "ready");
    let (r, _) = d.call("rank", json!({}));
    // (The test pipeline computes no metrics, so nothing is ranked.)
    assert_eq!((r["ready"].as_bool(), r["files"].as_u64()), (Some(true), Some(0)), "{r:#}");
}
