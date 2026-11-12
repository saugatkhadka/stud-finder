//! Engine and in-process daemon behaviour with a fake extractor.

mod common;

use common::*;
use ripple_cli::engine::Engine;
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn chain() -> Repo {
    Repo::new(&[
        ("lib/a.txt", "base\n"),
        ("lib/b.txt", "use lib/a.txt\n"),
        ("lib/c.txt", "use lib/b.txt\n"),
        ("app/d.txt", "nothing\n"),
        ("spec/a_spec.txt", "use lib/a.txt\n"),
        ("ripple.yml", "version: 1\nareas:\n  core:\n    tier: critical\n    paths: [lib/c.txt]\n"),
    ])
}

fn edges_of(engine_snap: &ripple_cli::engine::Snapshot) -> BTreeSet<(String, String)> {
    let g = &engine_snap.graph;
    (0..g.edge_count() as u32)
        .map(|e| {
            let (f, t, _) = g.edge(e);
            (g.path(f).to_string(), g.path(t).to_string())
        })
        .collect()
}

#[test]
fn build_then_query() {
    let repo = chain();
    let d = Daemon::start(&repo.root);
    let (r, g) = d.call("impact", json!({"all": true, "files": ["lib/a.txt"]}));
    assert_eq!(g, 1);
    assert_eq!(r["schema_version"], 2);
    assert_schema(&r);
    assert_eq!(impacted_of(&r), vec!["lib/b.txt", "lib/c.txt"]);
    assert_eq!(r["tests"][0]["file"], "spec/a_spec.txt");
    assert_eq!(r["tests"].as_array().unwrap().len(), 1);
    // reaches critical area `core` at depth 2 -> medium (default rule), not high (that needs 1 hop).
    assert_eq!(risk_of(&r), "medium");
    let (deps, _) = d.call("dependents", json!({"file": "lib/a.txt"}));
    assert_schema(&deps);
    let direct: Vec<&str> = deps["dependents"].as_array().unwrap().iter().map(|x| x["file"].as_str().unwrap()).collect();
    assert_eq!(direct, vec!["lib/b.txt"]);
    assert_eq!(tests_of(&deps), vec!["spec/a_spec.txt"]);
    let (why, _) = d.call("why", json!({"from": "lib/c.txt", "to": "lib/a.txt"}));
    assert_eq!(why["found"], true);
    assert_eq!(why["path"].as_array().unwrap().len(), 3);
    let (areas, _) = d.call("areas", json!({"file": "lib/c.txt"}));
    assert_eq!(areas["areas"][0]["area"], "core");
    let (stats, _) = d.call("stats", json!(null));
    assert_eq!(stats["edges"], 3);
    assert_eq!(stats["listing"], "git");
}

#[test]
fn edit_then_query_reflects_edit() {
    let repo = chain();
    let d = Daemon::start(&repo.root);
    // Let the initial index finish first: a write it already sees would not
    // need a second generation.
    let (r, _) = d.call("impact", json!({"all": true, "files": ["lib/a.txt"]}));
    assert_eq!(impacted_of(&r), vec!["lib/b.txt", "lib/c.txt"]);
    // d now depends on c: impact of a grows by one, without naming d in the request (sync drains the watcher).
    repo.write("app/d.txt", "use lib/c.txt\n");
    let (r, g) = d.call("impact", json!({"all": true, "files": ["lib/a.txt"], "sync": true}));
    assert!(g >= 2, "generation {g}");
    assert_eq!(impacted_of(&r), vec!["app/d.txt", "lib/b.txt", "lib/c.txt"]);
    // Naming the edited file is enough even without sync.
    repo.write("lib/c.txt", "no deps\n");
    let (r, _) = d.call("dependencies", json!({"file": "lib/c.txt"}));
    assert_eq!(r["count"], 0);
    // New file, named in the request.
    repo.write("lib/e.txt", "use lib/a.txt\n");
    let (r, _) = d.call("dependencies", json!({"file": "lib/e.txt"}));
    assert_eq!(files_of(&r), vec!["lib/a.txt"]);
    // Deleted file named in the request: reported, and its former dependents are kept (tombstone).
    repo.remove("lib/a.txt");
    let (r, _) = d.call("impact", json!({"all": true, "files": ["lib/a.txt"]}));
    let changed = &r["changed"][0];
    assert_eq!(changed["path"], "lib/a.txt");
    assert_eq!(changed["status"], "deleted");
    assert!(impacted_of(&r).contains(&"lib/b.txt".to_string()), "{r}");
    // ripple.yml edit reloads the policy.
    repo.write("ripple.yml", "version: 1\nareas:\n  edge:\n    tier: high\n    paths: [lib/b.txt]\n");
    let (r, _) = d.call("areas", json!({"file": "lib/b.txt", "sync": true}));
    assert_eq!(r["areas"][0]["area"], "edge");
}

