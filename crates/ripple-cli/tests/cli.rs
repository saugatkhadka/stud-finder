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
    let r = env.json(&["impact", "--files", "billing/invoice.txt", "--format", "json", "--all"]);
    assert_eq!(risk_of(&r), "critical");
    let status = env.run(&["daemon", "status"]);
    assert!(status.status.success(), "daemon should be running after autostart");
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(text.contains("daemon pid"), "{text}");
    let pid1 = env.paths().recorded_pid().unwrap();
    // Second call reuses the same daemon.
    env.json(&["impact", "--files", "app/ui.txt", "--format", "json", "--all"]);
    assert_eq!(env.paths().recorded_pid(), Some(pid1));
    env.stop();
    wait_gone(pid1);
    assert_eq!(env.run(&["daemon", "status"]).status.code(), Some(3));
}

#[test]
fn no_autostart_when_disabled() {
    let env = billing();
    for (k, v) in [("RIPPLE_NO_DAEMON", "1"), ("CI", "true")] {
        let o = env.cmd().env(k, v).args(["impact", "--files", "app/ui.txt", "--format", "json", "--all"]).output().unwrap();
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
    // Nothing accepts any more. A child the daemon was spawning at the moment
    // of the kill can hold a copy of the listening socket for a few
    // milliseconds (until it execs, which closes it), and connects queue on it
    // meanwhile; so poll rather than assert on the first attempt.
    let t = Instant::now();
    while ripple_cli::client::Client::connect(&paths).is_some() {
        assert!(t.elapsed() < Duration::from_secs(5), "the killed daemon's socket still accepts connections");
        std::thread::sleep(Duration::from_millis(5));
    }
    // The next command detects the dead socket, autostarts a new daemon and answers.
    let r = env.json(&["impact", "--files", "app/charge.txt", "--format", "json", "--all"]);
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
    let r = env.json(&["--no-daemon", "impact", "--base", "main", "--format", "json", "--all"]);
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
    let r = env.json(&["--no-daemon", "impact", "--format", "json", "--all"]);
    let mut c = changed(&r);
    c.sort();
    assert_eq!(c, vec![("lib/keep.txt".into(), "modified".into(), None), ("lib/untracked.txt".into(), "added".into(), None)]);
    git(root, &["add", "lib/keep.txt"]);
    let r = env.json(&["--no-daemon", "impact", "--staged", "--format", "json", "--all"]);
    assert_eq!(changed(&r), vec![("lib/keep.txt".into(), "modified".into(), None)]);
}

#[test]
fn deleted_file_dependents_with_running_daemon() {
    let env = Env::new(&[("lib/gone.txt", "x\n"), ("app/user.txt", "u\n")], "app/user.txt lib/gone.txt\n");
    let r = env.json(&["impact", "--files", "lib/gone.txt", "--format", "json", "--all"]);
    assert_eq!(r["counts"]["impacted"], 1);
    env.repo.remove("lib/gone.txt");
    std::fs::write(&env.edges, "").unwrap(); // what a real extractor would now produce
    let r = env.json(&["impact", "--files", "lib/gone.txt", "--format", "json", "--all"]);
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
    // The written file is all comments: the built-ins apply, and only those
    // matching files here are listed.
    assert!(text.contains("area payments") && text.contains("(built-in)") && !text.contains("matches no files") && !text.contains("area jobs"), "{text}");
    assert!(text.contains("default.touches_high"), "{text}");
    let o = env.run(&["init", "--stdout"]);
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stdout).contains("#   payments:"));
    env.repo.write("ripple.yml", "version: 1\nareas:\n  billing: {tier: high, paths: [nowhere/**]}\n");
    let text = String::from_utf8_lossy(&env.run(&["config", "check"]).stdout).to_string();
    assert!(text.contains("area billing") && text.contains("matches no files"), "{text}");
    env.repo.write("ripple.yml", "version: 1\nrules:\n  - id: x\n    when: { touches: [nope] }\n    risk: high\n");
    let o = env.run(&["config", "check"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("nope"));
}

