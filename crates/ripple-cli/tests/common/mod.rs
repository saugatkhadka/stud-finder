#![allow(dead_code)]
//! Test fixtures: a fake extractor, throwaway git repos, an in-process daemon.

use ripple_cli::client::{Client, wait_for_daemon};
use ripple_cli::engine::Pipeline;
use ripple_cli::paths::DaemonPaths;
use ripple_cli::server::{self, ServeOptions};
use ripple_core::{ChangeSet, Confidence, Edge, EdgeKind, ExtractOutput, Extractor, Workspace};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Each line `use <path>` in a file is an exact edge to `<path>`.
/// `delay_ms` makes every build/update sleep (to test concurrency).
pub struct UseExtractor {
    pub delay_ms: Arc<AtomicU64>,
    cache: BTreeMap<String, Vec<Edge>>,
}

impl UseExtractor {
    pub fn new(delay_ms: Arc<AtomicU64>) -> Self {
        Self { delay_ms, cache: BTreeMap::new() }
    }
    fn scan(ws: &Workspace, file: &str) -> Vec<Edge> {
        let text = std::fs::read_to_string(ws.abs(file)).unwrap_or_default();
        text.lines()
            .enumerate()
            .filter_map(|(i, l)| l.strip_prefix("use ").map(|t| Edge::new(file, t.trim(), EdgeKind::RubyConstant, Confidence::Exact).at(i as u32 + 1)))
            .collect()
    }
    fn output(&self, ws: &Workspace) -> ExtractOutput {
        let files: std::collections::HashSet<&str> = ws.files.iter().map(String::as_str).collect();
        let edges = self.cache.values().flatten().filter(|e| files.contains(e.to.as_str())).cloned().collect();
        ExtractOutput { edges, ..Default::default() }
    }
    fn sleep(&self) {
        let d = self.delay_ms.load(Ordering::SeqCst);
        if d > 0 {
            std::thread::sleep(Duration::from_millis(d));
        }
    }
}

impl Extractor for UseExtractor {
    fn name(&self) -> &'static str {
        "use"
    }
    fn build(&mut self, ws: &Workspace) -> anyhow::Result<ExtractOutput> {
        self.sleep();
        self.cache = ws.files.iter().map(|f| (f.clone(), Self::scan(ws, f))).collect();
        Ok(self.output(ws))
    }
    fn update(&mut self, ws: &Workspace, changes: &ChangeSet) -> anyhow::Result<ExtractOutput> {
        self.sleep();
        for r in &changes.removed {
            self.cache.remove(r);
        }
        for m in &changes.modified {
            self.cache.insert(m.clone(), Self::scan(ws, m));
        }
        Ok(self.output(ws))
    }
}

pub fn pipeline(delay: Arc<AtomicU64>) -> Pipeline {
    Pipeline { extractors: vec![Box::new(UseExtractor::new(delay))], linkers: vec![] }
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().expect("git");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub struct Repo {
    _dir: tempfile::TempDir,
    pub root: PathBuf,
}

impl Repo {
    /// A committed git repo with `files` (path, contents).
    pub fn new(files: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "t@example.com"]);
        git(&root, &["config", "user.name", "t"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        let r = Self { _dir: dir, root };
        for (p, c) in files {
            r.write(p, c);
        }
        r.commit("init");
        r
    }
    pub fn write(&self, p: &str, c: &str) {
        let path = self.root.join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, c).unwrap();
    }
    pub fn remove(&self, p: &str) {
        std::fs::remove_file(self.root.join(p)).unwrap();
    }
    pub fn commit(&self, msg: &str) {
        git(&self.root, &["add", "-A"]);
        git(&self.root, &["commit", "-q", "--allow-empty", "-m", msg]);
    }
}

/// A short private socket directory (Unix socket paths are limited to ~104 bytes).
pub fn socket_dir() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("rpl").tempdir_in("/tmp").unwrap()
}

pub fn paths_in(dir: &Path, root: &Path) -> DaemonPaths {
    let h = format!("{:016x}", ripple_cli::paths::fnv1a64(root.as_os_str().as_encoded_bytes()));
    DaemonPaths { dir: dir.to_path_buf(), socket: dir.join(format!("{h}.sock")), lock: dir.join(format!("{h}.lock")), log: dir.join(format!("{h}.log")) }
}

/// An in-process daemon on a thread; shut down on drop.
pub struct Daemon {
    pub paths: DaemonPaths,
    pub delay: Arc<AtomicU64>,
    thread: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
    _sock: tempfile::TempDir,
}

impl Daemon {
    pub fn start(root: &Path) -> Self {
        let sock = socket_dir();
        let paths = paths_in(sock.path(), root);
        let delay = Arc::new(AtomicU64::new(0));
        let (r, p, d) = (root.to_path_buf(), paths.clone(), delay.clone());
        let thread = std::thread::spawn(move || server::serve(&r, pipeline(d), p, ServeOptions { signals: false, ..Default::default() }));
        wait_for_daemon(&paths, Duration::from_secs(10)).expect("daemon up");
        Self { paths, delay, thread: Some(thread), _sock: sock }
    }
    pub fn client(&self) -> Client {
        Client::connect(&self.paths).expect("connect")
    }
    pub fn call(&self, method: &str, params: Value) -> (Value, u64) {
        let r = self.client().call(method, &params).unwrap();
        let body = r.result.unwrap_or_else(|e| panic!("{method}: {} {}", e.code, e.message));
        (serde_json::from_str(&body).unwrap(), r.generation)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut c) = Client::connect(&self.paths) {
            let _ = c.call("shutdown", &Value::Null);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub fn files_of(v: &Value) -> Vec<String> {
    let mut f: Vec<String> = v["files"].as_array().unwrap().iter().map(|x| x["file"].as_str().unwrap().to_string()).collect();
    f.sort();
    f
}

pub fn impacted_of(report: &Value) -> Vec<String> {
    files_of(&report["impacted"])
}