/// The freshness contract: a query that names a file sees that file's latest
/// content, every time, with no sleeps.
#[test]
fn freshness_loop_200() {
    let repo = Repo::new(&[("t/a0.txt", ""), ("t/a1.txt", ""), ("t/a2.txt", ""), ("t/b.txt", "use t/a0.txt\n")]);
    let d = Daemon::start(&repo.root);
    let mut c = d.client();
    let mut server_us = Vec::new();
    for i in 0..200 {
        let target = format!("t/a{}", i % 3);
        repo.write("t/b.txt", &format!("use {target}.txt\n# {i}\n"));
        let r = c.call("dependencies", &json!({"file": "t/b.txt"})).unwrap();
        server_us.push(r.server_us);
        let v: serde_json::Value = serde_json::from_str(&r.result.unwrap()).unwrap();
        assert_eq!(files_of(&v), vec![format!("{target}.txt")], "iteration {i}");
    }
    server_us.sort_unstable();
    eprintln!("freshness loop: server_us p50={} p99={}", server_us[100], server_us[197]);
}

/// Strict mode: `sync` also covers files the request does not name.
#[test]
fn freshness_loop_sync_unnamed() {
    let repo = Repo::new(&[("t/a.txt", ""), ("t/b.txt", "")]);
    let d = Daemon::start(&repo.root);
    let mut c = d.client();
    for i in 0..50 {
        let on = i % 2 == 0;
        repo.write("t/b.txt", if on { "use t/a.txt\n" } else { "\n" });
        let r = c.call("dependents", &json!({"file": "t/a.txt", "sync": true})).unwrap();
        let v: serde_json::Value = serde_json::from_str(&r.result.unwrap()).unwrap();
        assert_eq!(v["counts"]["dependents"], if on { 1 } else { 0 }, "iteration {i}");
    }
}

#[test]
fn concurrent_queries_during_rebuild() {
    let repo = chain();
    let d = Daemon::start(&repo.root);
    d.delay.store(800, Ordering::SeqCst);
    repo.write("app/d.txt", "use lib/a.txt\n");
    // Wait until the indexer has picked up the change and is busy.
    let t0 = Instant::now();
    loop {
        let (s, _) = d.call("stats", json!(null));
        if s["daemon"]["indexing"] == true {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "indexer never started");
        std::thread::sleep(Duration::from_millis(2));
    }
    // Queries about other files are answered from the current snapshot without waiting.
    let started = Instant::now();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let p = d.paths.clone();
            std::thread::spawn(move || {
                let mut c = ripple_cli::client::Client::connect(&p).unwrap();
                let mut worst = Duration::ZERO;
                for _ in 0..25 {
                    let t = Instant::now();
                    let r = c.call("impact", &json!({"all": true, "files": ["lib/b.txt"]})).unwrap();
                    assert!(r.result.is_ok());
                    worst = worst.max(t.elapsed());
                }
                worst
            })
        })
        .collect();
    let worst = handles.into_iter().map(|h| h.join().unwrap()).max().unwrap();
    assert!(worst < Duration::from_millis(300), "a query blocked for {worst:?} during a rebuild");
    let fast = started.elapsed();
    // A query naming the file being indexed waits for it and sees the change.
    let (r, _) = d.call("dependencies", json!({"file": "app/d.txt"}));
    assert_eq!(files_of(&r), vec!["lib/a.txt"]);
    eprintln!("200 queries during an 800ms rebuild: {fast:?} total, worst {worst:?}");
    d.delay.store(0, Ordering::SeqCst);
}

