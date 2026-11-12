//! Snapshots on disk and the sleeping daemon: a mapped snapshot answers
//! exactly like the heap one; corrupt or foreign files are refused; the
//! in-process daemon sleeps, keeps answering identically, ignores
//! irrelevant changes and wakes into a full index on relevant ones.

mod common;

use common::*;
use ripple_cli::engine::{Engine, Snapshot};
use ripple_cli::server::ServeOptions;
use ripple_cli::snapfile;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

fn repo() -> Repo {
    Repo::new(&[
        ("lib/a.txt", "def alpha\nbase\nend\ndef beta\nend\n"),
        ("lib/b.txt", "use lib/a.txt\ndef gamma\nuse lib/c.txt\nend\n"),
        ("lib/c.txt", "use lib/b.txt\n"),
        ("app/d.txt", "nothing\n"),
        ("app/e.txt", "use lib/gone.txt\n"),
        ("spec/a_spec.txt", "use lib/a.txt\n"),
        ("ripple.yml", "version: 1\nareas:\n  core:\n    tier: critical\n    paths: [lib/c.txt]\n"),
    ])
}

fn open(r: &Repo) -> (Engine, Arc<Snapshot>) {
    Engine::open(&r.root, pipeline(Arc::new(AtomicU64::new(0)))).unwrap()
}

fn round_trip(s: &Snapshot, dir: &std::path::Path) -> Arc<Snapshot> {
    let path = dir.join("x.snap");
    snapfile::save(s, &path).unwrap();
    Arc::new(snapfile::load(&path, &s.root).unwrap())
}

#[test]
fn loaded_snapshot_answers_like_the_heap_one() {
    let r = repo();
    let dir = tempfile::tempdir().unwrap();
    let (mut eng, s) = open(&r);
    let loaded = round_trip(&s, dir.path());
    assert!(loaded.files.len() >= 6);
    assert_eq!(snapfile::differences(&s, &loaded, 1), Vec::<String>::new());
    // After incremental passes (seen times, a deletion's tombstones, a patched graph).
    r.write("lib/a.txt", "def alpha\nchanged\nend\n");
    r.remove("app/d.txt");
    r.write("app/new.txt", "use lib/c.txt\n");
    let s = eng.apply_paths(&BTreeSet::from(["lib/a.txt".to_string(), "app/d.txt".to_string(), "app/new.txt".to_string()])).unwrap();
    assert!(s.tombstones > 0 || s.generation > 1);
    let loaded = round_trip(&s, dir.path());
    assert_eq!(snapfile::differences(&s, &loaded, 1), Vec::<String>::new());
}

