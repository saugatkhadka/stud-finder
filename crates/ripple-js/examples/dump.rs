//! `cargo run --release -p ripple-js --example dump -- <repo> [file] [options]`
//!
//! Builds the JS graph for a real repo and prints stats. Options:
//!   --config <file.json>   a `ripple_core::Config` as JSON (js/boundary sections are used)
//!   --alias <prefix=dir>   add a bundler alias (repeatable)
//!   --callee <name>        add an http callee (repeatable)
//!   --unresolved           list every unresolved import warning
//!   --prefixes             top 10 unresolved alias/workspace specifier prefixes
//!   --bench                time 10 single-file updates on a throwaway clone
//!   --edges <out.tsv>      write all edges (from, to, kind, confidence, line, detail), sorted
//!   --calls <out.tsv>      write all call sites, sorted
//!   --parse-errors         list js_parse_error / js_read_error warnings
//! With `[file]`, also prints its dependencies and direct dependents.

use globset::{Glob, GlobSetBuilder};
use ripple_core::{ChangeSet, Config, EdgeKind, ExtractOutput, Extractor, Workspace};
use ripple_js::JsExtractor;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let mut positional = Vec::new();
    let mut cfg = Config::default();
    let (mut list_unresolved, mut prefixes, mut bench, mut parse_errors) = (false, false, false, false);
    let (mut edges_out, mut calls_out) = (None, None);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => cfg = serde_json::from_str(&std::fs::read_to_string(args.next().unwrap())?)?,
            "--alias" => {
                let kv = args.next().unwrap();
                let (k, v) = kv.split_once('=').expect("--alias prefix=dir");
                cfg.js.aliases.insert(k.into(), v.into());
            }
            "--callee" => cfg.boundary.http_callees.push(args.next().unwrap()),
            "--unresolved" => list_unresolved = true,
            "--prefixes" => prefixes = true,
            "--bench" => bench = true,
            "--parse-errors" => parse_errors = true,
            "--edges" => edges_out = args.next(),
            "--calls" => calls_out = args.next(),
            _ => positional.push(a),
        }
    }
    let repo = PathBuf::from(positional.first().expect("usage: dump <repo> [file]")).canonicalize()?;
    let files = list_files(&repo, &cfg)?;
    let ws = Workspace { root: repo.clone(), files, config: cfg.clone() };

    let mut ex = JsExtractor::new();
    let t = Instant::now();
    let out = ex.build(&ws)?;
    let cold = t.elapsed();
    let t = Instant::now();
    let again = JsExtractor::new().build(&ws)?;
    let second_cold = t.elapsed();
    assert_eq!(sorted_edges(&out), sorted_edges(&again), "two builds differ");
    assert_eq!(sorted_calls(&out), sorted_calls(&again), "two builds differ (call sites)");

    println!("repo: {}  ({} files in workspace)", repo.display(), ws.files.len());
    println!("cold build: {:.1} ms (second fresh extractor: {:.1} ms; determinism: identical)", ms(cold), ms(second_cold));
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for e in &out.edges {
        *by_kind.entry(format!("{:?}", e.kind)).or_default() += 1;
    }
    println!("edges: {} {:?}", out.edges.len(), by_kind);
    for (k, v) in &out.stats {
        println!("  {k} = {v}");
    }
    let mut warn: BTreeMap<&str, usize> = BTreeMap::new();
    for w in &out.warnings {
        *warn.entry(w.code.as_str()).or_default() += 1;
    }
    println!("warnings: {warn:?}");
    if list_unresolved {
        for w in out.warnings.iter().filter(|w| w.code == "js_unresolved_import") {
            println!("  {}", w.message);
        }
    }
    if parse_errors {
        for w in out.warnings.iter().filter(|w| w.code != "js_unresolved_import") {
            println!("  [{}] {}", w.code, w.message);
        }
    }
    if prefixes {
        let mut by_prefix: BTreeMap<String, usize> = BTreeMap::new();
        for w in out.warnings.iter().filter(|w| w.code == "js_unresolved_import" && !w.message.contains("relative")) {
            let spec = w.message.split('\'').nth(1).unwrap_or("");
            *by_prefix.entry(prefix_of(spec)).or_default() += 1;
        }
        let mut v: Vec<_> = by_prefix.into_iter().collect();
        v.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        println!("top unresolved alias/workspace prefixes:");
        for (p, n) in v.iter().take(10) {
            println!("  {n:6}  {p}");
        }
    }
    if let Some(path) = edges_out {
        let mut s = String::new();
        for e in sorted_edges(&out) {
            s.push_str(&format!("{}\t{}\t{:?}\t{:?}\t{}\t{}\n", e.from, e.to, e.kind, e.confidence, e.line, e.detail.unwrap_or_default()));
        }
        std::fs::write(path, s)?;
    }
    if let Some(path) = calls_out {
        let mut s = String::new();
        for c in sorted_calls(&out) {
            s.push_str(&format!("{}:{}\t{}\t{}\t{}\t{}\n", c.file, c.line, c.method.unwrap_or_default(), c.url, c.callee, c.route_helper));
        }
        std::fs::write(path, s)?;
    }
    if let Some(file) = positional.get(1) {
        println!("\n{file} depends on:");
        for e in out.edges.iter().filter(|e| &e.from == file) {
            println!("  -> {} ({:?}, line {}, {})", e.to, e.kind, e.line, e.detail.as_deref().unwrap_or(""));
        }
        println!("{file} is depended on by:");
        for e in out.edges.iter().filter(|e| &e.to == file) {
            println!("  <- {} ({:?}, line {})", e.from, e.kind, e.line);
        }
        let calls: Vec<_> = out.facts.js_call_sites.iter().filter(|c| &c.file == file).collect();
        println!("call sites in {file}: {}", calls.len());
        for c in calls {
            println!("  {}:{} {:?} {} via {} helper={}", c.file, c.line, c.method, c.url, c.callee, c.route_helper);
        }
    }

    if bench {
        run_bench(&ws, &out)?;
    }
    Ok(())
}