#[test]
fn singleton_lock_in_process() {
    let repo = chain();
    let d = Daemon::start(&repo.root);
    let err = ripple_cli::server::serve(&repo.root, pipeline(Arc::new(AtomicU64::new(0))), d.paths.clone(), ripple_cli::server::ServeOptions { signals: false, ..Default::default() }).unwrap_err();
    assert!(err.to_string().contains("already serving"), "{err}");
    // The first daemon still answers.
    let (r, _) = d.call("stats", json!(null));
    assert_eq!(r["edges"], 3);
}

#[test]
fn incremental_matches_cold_build() {
    let repo = chain();
    let (mut eng, _) = Engine::open(&repo.root, pipeline(Arc::new(AtomicU64::new(0)))).unwrap();
    type Step = Box<dyn Fn(&Repo)>;
    let steps: Vec<Step> = vec![
        Box::new(|r| r.write("app/d.txt", "use lib/c.txt\nuse lib/a.txt\n")),
        Box::new(|r| r.write("lib/new.txt", "use app/d.txt\n")),
        Box::new(|r| r.remove("lib/b.txt")),
        Box::new(|r| r.write("lib/b.txt", "use lib/new.txt\n")),
        Box::new(|r| r.write("spec/a_spec.txt", "use lib/c.txt\n")),
    ];
    let names = ["app/d.txt", "lib/new.txt", "lib/b.txt", "lib/b.txt", "spec/a_spec.txt"];
    for (step, name) in steps.iter().zip(names) {
        step(&repo);
        let snap = eng.apply_paths(&BTreeSet::from([name.to_string()])).unwrap();
        let (_, cold) = Engine::open(&repo.root, pipeline(Arc::new(AtomicU64::new(0)))).unwrap();
        // Tombstones are extra edges the cold build cannot know; compare the rest.
        let warm: BTreeSet<_> = edges_of(&snap).into_iter().collect();
        let cold_e = edges_of(&cold);
        assert!(cold_e.is_subset(&warm), "after {name}: cold {cold_e:?} warm {warm:?}");
        let mut wf: Vec<_> = snap.files.iter().cloned().collect();
        let mut cf: Vec<_> = cold.files.iter().cloned().collect();
        wf.sort();
        cf.sort();
        assert_eq!(wf, cf, "after {name}");
    }
}

/// On case-insensitive file systems (macOS default) a wrongly-cased name must
/// not be added to the workspace as a second file.
#[test]
fn wrong_case_name_is_not_a_new_file() {
    let repo = chain();
    let d = Daemon::start(&repo.root);
    let r = d.client().call("impact", &json!({"all": true, "files": ["LIB/A.TXT", "lib/A.txt"]})).unwrap();
    assert!(r.result.is_ok());
    let (s, _) = d.call("stats", json!(null));
    assert_eq!(s["files"], 6, "{s}");
    // A real case-only rename is a remove plus an add.
    std::fs::rename(repo.root.join("app/d.txt"), repo.root.join("app/D.txt")).unwrap();
    let (s, _) = d.call("dependencies", json!({"file": "app/D.txt", "sync": true}));
    assert_eq!(s["count"], 0);
    let (s, _) = d.call("stats", json!({"sync": true}));
    assert_eq!(s["files"], 6, "{s}");
}