#[test]
fn bad_snapshots_are_refused() {
    let r = repo();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.snap");
    let (_, s) = open(&r);
    snapfile::save(&s, &path).unwrap();
    let good = std::fs::read(&path).unwrap();
    // Truncated, flipped bytes anywhere, garbage: refused, never a panic.
    for cut in [0, 7, 40, good.len() / 2, good.len() - 1] {
        std::fs::write(&path, &good[..cut]).unwrap();
        assert!(snapfile::load(&path, &r.root).is_err(), "truncated at {cut}");
    }
    for at in (0..good.len()).step_by(good.len() / 97 + 1) {
        let mut b = good.clone();
        b[at] ^= 0x41;
        std::fs::write(&path, &b).unwrap();
        assert!(snapfile::load(&path, &r.root).is_err(), "flipped byte {at}");
    }
    std::fs::write(&path, b"not a snapshot").unwrap();
    assert!(snapfile::load(&path, &r.root).is_err());
    // Another root, another config.
    std::fs::write(&path, &good).unwrap();
    assert!(snapfile::load(&path, &r.root).is_ok());
    let other = repo();
    assert!(snapfile::load(&path, &other.root).is_err());
    r.write("ripple.yml", "version: 1\n");
    assert!(snapfile::load(&path, &r.root).is_err());
    // The write is atomic: no temporary file is left behind.
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn sweep_finds_changes_made_while_no_watcher_ran() {
    let r = repo();
    let (_, s) = open(&r);
    std::thread::sleep(Duration::from_millis(20));
    let since = std::time::SystemTime::now();
    assert!(s.sweep(since).is_empty(), "{:?}", s.sweep(since));
    std::thread::sleep(Duration::from_millis(20));
    r.write("lib/a.txt", "changed\n");
    r.remove("app/d.txt");
    r.write("spec/new_spec.txt", "x\n");
    let found: BTreeSet<String> = s.sweep(since).into_iter().collect();
    for p in ["lib/a.txt", "app/d.txt", "app", "spec"] {
        assert!(found.contains(p), "{p} missing from {found:?}");
    }
}

fn asleep(d: &Daemon) -> bool {
    d.call("stats", json!(null)).0["daemon"]["sleep"]["asleep"] == true
}

fn wait_until(d: &Daemon, want: bool) {
    let t = Instant::now();
    while asleep(d) != want {
        assert!(t.elapsed() < Duration::from_secs(10), "daemon never became asleep={want}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The answers a client sees, minus timings.
fn answers(d: &Daemon) -> Vec<Value> {
    let mut out = Vec::new();
    for f in ["lib/a.txt", "lib/b.txt", "lib/c.txt", "app/d.txt", "app/e.txt", "spec/a_spec.txt"] {
        for m in ["dependents", "dependencies", "file", "areas"] {
            out.push(d.call(m, json!({ "file": f })).0);
        }
        out.push(d.call("impact", json!({ "files": [f] })).0);
    }
    out.push(d.call("why", json!({"from": "lib/c.txt", "to": "lib/a.txt"})).0);
    out.push(d.call("impact", json!({"changed": [{"path": "lib/a.txt", "status": "modified", "hunks": [{"start": 2, "count": 1}]}]})).0);
    for v in &mut out {
        strip(v);
    }
    out
}

/// The answers once history and coverage have finished loading (their
/// state is part of every impact reply).
fn settled(d: &Daemon) -> Vec<Value> {
    let t = Instant::now();
    loop {
        let a = answers(d);
        let text = Value::Array(a.clone()).to_string();
        // Unchanged across a history debounce: no HEAD move still pending.
        std::thread::sleep(ripple_cli::historyd::DEBOUNCE + Duration::from_millis(100));
        if !text.contains("\"building\"") && !text.contains("\"loading\"") && answers(d) == a {
            return a;
        }
        assert!(t.elapsed() < Duration::from_secs(10), "signals never settled");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The first answers that differ, as JSON lines.
fn same(a: &[Value], b: &[Value], what: &str) {
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(b) {
        assert_eq!(x.to_string(), y.to_string(), "{what}");
    }
}

fn strip(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.retain(|k, _| !k.ends_with("_us") && !k.ends_with("_ms") && k != "timing" && k != "generation");
            m.values_mut().for_each(strip);
        }
        Value::Array(a) => a.iter_mut().for_each(strip),
        _ => {}
    }
}

#[test]
fn daemon_sleeps_answers_the_same_and_wakes_on_relevant_changes() {
    let r = repo();
    let opts = ServeOptions { sleep_after: Some(Duration::from_millis(300)), ..Default::default() };
    let d = Daemon::start_with(&r.root, opts);
    let before = settled(&d);
    wait_until(&d, true);
    same(&answers(&d), &before, "asleep answers differ");
    let stats = d.call("stats", json!(null)).0;
    assert_eq!(stats["daemon"]["sleep"]["sleeps"], 1, "{stats}");

    // An ignored file and a git-ignored one do not wake it.
    r.write(".gitignore", "tmp/\n");
    r.commit("ignore tmp");
    wait_until(&d, false);
    wait_until(&d, true);
    r.write("tmp/scratch.txt", "use lib/a.txt\n");
    let (_, _) = d.call("stats", json!({"sync": true}));
    assert!(asleep(&d), "a git-ignored file woke the daemon");

    // A real edit wakes it; answers then match a cold index of the tree.
    r.write("app/d.txt", "use lib/c.txt\n");
    let (v, _) = d.call("dependents", json!({"file": "lib/c.txt", "sync": true}));
    assert!(dependents_of(&v).contains(&"app/d.txt".to_string()), "{v}");
    assert!(!asleep(&d));
    let woke = settled(&d);
    drop(d);
    let fresh = Daemon::start_with(&r.root, ServeOptions { sleep_after: Some(Duration::ZERO), ..Default::default() });
    same(&woke, &settled(&fresh), "woken daemon differs from a fresh one");
}

#[test]
fn edits_while_asleep_are_seen_by_the_next_query() {
    let r = repo();
    let d = Daemon::start_with(&r.root, ServeOptions { sleep_after: Some(Duration::from_millis(200)), ..Default::default() });
    wait_until(&d, true);
    // Named file: found stale by the freshness check even without waiting for the watcher.
    r.write("lib/c.txt", "use lib/b.txt\nuse app/d.txt\n");
    let (v, _) = d.call("dependencies", json!({"file": "lib/c.txt", "sync": true}));
    assert_eq!(v["count"], 2, "{v}");
    let generation = d.call("stats", json!(null)).1;
    assert!(generation >= 2, "generation went backwards: {generation}");
}

/// The real binary sleeps by re-executing itself: same pid, same socket,
/// answers from the mapped snapshot, and a relevant edit wakes it.
#[test]
fn binary_sleeps_by_reexec_and_wakes() {
    let r = Repo::new(&[
        ("app/models/a.rb", "class A\n  def go; end\nend\n"),
        ("app/models/b.rb", "class B\n  def run\n    A.new.go\n  end\nend\n"),
        ("app/models/c.rb", "class C\nend\n"),
        ("ripple.yml", "version: 1\ndaemon:\n  sleep_after_seconds: 1\n"),
    ]);
    let sock = socket_dir();
    let paths = paths_in(sock.path(), &r.root);
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_ripple"))
        .args(["serve", "--foreground", "--root"])
        .arg(&r.root)
        .env("RIPPLE_SOCKET_DIR", sock.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    struct Kill<'a>(&'a mut std::process::Child);
    impl Drop for Kill<'_> {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let child = Kill(&mut child);
    ripple_cli::client::wait_for_daemon(&paths, Duration::from_secs(20)).expect("daemon up");
    let call = |m: &str, p: Value| -> Value {
        let r = ripple_cli::client::Client::connect(&paths).unwrap().call(m, &p).unwrap();
        serde_json::from_str(&r.result.unwrap_or_else(|e| panic!("{m}: {}", e.message))).unwrap()
    };
    let deps = |v: &Value| v["dependents"].as_array().unwrap().iter().map(|d| d["file"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let before = call("dependents", json!({"file": "app/models/a.rb", "sync": true}));
    assert_eq!(deps(&before), ["app/models/b.rb"], "{before}");
    let t = Instant::now();
    while call("stats", json!(null))["daemon"]["sleep"]["asleep"] != true {
        assert!(t.elapsed() < Duration::from_secs(20), "never slept: {}", call("stats", json!(null))["daemon"]);
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(child.0.try_wait().unwrap().is_none(), "the daemon exited instead of sleeping");
    let sleep = call("stats", json!(null))["daemon"]["sleep"].clone();
    assert!(sleep["last"]["load_ms"].is_number(), "did not resume from the snapshot in a new image: {sleep}");
    let snaps: Vec<_> = std::fs::read_dir(r.root.join(".git/ripple")).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".snap")).collect();
    assert_eq!(snaps.len(), 1);
    // The graph's answer (signals such as churn may still be loading in either).
    let mut asleep = call("dependents", json!({"file": "app/models/a.rb"}));
    let mut before = before;
    strip(&mut asleep);
    strip(&mut before);
    for k in ["counts", "dependents", "further", "tests", "index"] {
        assert_eq!(asleep[k], before[k], "{k}");
    }
    // A relevant edit wakes it into a full index.
    r.write("app/models/c.rb", "class C\n  def x\n    A.new\n  end\nend\n");
    let after = call("dependents", json!({"file": "app/models/a.rb", "sync": true}));
    assert_eq!(deps(&after), ["app/models/b.rb", "app/models/c.rb"], "{after}");
    let s = call("stats", json!(null));
    assert_eq!(s["daemon"]["sleep"]["asleep"], false, "{}", s["daemon"]);
    assert_eq!(s["daemon"]["sleep"]["wakes"], 1, "{}", s["daemon"]);
    assert!(child.0.try_wait().unwrap().is_none());
}
