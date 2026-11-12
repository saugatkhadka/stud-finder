//! Shared helpers for the example binaries and the ignored real-repo tests:
//! building a `Workspace` from a git checkout (or plain directory) and a tiny
//! deterministic RNG.
#![allow(dead_code)]

use ripple_core::{Config, Edge, Workspace};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Lists files the way the engine does (tracked + untracked-not-ignored),
/// minus the default `config.ignore` directory globs.
pub fn workspace(root: &Path) -> Workspace {
    let config = Config::default();
    let files = list_files(root, &config);
    Workspace {
        root: root.to_path_buf(),
        files,
        config,
    }
}

pub fn list_files(root: &Path, config: &Config) -> Vec<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-co", "--exclude-standard", "-z"])
        .output();
    let mut files: Vec<String> = match out {
        Ok(o) if o.status.success() => o
            .stdout
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
        _ => walk(root),
    };
    let prefixes: Vec<String> = config
        .ignore
        .iter()
        .filter_map(|g| g.strip_suffix("**").map(String::from))
        .collect();
    files.retain(|f| !prefixes.iter().any(|p| f.starts_with(p.as_str())) && root.join(f).is_file());
    files.sort();
    files.dedup();
    files
}

fn walk(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(root.join(&rel)) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                continue;
            }
            let child = rel.join(&name);
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(child),
                Ok(t) if t.is_file() => out.push(child.to_string_lossy().replace('\\', "/")),
                _ => {}
            }
        }
    }
    out
}

pub fn sorted(mut edges: Vec<Edge>) -> Vec<Edge> {
    edges.sort_by(|a, b| {
        (&a.from, &a.to, a.kind, a.line, &a.detail)
            .cmp(&(&b.from, &b.to, b.kind, b.line, &b.detail))
    });
    edges
}

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Human-readable symmetric difference of two edge sets (first `limit` lines).
pub fn diff(expected: &[Edge], actual: &[Edge], limit: usize) -> Vec<String> {
    use std::collections::BTreeSet;
    let fmt = |e: &Edge| {
        format!(
            "{} -> {} {:?} {:?} :{} {}",
            e.from,
            e.to,
            e.kind,
            e.confidence,
            e.line,
            e.detail.as_deref().unwrap_or("")
        )
    };
    let a: BTreeSet<String> = expected.iter().map(fmt).collect();
    let b: BTreeSet<String> = actual.iter().map(fmt).collect();
    a.difference(&b)
        .map(|s| format!("- {s}"))
        .chain(b.difference(&a).map(|s| format!("+ {s}")))
        .take(limit)
        .collect()
}
