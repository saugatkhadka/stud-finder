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
    let (r, g) = d.call("impact", json!({"files": ["lib/a.txt"]}));
    assert_eq!(g, 1);
    assert_eq!(r["schema_version"], 1);
    assert_eq!(impacted_of(&r), vec!["lib/b.txt", "lib/c.txt"]);
    assert_eq!(r["tests_to_run"][0]["file"], "spec/a_spec.txt");
    assert_eq!(r["tests_to_run"].as_array().unwrap().len(), 1);
    // reaches critical area `core` at depth 2 -> medium (default rule), not high (that needs 1 hop).
    assert_eq!(r["risk"], "medium");
    let (deps, _) = d.call("dependents", json!({"file": "lib/a.txt"}));
    assert_eq!(files_of(&deps), vec!["lib/b.txt", "spec/a_spec.txt"]);
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
    let (r, _) = d.call("impact", json!({"files": ["lib/a.txt"]}));
    assert_eq!(impacted_of(&r), vec!["lib/b.txt", "lib/c.txt"]);
    // d now depends on c: impact of a grows by one, without naming d in the request (sync drains the watcher).
    repo.write("app/d.txt", "use lib/c.txt\n");
    let (r, g) = d.call("impact", json!({"files": ["lib/a.txt"], "sync": true}));
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
    let (r, _) = d.call("impact", json!({"files": ["lib/a.txt"]}));
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
        assert_eq!(v["count"], if on { 1 } else { 0 }, "iteration {i}");
    }
}

/// Waits until the indexer is in a pass (`stats` never waits, so polling it
/// cannot itself block on the pass).
fn wait_indexing(d: &Daemon) {
    let t0 = Instant::now();
    loop {
        let (s, _) = d.call("stats", json!(null));
        if s["daemon"]["indexing"] == true {
            return;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "indexer never started: {}", s["daemon"]);
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// While a long pass runs, queries wait at most `max_wait_ms`, then answer
/// from the current snapshot marked stale, with the pending path.
#[test]
fn concurrent_queries_during_rebuild() {
    let repo = chain();
    let d = Daemon::start(&repo.root);
    d.call("stats", json!({"sync": true}));
    d.delay.store(800, Ordering::SeqCst);
    repo.write("app/d.txt", "use lib/a.txt\n");
    wait_indexing(&d);
    let started = Instant::now();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let p = d.paths.clone();
            std::thread::spawn(move || {
                let mut c = ripple_cli::client::Client::connect(&p).unwrap();
                let mut worst = Duration::ZERO;
                let mut stale = 0;
                for _ in 0..25 {
                    let t = Instant::now();
                    let r = c.call("impact", &json!({"files": ["lib/b.txt"], "max_wait_ms": 20})).unwrap();
                    assert!(r.result.is_ok());
                    if r.fresh.stale {
                        assert!(r.fresh.pending.contains(&"app/d.txt".to_string()), "{:?}", r.fresh);
                        stale += 1;
                    }
                    worst = worst.max(t.elapsed());
                }
                (worst, stale)
            })
        })
        .collect();
    let results: Vec<(Duration, usize)> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let worst = results.iter().map(|r| r.0).max().unwrap();
    assert!(results.iter().map(|r| r.1).sum::<usize>() > 0, "no query saw the pass in progress");
    assert!(worst < Duration::from_millis(300), "a query blocked for {worst:?} during a rebuild");
    let fast = started.elapsed();
    // A synced query waits for the pass and sees the change.
    let (r, reply) = d.call_full("dependencies", json!({"file": "app/d.txt", "sync": true}));
    assert_eq!(files_of(&r), vec!["lib/a.txt"]);
    assert!(!reply.fresh.stale && reply.fresh.synced, "{:?}", reply.fresh);
    eprintln!("200 queries during an 800ms rebuild: {fast:?} total, worst {worst:?}");
    d.delay.store(0, Ordering::SeqCst);
}

