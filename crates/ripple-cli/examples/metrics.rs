//! `cargo run --release -p ripple-cli --example metrics -- ROOT [TOP]`:
//! cold-indexes ROOT with the standard pipeline and prints the size/complexity
//! distribution from the published snapshot (`Graph::metrics`), per kind of
//! file, with the TOP (default 5) most complex files and Ruby methods.
//! Read-only: never writes to ROOT. A debugging aid, not part of the CLI.
//! With `METRICS_DUMP=path`, writes `path<TAB>lines<TAB>complexity<TAB>methods<TAB>max_method`
//! for every file, and `path:start-end<TAB>complexity` for every Ruby method.

use ripple_cli::engine::{Engine, Pipeline};
use ripple_core::FileMetrics;
use std::fmt::Write as _;
use std::time::Instant;

fn kind(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "rb" | "rake" | "ru" | "gemspec" => "ruby",
        "erb" | "haml" | "slim" | "builder" | "jbuilder" => "template",
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" | "vue" => "js",
        _ if path.ends_with("Gemfile") || path.ends_with("Rakefile") => "ruby",
        _ => "other",
    }
}

fn pct(sorted: &[u32], p: f64) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn main() -> anyhow::Result<()> {
    let root = std::path::PathBuf::from(std::env::args().nth(1).expect("usage: metrics ROOT [TOP]")).canonicalize()?;
    let top: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    let t = Instant::now();
    let (_engine, snap) = Engine::open(&root, Pipeline::standard())?;
    println!("indexed {} in {} ms", root.display(), t.elapsed().as_millis());
    let g = &snap.graph;
    for t in g.metrics_tables() {
        println!("table: {} files, per-method {}, {} heap bytes", t.len(), t.has_methods(), t.heap_bytes());
    }
    let t = Instant::now();
    let rows: Vec<(&str, FileMetrics)> = (0..g.node_count() as u32).filter_map(|n| Some((g.path(n), g.metrics(n)?))).collect();
    let per = t.elapsed().as_nanos() as f64 / g.node_count().max(1) as f64;
    println!("{} of {} nodes have metrics (lookup {per:.0} ns/node)", rows.len(), g.node_count());
    for k in ["ruby", "js", "template", "other"] {
        let mut sel: Vec<&(&str, FileMetrics)> = rows.iter().filter(|(p, _)| kind(p) == k).collect();
        if sel.is_empty() {
            continue;
        }
        let mut cx: Vec<u32> = sel.iter().map(|(_, m)| m.complexity).collect();
        cx.sort_unstable();
        let mut ln: Vec<u32> = sel.iter().map(|(_, m)| m.lines).collect();
        ln.sort_unstable();
        let methods: u64 = sel.iter().map(|(_, m)| u64::from(m.methods)).sum();
        println!(
            "\n[{k}] files {}  methods {methods}  complexity p50 {} p90 {} p99 {} max {}  lines p50 {} p90 {} max {}  total lines {}",
            sel.len(),
            pct(&cx, 0.5),
            pct(&cx, 0.9),
            pct(&cx, 0.99),
            cx.last().unwrap(),
            pct(&ln, 0.5),
            pct(&ln, 0.9),
            ln.last().unwrap(),
            ln.iter().map(|&l| u64::from(l)).sum::<u64>()
        );
        sel.sort_by(|a, b| b.1.complexity.cmp(&a.1.complexity).then(a.0.cmp(b.0)));
        for (p, m) in sel.iter().take(top) {
            println!("  {:>6} cx  {:>6} lines  {:>4} methods  max {:>4}  {p}", m.complexity, m.lines, m.methods, m.max_method);
        }
    }
    let mut methods: Vec<(u16, &str, u32, u32)> = Vec::new();
    for n in 0..g.node_count() as u32 {
        for m in g.method_metrics(n) {
            methods.push((m.complexity, g.path(n), m.start, m.end()));
        }
    }
    methods.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)).then(a.2.cmp(&b.2)));
    let mut mc: Vec<u32> = methods.iter().map(|m| u32::from(m.0)).collect();
    mc.sort_unstable();
    println!("\n[ruby methods] {}  complexity p50 {} p90 {} p99 {} max {}", mc.len(), pct(&mc, 0.5), pct(&mc, 0.9), pct(&mc, 0.99), mc.last().copied().unwrap_or(0));
    for (c, p, s, e) in methods.iter().take(top) {
        println!("  {c:>6} cx  {p}:{s}-{e}");
    }
    if let Some(out) = std::env::var_os("METRICS_DUMP") {
        let mut s = String::new();
        for (p, m) in &rows {
            let _ = writeln!(s, "{p}\t{}\t{}\t{}\t{}", m.lines, m.complexity, m.methods, m.max_method);
        }
        for (c, p, a, b) in &methods {
            let _ = writeln!(s, "{p}:{a}-{b}\t{c}");
        }
        std::fs::write(out, s)?;
    }
    Ok(())
}