#[test]
fn query_commands_text() {
    let env = billing();
    let o = env.run(&["--no-daemon", "dependents", "billing/invoice.txt", "--depth", "0", "--all"]);
    let t = String::from_utf8_lossy(&o.stdout);
    assert!(t.contains("dependents 1/1:\n  app/charge.txt") && t.contains("  app/ui.txt  hop 2 via app/charge.txt") && t.contains("spec/invoice_spec.txt"), "{t}");
    let v = env.json(&["--no-daemon", "dependents", "billing/invoice.txt", "--format", "json"]);
    assert_schema(&v);
    assert_eq!(v["counts"]["impacted"], 2);
    let o = env.run(&["--no-daemon", "why", "app/ui.txt", "billing/invoice.txt"]);
    assert!(String::from_utf8_lossy(&o.stdout).contains("depends on"));
    let o = env.run(&["--no-daemon", "areas", "billing/invoice.txt"]);
    assert!(String::from_utf8_lossy(&o.stdout).contains("area billing (critical)"));
    let o = env.run(&["--no-daemon", "dependencies", "nope.txt"]);
    assert_eq!(o.status.code(), Some(2));
}

/// `dependents` takes `--sync` and `--no-wait` like `impact`.
#[test]
fn dependents_sync_and_no_wait() {
    let env = billing();
    let v = env.json(&["dependents", "billing/invoice.txt", "--format", "json", "--no-wait"]);
    assert_eq!(v["counts"]["dependents"], 1);
    // A new file and its edge, not named in the request: only a synced answer must see it.
    std::fs::write(&env.edges, "app/charge.txt billing/invoice.txt\napp/ui.txt app/charge.txt\nspec/invoice_spec.txt billing/invoice.txt\napp/new.txt billing/invoice.txt\n").unwrap();
    env.repo.write("app/new.txt", "1\n");
    let v = env.json(&["dependents", "billing/invoice.txt", "--format", "json", "--sync"]);
    assert_eq!(v["counts"]["dependents"], 2, "{v}");
    assert_eq!(env.run(&["dependents", "billing/invoice.txt", "--sync", "--no-wait"]).status.code(), Some(2), "the two flags conflict");
}

// ---- method-level scopes ----------------------------------------------------

/// purchase.txt is laid out like a Ruby model: refund! on lines 4-7, charge!
/// on 9-11. svc#run calls refund!, session#login calls charge!, report uses
/// the class body, sub inherits; spec covers charge!.
const PURCHASE: &str = "class Purchase\n  has_many :refunds\n\n  def refund!\n    a\n    b\n  end\n\n  def charge!\n    c\n  end\nend\n";
const SCOPED_EDGES: &str = "\
@scope app/purchase.txt refund! 4 7
@scope app/purchase.txt charge! 9 11
app/svc.txt#run app/purchase.txt#refund! ruby_constant exact 12 Purchase
app/session.txt#login app/purchase.txt#charge! ruby_constant exact 5 Purchase
app/report.txt app/purchase.txt#@body ruby_constant exact 9 Purchase
@inherit app/sub.txt app/purchase.txt ruby_constant exact 1 Purchase
app/caller.txt app/svc.txt#run
spec/purchase_spec.txt app/purchase.txt#charge!
";

