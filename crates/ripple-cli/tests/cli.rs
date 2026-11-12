//! The `ripple` binary: autostart, singleton, stale sockets, `check`, `--base`.
//! Edges come from `RIPPLE_EDGE_FILE` (the binary uses the production pipeline).

mod common;

use common::*;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Env {
    repo: Repo,
    sock: tempfile::TempDir,
    edges: PathBuf,
    _edges_dir: tempfile::TempDir,
}

impl Env {
    fn new(files: &[(&str, &str)], edges: &str) -> Self {
        let repo = Repo::new(files);
        let edges_dir = tempfile::tempdir().unwrap();
        let path = edges_dir.path().join("edges.txt");
        std::fs::write(&path, edges).unwrap();
        Self { repo, sock: socket_dir(), edges: path, _edges_dir: edges_dir }
    }
    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_ripple"));
        c.current_dir(&self.repo.root)
            .env("RIPPLE_SOCKET_DIR", self.sock.path())
            .env("RIPPLE_EDGE_FILE", &self.edges)
            .env_remove("CI")
            .env_remove("GITHUB_ACTIONS")
            .env_remove("RIPPLE_NO_DAEMON")
            .env_remove("RIPPLE_ROOT")
            .env_remove("GITHUB_OUTPUT")
            .env_remove("GITHUB_STEP_SUMMARY");
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
    }
    fn json(&self, args: &[&str]) -> Value {
        let o = self.run(args);
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        serde_json::from_slice(&o.stdout).unwrap()
    }
    fn paths(&self) -> ripple_cli::paths::DaemonPaths {
        paths_in(self.sock.path(), &self.repo.root)
    }
    fn stop(&self) {
        let _ = self.run(&["daemon", "stop"]);
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.stop();
    }
}

const CONFIG: &str = "version: 1\nareas:\n  billing:\n    tier: critical\n    owners: ['@org/pay']\n    paths: [billing/**]\nci:\n  fail_on: critical\n";

fn billing() -> Env {
    Env::new(
        &[("billing/invoice.txt", "1\n"), ("app/charge.txt", "1\n"), ("app/ui.txt", "1\n"), ("spec/invoice_spec.txt", "1\n"), ("ripple.yml", CONFIG)],
        "app/charge.txt billing/invoice.txt\napp/ui.txt app/charge.txt\nspec/invoice_spec.txt billing/invoice.txt\n",
    )
}