/// Regression (the stale answers of `freshness_loop_sync_unnamed`): a `sync`
/// query used to be answered from the old snapshot when a pass was already
/// running and none of the request's own files were pending, even though the
/// running pass held the (unnamed) edit the answer depends on. Under load the
/// 30 ms debounce expired before the request arrived, so the pass had started.
#[test]
fn sync_waits_for_running_pass_on_unnamed_file() {
    let repo = Repo::new(&[("t/a.txt", ""), ("t/b.txt", "")]);
    let d = Daemon::start(&repo.root);
    d.call("stats", json!({"sync": true}));
    d.delay.store(400, Ordering::SeqCst);
    repo.write("t/b.txt", "use t/a.txt\n");
    wait_indexing(&d);
    let (v, reply) = d.call_full("dependents", json!({"file": "t/a.txt", "sync": true}));
    assert_eq!(v["count"], 1, "a sync answer missed an edit made before it");
    assert!(!reply.fresh.stale);
    d.delay.store(0, Ordering::SeqCst);
}

/// Default policy: wait up to `max_wait_ms`, then answer stale with the
/// pending paths; `sync` waits for them; `no_wait` does not wait at all.
#[test]
fn bounded_wait_stale_flag_sync_and_no_wait() {
    let repo = chain();
    let d = Daemon::start(&repo.root);
    d.call("stats", json!({"sync": true}));
    d.delay.store(700, Ordering::SeqCst);
    repo.write("app/d.txt", "use lib/a.txt\n");
    repo.write("lib/new.txt", "use lib/a.txt\n");
    // Bounded: the edit was made before the request, so it is either in the
    // answer or listed as pending.
    let t = Instant::now();
    let (v, reply) = d.call_full("dependents", json!({"file": "lib/a.txt", "max_wait_ms": 50}));
    let took = t.elapsed();
    assert!(took < Duration::from_millis(600), "bounded wait took {took:?}");
    assert!(reply.fresh.stale, "{:?}", reply.fresh);
    assert!(reply.fresh.synced);
    assert_eq!(reply.fresh.pending, vec!["app/d.txt", "lib/new.txt"], "{:?}", reply.fresh);
    assert_eq!(reply.fresh.pending_count, 2);
    assert_eq!(files_of(&v), vec!["lib/b.txt", "spec/a_spec.txt"], "answered from the previous snapshot");
    // no_wait: answers at once, also stale.
    let t = Instant::now();
    let (_, reply) = d.call_full("dependents", json!({"file": "lib/a.txt", "no_wait": true}));
    assert!(t.elapsed() < Duration::from_millis(300), "{:?}", t.elapsed());
    assert!(reply.fresh.stale && !reply.fresh.synced, "{:?}", reply.fresh);
    // sync: waits, fresh, sees both edits.
    let (v, reply) = d.call_full("dependents", json!({"file": "lib/a.txt", "sync": true}));
    assert!(!reply.fresh.stale && reply.fresh.pending.is_empty() && reply.fresh.pending_count == 0, "{:?}", reply.fresh);
    assert_eq!(files_of(&v), vec!["app/d.txt", "lib/b.txt", "lib/new.txt", "spec/a_spec.txt"]);
    // Impact reports the pending count in the report too.
    d.delay.store(0, Ordering::SeqCst);
    let (r, reply) = d.call_full("impact", json!({"files": ["lib/a.txt"]}));
    assert!(!reply.fresh.stale);
    assert_eq!(r["index"]["pending_changes"], 0);
    // A stale answer is never given when the index has caught up in time.
    repo.write("app/d.txt", "nothing\n");
    let (v, reply) = d.call_full("dependents", json!({"file": "lib/a.txt", "max_wait_ms": 5000}));
    assert!(!reply.fresh.stale);
    assert_eq!(files_of(&v), vec!["lib/b.txt", "lib/new.txt", "spec/a_spec.txt"]);
}