/// Starts a daemon with a short idle timeout on a thread; returns the thread.
fn idle_daemon(repo: &Repo, sock: &std::path::Path, timeout: Duration) -> (ripple_cli::paths::DaemonPaths, std::thread::JoinHandle<anyhow::Result<()>>) {
    let paths = paths_in(sock, &repo.root);
    let (r, p) = (repo.root.clone(), paths.clone());
    let opts = ripple_cli::server::ServeOptions { signals: false, idle_timeout: Some(timeout) };
    let t = std::thread::spawn(move || ripple_cli::server::serve(&r, pipeline(Arc::new(AtomicU64::new(0))), p, opts));
    ripple_cli::client::wait_for_daemon(&paths, Duration::from_secs(10)).expect("daemon up");
    (paths, t)
}

#[test]
fn idle_timeout_stops_daemon_and_allows_restart() {
    let repo = chain();
    let sock = socket_dir();
    let (paths, t) = idle_daemon(&repo, sock.path(), Duration::from_millis(400));
    // Requests keep it alive well past the timeout.
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(1200) {
        let mut c = ripple_cli::client::Client::connect(&paths).expect("still serving");
        assert!(c.call("stats", &json!(null)).unwrap().result.is_ok());
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!t.is_finished(), "exited while requests were arriving");
    // Without requests it exits, removing its socket and releasing the lock.
    let idle_from = Instant::now();
    t.join().unwrap().unwrap();
    let waited = idle_from.elapsed();
    assert!(waited >= Duration::from_millis(250) && waited < Duration::from_secs(3), "{waited:?}");
    assert!(!paths.socket.exists());
    // A new daemon can take over the same root.
    let (paths, t) = idle_daemon(&repo, sock.path(), Duration::ZERO);
    let mut c = ripple_cli::client::Client::connect(&paths).unwrap();
    assert!(c.call("stats", &json!(null)).unwrap().result.is_ok());
    let _ = c.call("shutdown", &json!(null));
    t.join().unwrap().unwrap();
}

/// Connects, retrying while the listen backlog is full (macOS refuses
/// connections beyond 128 queued ones instead of blocking).
fn connect_retry(p: &ripple_cli::paths::DaemonPaths) -> ripple_cli::client::Client {
    let t = Instant::now();
    loop {
        if let Some(c) = ripple_cli::client::Client::connect(p) {
            return c;
        }
        assert!(t.elapsed() < Duration::from_secs(10), "connect refused for 10s");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The daemon holds at most `MAX_CONNECTIONS` connections; another client
/// waits (in the listen backlog) until one closes. Bursts beyond the cap are
/// all answered.
#[test]
fn connection_cap_queues_extra_clients() {
    let repo = chain();
    let d = Daemon::start(&repo.root);
    let cap = ripple_cli::server::MAX_CONNECTIONS;
    let mut idle: Vec<_> = (0..cap).map(|_| connect_retry(&d.paths)).collect();
    std::thread::sleep(Duration::from_millis(200));
    let p = d.paths.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut c = connect_retry(&p);
        let r = c.call("stats", &json!(null)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&r.result.unwrap()).unwrap();
        tx.send(v["daemon"]["connections"].as_u64().unwrap()).unwrap();
    });
    assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "answered beyond the connection cap");
    idle.pop();
    let n = rx.recv_timeout(Duration::from_secs(5)).expect("answered once a slot freed");
    assert!(n as usize <= cap, "{n}");
    drop(idle);
    let t = Instant::now();
    while d.call("stats", json!(null)).0["daemon"]["connections"] != 1 {
        assert!(t.elapsed() < Duration::from_secs(5), "closed connections still hold slots");
        std::thread::sleep(Duration::from_millis(10));
    }
    // A burst of short-lived clients, more than the cap.
    let handles: Vec<_> = (0..cap * 2)
        .map(|_| {
            let p = d.paths.clone();
            std::thread::spawn(move || {
                let mut c = connect_retry(&p);
                c.call("stats", &json!(null)).unwrap().result.is_ok()
            })
        })
        .collect();
    assert!(handles.into_iter().all(|h| h.join().unwrap()));
}
