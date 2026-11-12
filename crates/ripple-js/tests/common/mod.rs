//! Shared helpers for the integration tests: list a directory as a `Workspace`
//! and render edges / call sites as stable one-line strings.
#![allow(dead_code)]

use ripple_core::{Config, ExtractOutput, Workspace};
use std::path::{Path, PathBuf};

pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// All files under `root`, repo-relative and sorted (stand-in for the engine's file list).
pub fn list_files(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n == "node_modules" || n == ".git") {
                    continue;
                }
                walk(&p, root, out);
            } else {
                out.push(p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

pub fn workspace(root: &Path, config: Config) -> Workspace {
    Workspace { root: root.to_path_buf(), files: list_files(root), config }
}

pub fn edges(out: &ExtractOutput) -> Vec<String> {
    let mut v: Vec<String> = out
        .edges
        .iter()
        .map(|e| format!("{} -> {} [{:?} {:?} L{}] {}", e.from, e.to, e.kind, e.confidence, e.line, e.detail.as_deref().unwrap_or("")))
        .collect();
    v.sort();
    v
}

pub fn calls(out: &ExtractOutput) -> Vec<String> {
    let mut v: Vec<String> = out
        .facts
        .js_call_sites
        .iter()
        .map(|c| format!("{}:{} {} {} via {}{}", c.file, c.line, c.method.as_deref().unwrap_or("-"), c.url, c.callee, if c.route_helper { " (route helper)" } else { "" }))
        .collect();
    v.sort();
    v
}

pub fn warnings(out: &ExtractOutput) -> Vec<String> {
    let mut v: Vec<String> = out.warnings.iter().map(|w| format!("{} {}", w.code, w.message)).collect();
    v.sort();
    v
}

/// Prints `lines` as a Rust array literal, for freezing new expectations.
pub fn show(label: &str, lines: &[String]) {
    println!("--- {label}");
    for l in lines {
        println!("        {l:?},");
    }
}