fn prefix_of(spec: &str) -> String {
    let mut parts = spec.split('/');
    match parts.next() {
        Some(first) if first.starts_with('@') => format!("{first}/{}", parts.next().unwrap_or("")),
        Some(first) => first.to_string(),
        None => String::new(),
    }
}

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn sorted_edges(o: &ExtractOutput) -> Vec<ripple_core::Edge> {
    let mut v = o.edges.clone();
    v.sort_by(|a, b| (&a.from, &a.to, a.kind, a.line, &a.detail).cmp(&(&b.from, &b.to, b.kind, b.line, &b.detail)));
    v
}

fn sorted_calls(o: &ExtractOutput) -> Vec<ripple_core::JsCallSite> {
    let mut v = o.facts.js_call_sites.clone();
    v.sort_by(|a, b| (&a.file, a.line, &a.url, &a.callee, &a.method).cmp(&(&b.file, b.line, &b.url, &b.callee, &b.method)));
    v
}

/// `git ls-files` (tracked + untracked-not-ignored) minus `config.ignore`, sorted.
fn list_files(repo: &Path, cfg: &Config) -> anyhow::Result<Vec<String>> {
    let out = Command::new("git").args(["ls-files", "-co", "--exclude-standard", "-z"]).current_dir(repo).output()?;
    anyhow::ensure!(out.status.success(), "git ls-files failed");
    let mut ignore = GlobSetBuilder::new();
    for g in &cfg.ignore {
        ignore.add(Glob::new(g)?);
    }
    let ignore = ignore.build()?;
    let mut files: Vec<String> = out
        .stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .filter(|f| !ignore.is_match(f) && repo.join(f).is_file())
        .collect();
    files.sort();
    files.dedup();
    Ok(files)
}

/// Median of 10 single-file updates. Runs on a throwaway clone (the repo is
/// read-only): code and config files are copied, other files are empty placeholders.
fn run_bench(ws: &Workspace, base: &ExtractOutput) -> anyhow::Result<()> {
    let tmp = std::env::temp_dir().join(format!("ripple-js-bench-{}", std::process::id()));
    for f in &ws.files {
        let dst = tmp.join(f);
        std::fs::create_dir_all(dst.parent().unwrap())?;
        let src = ws.root.join(f);
        let ext = f.rsplit('.').next().unwrap_or("");
        let keep = ["js", "jsx", "ts", "tsx", "mjs", "cjs", "mts", "cts", "vue", "json"].contains(&ext);
        if keep { std::fs::copy(&src, &dst)?; } else { std::fs::write(&dst, b"")?; }
    }
    let clone = Workspace { root: tmp.clone(), files: ws.files.clone(), config: ws.config.clone() };
    let mut ex = JsExtractor::new();
    ex.build(&clone)?;
    // Pick, deterministically, up to 10 code files with at least 3 imports.
    let mut importers: BTreeMap<&str, usize> = BTreeMap::new();
    for e in &base.edges {
        *importers.entry(e.from.as_str()).or_default() += 1;
    }
    let candidates: Vec<&str> = importers.iter().filter(|(_, n)| **n >= 3).map(|(f, _)| *f).collect();
    let stride = (candidates.len() / 10).max(1);
    let targets: Vec<&str> = (0..10).map(|i| candidates[(i * stride) % candidates.len()]).collect();
    let mut times = Vec::new();
    for (i, f) in targets.iter().enumerate() {
        let p = tmp.join(f);
        let mut text = std::fs::read_to_string(&p)?;
        text.push_str(&format!("\n// edit {i}\n"));
        std::fs::write(&p, text)?;
        let t = Instant::now();
        let out = ex.update(&clone, &ChangeSet { modified: vec![f.to_string()], removed: vec![] })?;
        times.push(ms(t.elapsed()));
        assert_eq!(out.edges.len(), base.edges.len());
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("single-file update (content edit): n={} median {:.2} ms, min {:.2}, max {:.2}", times.len(), times[times.len() / 2], times[0], times[times.len() - 1]);

    // Structural update: adding a file forces re-resolution of everything.
    let newf = "ripple_bench_added_file.ts".to_string();
    std::fs::write(tmp.join(&newf), "export const x = 1;\n")?;
    let mut files = ws.files.clone();
    files.push(newf.clone());
    files.sort();
    let clone2 = Workspace { files, ..clone.clone() };
    let t = Instant::now();
    ex.update(&clone2, &ChangeSet { modified: vec![newf], removed: vec![] })?;
    println!("add-one-file update (re-resolves everything): {:.2} ms", ms(t.elapsed()));
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = EdgeKind::JsImport;
    Ok(())
}
