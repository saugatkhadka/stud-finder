//! Real-repo sanity tool.
//!
//! ```text
//! cargo run --release -p ripple-ruby --example dump -- <repo> [options]
//!   --dependents <file>     direct dependents of <file> (edges whose `to` is it)
//!   --deps <file>           direct dependencies of <file>
//!   --sample <n> [seed]     n random edges with the source line, for precision checks
//!   --update <file> [n]     time n single-file updates (default 10); <file> is
//!                           rewritten with an extra comment line, so use a copy
//!   --determinism           build twice and compare sorted edge sets
//!   --grep <text>           print every edge whose debug form contains <text>
//!   --warnings              print warnings
//!   --stats                 print all stats
//!   --scope-sample <n> [seed]  n random scoped and n random unscoped edges
//!                           from controllers, with the enclosing `def` line
//! ```
//!
//! Edges carrying a `from_scope` print it as `@action`.

#[path = "support/mod.rs"]
mod support;

use ripple_core::{ChangeSet, Edge, Extractor};
use ripple_ruby::RubyExtractor;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(root) = args.first().map(PathBuf::from) else {
        anyhow::bail!(
            "usage: dump <repo> [--dependents f] [--deps f] [--sample n [seed]] [--update f [n]] [--determinism] [--warnings] [--stats]"
        );
    };
    let opt = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .map(|i| args.get(i + 1).cloned().unwrap_or_default())
    };
    let opt2 = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 2).filter(|s| !s.starts_with("--")).cloned())
    };
    let flag = |name: &str| args.iter().any(|a| a == name);

    let ws = support::workspace(&root);
    let mut ex = RubyExtractor::new();
    let t = Instant::now();
    let out = ex.build(&ws)?;
    let cold = t.elapsed();

    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for e in &out.edges {
        *by_kind.entry(format!("{:?}", e.kind)).or_default() += 1;
    }
    println!("repo: {}", root.display());
    println!("workspace files: {}", ws.files.len());
    println!("cold build: {:.1} ms", cold.as_secs_f64() * 1000.0);
    println!("edges: {} {:?}", out.edges.len(), by_kind);
    println!("warnings: {}", out.warnings.len());
    let scoped: Vec<&Edge> = out.edges.iter().filter(|e| e.from_scope.is_some()).collect();
    println!(
        "scoped edges: {} from {} files",
        scoped.len(),
        scoped
            .iter()
            .map(|e| &e.from)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    );
    let keys = [
        "ruby_files_indexed",
        "ruby_templates_scanned",
        "ruby_refs_total",
        "ruby_refs_resolved",
        "ruby_refs_unresolved",
        "ruby_refs_namespace_skipped",
        "ruby_time_analyse_us",
        "ruby_time_merge_us",
        "ruby_time_resolve_us",
        "ruby_time_edges_us",
    ];
    for (k, v) in &out.stats {
        if flag("--stats") || keys.contains(&k.as_str()) {
            println!("  {k} = {v}");
        }
    }
    if flag("--warnings") {
        for w in &out.warnings {
            println!(
                "  warning {} {}: {}",
                w.code,
                w.file.as_deref().unwrap_or("-"),
                w.message
            );
        }
    }

    if let Some(f) = opt("--dependents") {
        let mut deps: Vec<&Edge> = out.edges.iter().filter(|e| e.to == f).collect();
        deps.sort_by(|a, b| (&a.from, a.kind).cmp(&(&b.from, b.kind)));
        println!(
            "direct dependents of {f}: {} edges, {} files",
            deps.len(),
            deps.iter()
                .map(|e| &e.from)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        );
        for e in deps {
            println!(
                "  {}:{} {:?}{} {}",
                e.from,
                e.line,
                e.kind,
                scope(e),
                e.detail.as_deref().unwrap_or("")
            );
        }
    }
    if let Some(pat) = opt("--grep") {
        for e in support::sorted(out.edges.clone()) {
            let line = format!(
                "{}:{} -> {} {:?} {:?}{} {}",
                e.from,
                e.line,
                e.to,
                e.kind,
                e.confidence,
                scope(&e),
                e.detail.as_deref().unwrap_or("")
            );
            if line.contains(&pat) {
                println!("  {line}");
            }
        }
    }
    if let Some(f) = opt("--deps") {
        let mut deps: Vec<&Edge> = out.edges.iter().filter(|e| e.from == f).collect();
        deps.sort_by(|a, b| (a.line, &a.to).cmp(&(b.line, &b.to)));
        println!("direct dependencies of {f}: {}", deps.len());
        for e in deps {
            println!(
                "  :{} -> {} {:?} {:?}{} {}",
                e.line,
                e.to,
                e.kind,
                e.confidence,
                scope(e),
                e.detail.as_deref().unwrap_or("")
            );
        }
    }
    if let Some(n) = opt("--sample") {
        let n: usize = n.parse().unwrap_or(40);
        let seed: u64 = opt2("--sample").and_then(|s| s.parse().ok()).unwrap_or(42);
        let edges = support::sorted(out.edges.clone());
        let mut rng = support::Rng::new(seed);
        println!("sample of {n} edges (seed {seed}):");
        for i in 0..n {
            let e = &edges[rng.below(edges.len())];
            let text = std::fs::read_to_string(root.join(&e.from))
                .ok()
                .and_then(|s| {
                    s.lines()
                        .nth(e.line.saturating_sub(1) as usize)
                        .map(|l| l.trim().chars().take(140).collect::<String>())
                })
                .unwrap_or_default();
            println!(
                "[{i:02}] {}:{} -> {} {:?}/{:?} {}\n      | {}",
                e.from,
                e.line,
                e.to,
                e.kind,
                e.confidence,
                e.detail.as_deref().unwrap_or(""),
                if e.line == 0 {
                    "(convention)".into()
                } else {
                    text
                }
            );
        }
    }
    if let Some(n) = opt("--scope-sample") {
        let n: usize = n.parse().unwrap_or(15);
        let seed: u64 = opt2("--scope-sample").and_then(|s| s.parse().ok()).unwrap_or(42);
        let mut rng = support::Rng::new(seed);
        let edges = support::sorted(out.edges.clone());
        let ctrl = |e: &&Edge| e.from.contains("app/controllers/") && !e.from.contains("/concerns/");
        let scoped: Vec<&Edge> = edges.iter().filter(ctrl).filter(|e| e.from_scope.is_some() && e.line > 0).collect();
        let unscoped: Vec<&Edge> = edges.iter().filter(ctrl).filter(|e| e.from_scope.is_none() && e.line > 0).collect();
        for (label, set) in [("scoped", scoped), ("unscoped", unscoped)] {
            println!("{label} controller edges: {} (sample {n}, seed {seed})", set.len());
            for i in 0..n.min(set.len()) {
                let e = set[rng.below(set.len())];
                let src = std::fs::read_to_string(root.join(&e.from)).unwrap_or_default();
                let lines: Vec<&str> = src.lines().collect();
                let at = e.line.saturating_sub(1) as usize;
                // Nearest preceding `def`/`private`/`class` line, as a hint for the reviewer.
                let ctx = (0..=at.min(lines.len().saturating_sub(1)))
                    .rev()
                    .find(|&j| {
                        let t = lines[j].trim_start();
                        t.starts_with("def ") || t.starts_with("private") || t.starts_with("protected") || t.starts_with("class ")
                    })
                    .map(|j| format!("L{} {}", j + 1, lines[j].trim()))
                    .unwrap_or_default();
                println!(
                    "[{i:02}] {}:{} -> {} {:?}{}\n      | {}\n      ^ {}",
                    e.from,
                    e.line,
                    e.to,
                    e.kind,
                    scope(e),
                    lines.get(at).map(|l| l.trim()).unwrap_or(""),
                    ctx
                );
            }
        }
    }
    if flag("--determinism") {
        let again = RubyExtractor::new().build(&ws)?;
        let same = support::sorted(out.edges.clone()) == support::sorted(again.edges);
        println!("determinism: two builds identical = {same}");
    }
    if flag("--reindex-all") {
        // Re-feeds every file through `update` on the warm graph: a no-op
        // edit, so the result should equal the cold build.
        let mut warm = RubyExtractor::new();
        warm.build(&ws)?;
        let t = Instant::now();
        let again = warm.update(
            &ws,
            &ChangeSet {
                modified: ws.files.clone(),
                removed: vec![],
            },
        )?;
        let same = support::sorted(out.edges.clone()) == support::sorted(again.edges.clone());
        println!(
            "reindex-all: {:.1} ms, equal to cold build = {same}, {} -> {} edges",
            t.elapsed().as_secs_f64() * 1000.0,
            out.edges.len(),
            again.edges.len()
        );
        for line in support::diff(&out.edges, &again.edges, 40) {
            println!("    {line}");
        }
    }
    if let Some(f) = opt("--update") {
        let n: usize = opt2("--update").and_then(|s| s.parse().ok()).unwrap_or(10);
        let path = root.join(&f);
        let original = std::fs::read_to_string(&path)?;
        let mut times = Vec::new();
        for i in 0..n {
            std::fs::write(&path, format!("{original}\n# ripple update bench {i}\n"))?;
            let t = Instant::now();
            let o = ex.update(
                &ws,
                &ChangeSet {
                    modified: vec![f.clone()],
                    removed: vec![],
                },
            )?;
            times.push(t.elapsed().as_secs_f64() * 1000.0);
            if i == 0 {
                println!(
                    "  first update: analyse {} us, merge {} us, resolve {} us, repair {} us, edges {} us",
                    o.stats["ruby_time_analyse_us"],
                    o.stats["ruby_time_merge_us"],
                    o.stats["ruby_time_resolve_us"],
                    o.stats
                        .get("ruby_time_shadow_repair_us")
                        .copied()
                        .unwrap_or(0),
                    o.stats["ruby_time_edges_us"]
                );
            }
        }
        std::fs::write(&path, original)?;
        let check = ex.update(
            &ws,
            &ChangeSet {
                modified: vec![f.clone()],
                removed: vec![],
            },
        )?;
        let same = support::sorted(check.edges.clone()) == support::sorted(out.edges.clone());
        for line in support::diff(&out.edges, &check.edges, 30) {
            println!("    {line}");
        }
        times.sort_by(f64::total_cmp);
        println!(
            "update {f}: median {:.1} ms (min {:.1}, max {:.1}) over {n}; restored == cold build: {same}",
            times[n / 2],
            times[0],
            times[n - 1]
        );
    }
    Ok(())
}

fn scope(e: &Edge) -> String {
    e.from_scope
        .as_deref()
        .map(|s| format!(" @{s}"))
        .unwrap_or_default()
}