fn changed(report: &Value) -> Vec<(String, String, Option<String>)> {
    report["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["path"].as_str().unwrap().to_string(), c["status"].as_str().unwrap().to_string(), c["old_path"].as_str().map(str::to_string)))
        .collect()
}

fn wait_gone(pid: i32) {
    let t = Instant::now();
    // SAFETY: kill(pid, 0) only probes for existence.
    while unsafe { libc::kill(pid, 0) } == 0 && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn spawn_foreground(env: &Env) -> Child {
    env.cmd().args(["serve", "--foreground"]).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap()
}

fn wait_socket(p: &Path) {
    let t = Instant::now();
    while std::os::unix::net::UnixStream::connect(p).is_err() {
        assert!(t.elapsed() < Duration::from_secs(10), "daemon did not come up");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn autostart_then_reuse() {
    let env = billing();
    assert!(ripple_cli::client::Client::connect(&env.paths()).is_none());
    let r = env.json(&["impact", "--files", "billing/invoice.txt", "--format", "json"]);
    assert_eq!(r["risk"], "critical");
    let status = env.run(&["daemon", "status"]);
    assert!(status.status.success(), "daemon should be running after autostart");
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(text.contains("daemon pid"), "{text}");
    let pid1 = env.paths().recorded_pid().unwrap();
    // Second call reuses the same daemon.
    env.json(&["impact", "--files", "app/ui.txt", "--format", "json"]);
    assert_eq!(env.paths().recorded_pid(), Some(pid1));
    env.stop();
    wait_gone(pid1);
    assert_eq!(env.run(&["daemon", "status"]).status.code(), Some(3));
}

#[test]
fn no_autostart_when_disabled() {
    let env = billing();
    for (k, v) in [("RIPPLE_NO_DAEMON", "1"), ("CI", "true")] {
        let o = env.cmd().env(k, v).args(["impact", "--files", "app/ui.txt", "--format", "json"]).output().unwrap();
        assert!(o.status.success());
        assert!(ripple_cli::client::Client::connect(&env.paths()).is_none(), "{k} must disable autostart");
    }
    let o = env.run(&["--no-daemon", "stats", "--format", "json"]);
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(v["daemon"].is_null());
    assert!(ripple_cli::client::Client::connect(&env.paths()).is_none());
}

#[test]
fn singleton_lock() {
    let env = billing();
    let mut first = spawn_foreground(&env);
    wait_socket(&env.paths().socket);
    let second = env.run(&["serve", "--foreground"]);
    assert_eq!(second.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&second.stderr).contains("already serving"), "{}", String::from_utf8_lossy(&second.stderr));
    // Background start is a no-op too.
    let bg = env.run(&["daemon", "start"]);
    assert!(String::from_utf8_lossy(&bg.stdout).contains("already running"));
    env.stop();
    let _ = first.wait();
}

#[test]
fn stale_socket_after_kill() {
    let env = billing();
    let mut first = spawn_foreground(&env);
    let paths = env.paths();
    wait_socket(&paths.socket);
    // SIGKILL: no cleanup, the socket file stays behind.
    first.kill().unwrap();
    first.wait().unwrap();
    assert!(paths.socket.exists(), "socket file should be left behind by SIGKILL");
    assert!(ripple_cli::client::Client::connect(&paths).is_none());
    // The next command detects the dead socket, autostarts a new daemon and answers.
    let r = env.json(&["impact", "--files", "app/charge.txt", "--format", "json"]);
    assert_eq!(impacted_of(&r), vec!["app/ui.txt"]);
    let pid = paths.recorded_pid().unwrap();
    assert_ne!(pid as u32, first.id());
    env.stop();
}

#[test]
fn sigterm_removes_socket() {
    let env = billing();
    let mut d = spawn_foreground(&env);
    let paths = env.paths();
    wait_socket(&paths.socket);
    // SAFETY: signalling our own child.
    unsafe { libc::kill(d.id() as i32, libc::SIGTERM) };
    d.wait().unwrap();
    assert!(!paths.socket.exists());
}

#[test]
fn check_exit_codes_and_formats() {
    let env = billing();
    let root = &env.repo.root;
    git(root, &["checkout", "-q", "-b", "feature"]);
    // Low-risk change: exit 0.
    env.repo.write("app/ui.txt", "2\n");
    env.repo.commit("ui");
    let o = env.run(&["check", "--base", "main", "--format", "json"]);
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    // Critical change: exit 1 at ci.fail_on (critical) ...
    env.repo.write("billing/invoice.txt", "2\n3\n");
    env.repo.commit("billing");
    let o = env.run(&["check", "--base", "main", "--format", "markdown"]);
    assert_eq!(o.status.code(), Some(1));
    let md = String::from_utf8_lossy(&o.stdout);
    assert!(md.contains("<!-- ripple-impact-report -->") && md.contains("CRITICAL risk"), "{md}");
    // ... 0 with --fail-on never.
    assert_eq!(env.run(&["check", "--base", "main", "--fail-on", "never"]).status.code(), Some(0));
    // --fail-on overrides the config the other way too.
    env.repo.write("ripple.yml", CONFIG.split("ci:").next().unwrap()); // no ci.fail_on: never fails by default
    assert_eq!(env.run(&["check", "--base", "main", "--format", "json"]).status.code(), Some(0));
    assert_eq!(env.run(&["check", "--base", "main", "--format", "json", "--fail-on", "critical"]).status.code(), Some(1));
    env.repo.write("ripple.yml", CONFIG);
    // GitHub Actions: auto-selects github format, writes outputs and the step summary, never autostarts.
    let dir = tempfile::tempdir().unwrap();
    let (out, summary) = (dir.path().join("out"), dir.path().join("summary"));
    let o = env.cmd().env("GITHUB_ACTIONS", "true").env("GITHUB_OUTPUT", &out).env("GITHUB_STEP_SUMMARY", &summary).args(["check", "--base", "main"]).output().unwrap();
    assert_eq!(o.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(stdout.contains("::warning file=billing/invoice.txt,line=1"), "{stdout}");
    assert!(stdout.contains("::error"), "{stdout}");
    let outputs = std::fs::read_to_string(&out).unwrap();
    assert!(outputs.contains("risk=critical") && outputs.contains("require=human_review") && outputs.contains("impacted="), "{outputs}");
    assert!(std::fs::read_to_string(&summary).unwrap().contains("## Change impact"));
    assert!(ripple_cli::client::Client::connect(&env.paths()).is_none(), "check must not autostart");
    // Usage and config errors: exit 2.
    assert_eq!(env.run(&["check"]).status.code(), Some(2));
    assert_eq!(env.run(&["check", "--base", "no-such-ref"]).status.code(), Some(2));
    env.repo.write("ripple.yml", "version: 1\nareas:\n  billing:\n    tier: extreme\n    paths: [x]\n");
    let o = env.run(&["check", "--base", "main"]);
    assert_eq!(o.status.code(), Some(2));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("line 4") && err.contains("tier"), "{err}");
}

#[test]
fn base_with_renames_deletions_untracked() {
    let env = Env::new(
        &[("lib/a.txt", "one\ntwo\nthree\nfour\nfive\n"), ("lib/gone.txt", "x\n"), ("lib/keep.txt", "k\n"), ("app/user.txt", "u\n")],
        "app/user.txt lib/gone.txt\napp/user.txt lib/a.txt\n",
    );
    let root = &env.repo.root;
    git(root, &["checkout", "-q", "-b", "feature"]);
    git(root, &["mv", "lib/a.txt", "lib/renamed.txt"]);
    env.repo.remove("lib/gone.txt");
    env.repo.commit("rename and delete");
    env.repo.write("lib/keep.txt", "k\nk2\n"); // uncommitted modification
    env.repo.write("lib/untracked.txt", "new\n"); // untracked
    let r = env.json(&["--no-daemon", "impact", "--base", "main", "--format", "json"]);
    let mut c = changed(&r);
    c.sort();
    assert_eq!(
        c,
        vec![
            ("lib/gone.txt".into(), "deleted".into(), None),
            ("lib/keep.txt".into(), "modified".into(), None),
            ("lib/renamed.txt".into(), "renamed".into(), Some("lib/a.txt".into())),
            ("lib/untracked.txt".into(), "added".into(), None),
        ]
    );
    let keep = r["changed"].as_array().unwrap().iter().find(|x| x["path"] == "lib/keep.txt").unwrap();
    assert_eq!((keep["added"].as_u64(), keep["deleted"].as_u64(), keep["first_line"].as_u64()), (Some(1), Some(0), Some(2)));
    // Deleted / renamed-away files are not in a cold index: reported with a warning, never dropped.
    let codes: Vec<&str> = r["warnings"].as_array().unwrap().iter().filter_map(|w| w["code"].as_str()).collect();
    assert!(codes.contains(&"deleted_file_not_indexed"), "{codes:?}");
    // --staged and the default (working tree vs HEAD).
    let r = env.json(&["--no-daemon", "impact", "--format", "json"]);
    let mut c = changed(&r);
    c.sort();
    assert_eq!(c, vec![("lib/keep.txt".into(), "modified".into(), None), ("lib/untracked.txt".into(), "added".into(), None)]);
    git(root, &["add", "lib/keep.txt"]);
    let r = env.json(&["--no-daemon", "impact", "--staged", "--format", "json"]);
    assert_eq!(changed(&r), vec![("lib/keep.txt".into(), "modified".into(), None)]);
}

#[test]
fn deleted_file_dependents_with_running_daemon() {
    let env = Env::new(&[("lib/gone.txt", "x\n"), ("app/user.txt", "u\n")], "app/user.txt lib/gone.txt\n");
    let r = env.json(&["impact", "--files", "lib/gone.txt", "--format", "json"]);
    assert_eq!(r["impacted"]["total"], 1);
    env.repo.remove("lib/gone.txt");
    std::fs::write(&env.edges, "").unwrap(); // what a real extractor would now produce
    let r = env.json(&["impact", "--files", "lib/gone.txt", "--format", "json"]);
    assert_eq!(r["changed"][0]["status"], "deleted");
    assert_eq!(impacted_of(&r), vec!["app/user.txt"], "tombstone keeps the former dependent");
}

#[test]
fn init_and_config_check() {
    let env = Env::new(&[("app/models/billing/x.rb", "")], "");
    std::fs::remove_file(env.repo.root.join("ripple.yml")).ok();
    let o = env.run(&["init"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(env.run(&["init"]).status.code(), Some(2), "refuses to overwrite");
    let o = env.run(&["config", "check"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(text.contains("area billing") && text.contains("matches no files"), "{text}");
    env.repo.write("ripple.yml", "version: 1\nrules:\n  - id: x\n    when: { touches: [nope] }\n    risk: high\n");
    let o = env.run(&["config", "check"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("nope"));
}

#[test]
fn query_commands_text() {
    let env = billing();
    let o = env.run(&["--no-daemon", "dependents", "billing/invoice.txt", "--depth", "0"]);
    let t = String::from_utf8_lossy(&o.stdout);
    assert!(t.contains("3 dependents") && t.contains("app/ui.txt"), "{t}");
    let o = env.run(&["--no-daemon", "why", "app/ui.txt", "billing/invoice.txt"]);
    assert!(String::from_utf8_lossy(&o.stdout).contains("depends on"));
    let o = env.run(&["--no-daemon", "areas", "billing/invoice.txt"]);
    assert!(String::from_utf8_lossy(&o.stdout).contains("area billing (critical)"));
    let o = env.run(&["--no-daemon", "dependencies", "nope.txt"]);
    assert_eq!(o.status.code(), Some(2));
}