fn scoped_env() -> Env {
    let cfg = "version: 1\ndefaults: {areas: false}\nareas:\n  auth:\n    tier: critical\n    paths: [app/session.txt]\nci:\n  fail_on: critical\n";
    let files = [
        ("app/purchase.txt", PURCHASE),
        ("app/svc.txt", "s\n"),
        ("app/session.txt", "s\n"),
        ("app/report.txt", "r\n"),
        ("app/sub.txt", "s\n"),
        ("app/caller.txt", "c\n"),
        ("spec/purchase_spec.txt", "t\n"),
        ("ripple.yml", cfg),
    ];
    Env::new(&files, SCOPED_EDGES)
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

fn strip_timing(mut v: Value) -> Value {
    v["timing"] = Value::Null;
    // The daemon reads history and coverage in the background, one-shot
    // mode inline: their states legitimately differ between the two.
    if let Some(o) = v.as_object_mut() {
        o.remove("signals");
    }
    v
}

#[test]
fn scope_flag_and_file_level() {
    let env = scoped_env();
    let r = env.json(&["--no-daemon", "impact", "--files", "app/purchase.txt", "--scope", "refund!", "--format", "json", "--all"]);
    assert_eq!(impacted_of(&r), ["app/caller.txt", "app/sub.txt", "app/svc.txt"]);
    assert_eq!(r["changed"][0]["scopes"], serde_json::json!(["refund!"]));
    assert_eq!((r["counts"]["impacted"].as_u64(), r["counts"]["impacted_file_level"].as_u64()), (Some(3), Some(5)));
    assert_eq!(risk_of(&r), "low", "refund! does not reach auth");
    // The svc hop names the method it sits in and the one it calls.
    let svc = r["dependents"].as_array().unwrap().iter().find(|f| f["file"] == "app/svc.txt").unwrap();
    assert_eq!((svc["from_scope"].as_str(), svc["to_scope"].as_str()), (Some("run"), Some("refund!")));

    let r = env.json(&["--no-daemon", "impact", "--files", "app/purchase.txt", "--scope", "charge!", "--format", "json", "--all"]);
    assert_eq!(impacted_of(&r), ["app/session.txt", "app/sub.txt"]);
    assert_eq!(r["tests"][0]["file"], "spec/purchase_spec.txt");
    assert_eq!(risk_of(&r), "medium", "reaches the critical auth area in one hop");

    let r = env.json(&["--no-daemon", "impact", "--files", "app/purchase.txt", "--scope", "refund!", "--file-level", "--format", "json", "--all"]);
    assert_eq!(impacted_of(&r), ["app/caller.txt", "app/report.txt", "app/session.txt", "app/sub.txt", "app/svc.txt"]);
    assert!(r["changed"][0]["scopes"].is_null());
    assert_eq!(r["query"]["file_level"], true);

    // Unknown method: warned; only heirs and whole-file dependents are reached.
    let r = env.json(&["--no-daemon", "impact", "--files", "app/purchase.txt", "--scope", "nope", "--format", "json", "--all"]);
    assert_eq!(impacted_of(&r), ["app/sub.txt"]);
    assert!(r["warnings"].as_array().unwrap().iter().any(|w| w["code"] == "scope_unknown"), "{}", r["warnings"]);
    // --scope needs exactly one explicit file.
    let o = env.run(&["--no-daemon", "impact", "--scope", "refund!"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("--scope needs exactly one"));

    // dependents / why / stats.
    let o = env.run(&["--no-daemon", "dependents", "app/purchase.txt", "--scope", "refund!", "--depth", "0"]);
    let t = String::from_utf8_lossy(&o.stdout);
    assert!(t.contains("dependents 2/2:\n"), "{t}");
    assert!(t.contains("  app/svc.txt:12 in run  ruby_constant Purchase#refund!\n"), "{t}");
    let w = env.json(&["--no-daemon", "why", "app/caller.txt", "app/purchase.txt", "--format", "json"]);
    assert_eq!(w["found"], true);
    assert_eq!(w["path"][1]["from_scope"], "run");
    let w = env.json(&["--no-daemon", "why", "app/caller.txt", "app/purchase.txt", "--scope", "charge!", "--format", "json"]);
    assert_eq!((w["found"].as_bool(), w["file_level_path"].as_bool()), (Some(false), Some(true)));
    let o = env.run(&["--no-daemon", "why", "app/caller.txt", "app/purchase.txt", "--scope", "charge!"]);
    assert!(String::from_utf8_lossy(&o.stdout).contains("not through the methods"), "{}", String::from_utf8_lossy(&o.stdout));
    let o = env.run(&["--no-daemon", "why", "app/caller.txt", "app/purchase.txt"]);
    let t = String::from_utf8_lossy(&o.stdout);
    assert!(t.contains("app/svc.txt#run\n") && t.contains("`Purchase#refund!` line 12"), "{t}");
    let s = env.json(&["--no-daemon", "stats", "--format", "json"]);
    assert_eq!((s["files_with_scopes"].as_u64(), s["scopes"].as_u64()), (Some(1), Some(2)));
    let o = env.run(&["--no-daemon", "stats"]);
    assert!(String::from_utf8_lossy(&o.stdout).contains("scopes 2 in 1 files"));
}

#[test]
fn diff_lines_map_to_methods() {
    let env = scoped_env();
    let root = &env.repo.root;
    git(root, &["checkout", "-q", "-b", "feature"]);
    let edit = |from: &str, to: &str| env.repo.write("app/purchase.txt", &PURCHASE.replace(from, to));

    // A body line of refund!: only its callers.
    edit("    a\n", "    a2\n");
    let r = env.json(&["--no-daemon", "impact", "--format", "json", "--all"]);
    assert_eq!(r["changed"][0]["scopes"], serde_json::json!(["refund!"]));
    assert_eq!(impacted_of(&r), ["app/caller.txt", "app/sub.txt", "app/svc.txt"]);
    // Same through the daemon, and identical JSON.
    let d = env.json(&["impact", "--format", "json", "--all"]);
    assert_eq!(strip_timing(d), strip_timing(r.clone()));

    // check --base: the markdown says what was narrowed and by how much.
    env.repo.commit("edit refund!");
    let o = env.run(&["check", "--base", "main", "--format", "markdown"]);
    let md = String::from_utf8_lossy(&o.stdout);
    assert!(md.contains("**3** impacted files (method-level; 5 if whole files are assumed changed)"), "{md}");
    assert!(md.contains("| `app/purchase.txt` | modified | +1 −1 | body refund! |"), "{md}");
    assert!(md.contains("method-level analysis (1 of 1 changed file narrowed"), "{md}");
    let o = env.run(&["check", "--base", "main", "--format", "markdown", "--file-level"]);
    let md = String::from_utf8_lossy(&o.stdout);
    assert!(md.contains("**5** impacted files ·") && md.contains("file-level analysis"), "{md}");

    // Both methods; a pure deletion inside charge! counts as charge!.
    edit("    a\n", "    a2\n");
    std::fs::write(root.join("app/purchase.txt"), PURCHASE.replace("    a\n", "    a2\n").replace("    c\n", "")).unwrap();
    let r = env.json(&["--no-daemon", "impact", "--base", "main", "--format", "json", "--all"]);
    assert_eq!(r["changed"][0]["scopes"], serde_json::json!(["charge!", "refund!"]));
    assert_eq!(sorted(impacted_of(&r)), ["app/caller.txt", "app/session.txt", "app/sub.txt", "app/svc.txt"]);

    // The def line itself, a class-level line, or a removed method: whole file.
    for (from, to) in [("  def refund!\n", "  def refund!(x)\n"), ("  has_many :refunds\n", "  has_many :refunds2\n"), ("  def charge!\n    c\n  end\n", "")] {
        edit(from, to);
        let r = env.json(&["--no-daemon", "impact", "--format", "json", "--all"]);
        assert!(r["changed"][0]["scopes"].is_null(), "{from:?}: {}", r["changed"][0]);
        assert_eq!(r["counts"]["impacted"], 5, "{from:?}");
    }

    // --staged with unstaged edits on top: line numbers would not match the
    // indexed working tree, so the file is whole.
    edit("    b\n", "    b2\n");
    git(root, &["add", "app/purchase.txt"]);
    let r = env.json(&["--no-daemon", "impact", "--staged", "--format", "json", "--all"]);
    assert_eq!(r["changed"][0]["scopes"], serde_json::json!(["refund!"]));
    env.repo.write("app/purchase.txt", &PURCHASE.replace("    b\n", "    b2\n").replace("    c\n", "    c2\n"));
    let r = env.json(&["--no-daemon", "impact", "--staged", "--format", "json", "--all"]);
    assert!(r["changed"][0]["scopes"].is_null(), "{}", r["changed"][0]);

    // --files carries no diff: whole file.
    let r = env.json(&["--no-daemon", "impact", "--files", "app/purchase.txt", "--format", "json", "--all"]);
    assert!(r["changed"][0]["scopes"].is_null());
    env.stop();
}

#[test]
fn scopes_follow_incremental_updates_in_the_daemon() {
    let env = scoped_env();
    let edit = |from: &str, to: &str| env.repo.write("app/purchase.txt", &PURCHASE.replace(from, to));
    edit("    c\n", "    c2\n");
    let r = env.json(&["impact", "--format", "json", "--all"]);
    assert_eq!(r["changed"][0]["scopes"], serde_json::json!(["charge!"]));
    // The scope table moves (a line inserted above refund!): the daemon must
    // re-read it, or line 10 would still look like charge!.
    std::fs::write(&env.edges, SCOPED_EDGES.replace("refund! 4 7", "refund! 5 8").replace("charge! 9 11", "charge! 10 12")).unwrap();
    env.repo.write("app/purchase.txt", &PURCHASE.replace("  has_many :refunds\n", "  has_many :refunds\n  # note\n").replace("    a\n", "    a2\n"));
    let r = env.json(&["impact", "--format", "json", "--all"]);
    assert!(r["changed"][0]["scopes"].is_null(), "a class-level line was added: {}", r["changed"][0]);
    env.repo.commit("shift");
    env.repo.write("app/purchase.txt", &PURCHASE.replace("  has_many :refunds\n", "  has_many :refunds\n  # note\n").replace("    a\n", "    a3\n"));
    let r = env.json(&["impact", "--format", "json", "--all"]);
    assert_eq!(r["changed"][0]["scopes"], serde_json::json!(["refund!"]), "line 6 is refund! after the shift");
    let s = env.json(&["stats", "--format", "json"]);
    assert_eq!(s["scopes"], 2);
    env.stop();
}

#[test]
fn deleted_js_file_lists_importers_in_a_cold_run() {
    let env = Env::new(&[("app/javascript/a.ts", "import { b } from './b';\nexport const a = b;\n"), ("app/javascript/b.ts", "export const b = 1;\n")], "");
    git(&env.repo.root, &["checkout", "-q", "-b", "feature"]);
    env.repo.remove("app/javascript/b.ts");
    env.repo.commit("drop b");
    let r = env.json(&["--no-daemon", "impact", "--base", "main", "--format", "json", "--all"]);
    let w: Vec<&Value> = r["warnings"].as_array().unwrap().iter().filter(|w| w["code"] == "deleted_file_still_imported").collect();
    assert_eq!(w.len(), 1, "{}", r["warnings"]);
    assert!(w[0]["message"].as_str().unwrap().contains("app/javascript/a.ts"), "{}", w[0]);
}

/// After a daemon exits on its idle timeout, the next command autostarts a
/// new one; `daemon.idle_timeout_minutes` is accepted by `config check`.
#[test]
fn autostart_after_idle_exit() {
    let env = billing();
    env.repo.write("ripple.yml", &format!("{CONFIG}daemon:\n  idle_timeout_minutes: 0\n"));
    let o = env.run(&["config", "check"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let paths = env.paths();
    let (r, p) = (env.repo.root.clone(), paths.clone());
    let opts = ripple_cli::server::ServeOptions { signals: false, idle_timeout: Some(Duration::from_millis(200)), ..Default::default() };
    let t = std::thread::spawn(move || ripple_cli::server::serve(&r, pipeline(Default::default()), p, opts));
    wait_socket(&paths.socket);
    t.join().unwrap().unwrap();
    assert!(ripple_cli::client::Client::connect(&paths).is_none());
    let r = env.json(&["impact", "--files", "billing/invoice.txt", "--format", "json", "--all"]);
    assert_eq!(risk_of(&r), "critical");
    assert!(env.paths().recorded_pid().is_some_and(|p| p as u32 != std::process::id()), "a new daemon process answered");
}

#[test]
fn config_flag_selects_another_file_and_its_own_daemon() {
    let env = billing();
    let dir = tempfile::tempdir().unwrap();
    let alt = dir.path().join("alt.yml");
    std::fs::write(&alt, "version: 1\nareas:\n  billing:\n    tier: low\n    paths: [billing/**]\nrules:\n  - id: alt-rule\n    when: { touches: [billing] }\n    risk: medium\n").unwrap();
    let alt = alt.to_str().unwrap();
    let rules = |r: &Value| r["policy"]["reasons"].as_array().map(|a| a.iter().map(|x| x["rule"].as_str().unwrap().to_string()).collect::<Vec<_>>()).unwrap_or_default();
    let q = ["impact", "--files", "billing/invoice.txt", "--format", "json"];
    for no_daemon in [true, false] {
        let with = |cfg: Option<&str>| {
            let mut a: Vec<&str> = Vec::new();
            if no_daemon {
                a.push("--no-daemon");
            }
            if let Some(c) = cfg {
                a.extend(["--config", c]);
            }
            a.extend(q);
            env.json(&a)
        };
        let r = with(Some(alt));
        assert_eq!((risk_of(&r).as_str(), rules(&r)), (Some("medium"), vec!["alt-rule".to_string()]), "no_daemon {no_daemon}");
        // Without the flag: the repo's ripple.yml (from a different daemon).
        let r = with(None);
        assert_eq!(risk_of(&r), "critical", "no_daemon {no_daemon}");
    }
    let o = env.run(&["--config", alt, "config", "check"]);
    assert!(String::from_utf8_lossy(&o.stdout).contains("alt.yml"), "{}", String::from_utf8_lossy(&o.stdout));
    let o = env.run(&["--config", "nope.yml", "impact"]);
    assert_eq!(o.status.code(), Some(2), "{}", String::from_utf8_lossy(&o.stderr));
    let _ = env.run(&["--config", alt, "daemon", "stop"]);
}