/// A stale `impact` does not map hunks of a pending file (new contents) onto
/// method ranges indexed from its old contents: the file counts as wholly
/// changed. Once indexed, the hunks map onto the new ranges.
#[test]
fn stale_impact_treats_pending_changed_file_as_whole() {
    let repo = Repo::new(&[("lib/m.txt", "def foo\nx\nend\ndef bar\ny\nend\n"), ("lib/u.txt", "use lib/m.txt\n")]);
    let d = Daemon::start(&repo.root);
    d.call("stats", json!({"sync": true}));
    let changed = json!([{"path": "lib/m.txt", "status": "modified", "hunks": [{"start": 5, "count": 1}]}]);
    let (r, reply) = d.call_full("impact", json!({"changed": changed}));
    assert!(!reply.fresh.stale);
    assert_eq!(r["changed"][0]["scopes"], json!(["bar"]), "{}", r["changed"]);
    // Insert a method on top: line 5 is now inside `foo2`... but the index still has the old ranges.
    d.delay.store(600, Ordering::SeqCst);
    repo.write("lib/m.txt", "def foo2\na\nend\ndef foo\nx\nend\ndef bar\ny\nend\n");
    let (r, reply) = d.call_full("impact", json!({"changed": changed, "max_wait_ms": 30}));
    assert!(reply.fresh.stale && reply.fresh.pending.contains(&"lib/m.txt".to_string()), "{:?}", reply.fresh);
    assert!(r["changed"][0].get("scopes").is_none_or(|s| s.is_null()), "stale hunks mapped onto old ranges: {}", r["changed"]);
    assert_eq!(r["index"]["pending_changes"], 1);
    let (r, reply) = d.call_full("impact", json!({"changed": changed, "sync": true}));
    assert!(!reply.fresh.stale);
    assert_eq!(r["changed"][0]["scopes"], json!(["foo"]), "{}", r["changed"]);
    d.delay.store(0, Ordering::SeqCst);
}

/// Many edits while a pass runs are indexed together in the next pass,
/// not one pass each.
#[test]
fn edits_coalesce_into_one_pass() {
    let repo = chain();
    let d = Daemon::start(&repo.root);
    d.call("stats", json!({"sync": true}));
    let passes = |d: &Daemon| d.call("stats", json!(null)).0["daemon"]["indexer"]["passes"].as_u64().unwrap();
    let p0 = passes(&d);
    d.delay.store(300, Ordering::SeqCst);
    repo.write("app/d.txt", "use lib/a.txt\n");
    wait_indexing(&d);
    // 4 "agents" x 10 edits each, while the pass runs; every edit followed by a query.
    let handles: Vec<_> = (0..4)
        .map(|k| {
            let root = repo.root.clone();
            let p = d.paths.clone();
            std::thread::spawn(move || {
                let mut c = ripple_cli::client::Client::connect(&p).unwrap();
                for i in 0..10 {
                    let f = format!("gen/f{k}_{i}.txt");
                    std::fs::create_dir_all(root.join("gen")).unwrap();
                    std::fs::write(root.join(&f), "use lib/a.txt\n").unwrap();
                    assert!(c.call("dependents", &json!({"file": "lib/a.txt", "no_wait": true})).unwrap().result.is_ok());
                }
            })
        })
        .collect();
    handles.into_iter().for_each(|h| h.join().unwrap());
    let (v, _) = d.call("dependents", json!({"file": "lib/a.txt", "sync": true}));
    assert_eq!(v["count"], 43, "{v}");
    let n = passes(&d) - p0;
    // The pass for d.txt, then one (rarely two, if it starts between edits) for the 40 files.
    assert!(n <= 3, "{n} passes for 41 edits");
    d.delay.store(0, Ordering::SeqCst);
}

/// `stats` never waits for the index, and reports the cookie round trips.
#[test]
fn stats_never_waits_and_reports_sync_latency() {
    let repo = chain();
    let d = Daemon::start(&repo.root);
    for _ in 0..20 {
        d.call("impact", json!({"files": ["lib/a.txt"]}));
    }
    d.delay.store(1000, Ordering::SeqCst);
    repo.write("app/d.txt", "use lib/a.txt\n");
    d.call("impact", json!({"files": ["lib/a.txt"], "no_wait": true}));
    let t = Instant::now();
    let (s, _) = d.call("stats", json!(null));
    assert!(t.elapsed() < Duration::from_millis(500), "stats waited {:?}", t.elapsed());
    let f = &s["daemon"]["freshness"];
    assert!(f["cookies"].as_u64().unwrap() >= 20, "{f}");
    eprintln!("cookie round trip: p50 {} us, p99 {} us", f["cookie_p50_us"], f["cookie_p99_us"]);
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
    let r = d.client().call("impact", &json!({"files": ["LIB/A.TXT", "lib/A.txt"]})).unwrap();
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
