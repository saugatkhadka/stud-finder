//! Method-level measurement harness.
//!
//! ```text
//! cargo run --release -p ripple-ruby --example methods -- stats <repo> [--file-level]
//!       [--copy <dir>] [--updates n] [--edges-out <path>]
//!   edge counts by kind and by method category, scopes, denied methods,
//!   cold build (median of 3) and, with --copy, the median single-file update
//!   over n (default 10) different files of a copy of <repo> (only Ruby/template
//!   files are copied; the source repo is never written).
//!
//! cargo run --release -p ripple-ruby --example methods -- impact <repo> <file>
//!       [--n 15] [--seed 1] [--depth 3] [--methods a,b] [--explain] [--quiet]
//!   (a) whole-file impact of <file>, (b) impact of n random scoped methods of
//!   <file> (or --methods): files reached (depth <= --depth), min/median/max.
//!   --explain adds, per method: files at depth 1 of the file-level query that
//!   the scoped query drops (with the edges to <file>), a word-mention oracle
//!   (files containing the method name that depend on <file> at depth 1, and
//!   whether the scoped query reaches them), and the explanation path of up
//!   to 5 reached depth-1 files (for precision sampling).
//! ```
//!
//! Only Ruby edges exist in this graph; the mention filter uses the
//! extractor's mention table.

#[path = "support/mod.rs"]
mod support;

use ripple_core::{ChangeSet, Edge, Extractor, Graph, QueryOpts, Workspace};
use ripple_ruby::RubyExtractor;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;


fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned());
    let flag = |name: &str| args.iter().any(|a| a == name);
    match args.first().map(String::as_str) {
        Some("stats") => stats(Path::new(&args[1]), flag("--file-level"), opt("--copy"), opt("--updates").and_then(|s| s.parse().ok()).unwrap_or(10), opt("--edges-out")),
        Some("impact") => impact(
            Path::new(&args[1]),
            &args[2],
            opt("--n").and_then(|s| s.parse().ok()).unwrap_or(15),
            opt("--seed").and_then(|s| s.parse().ok()).unwrap_or(1),
            opt("--depth").and_then(|s| s.parse().ok()).unwrap_or(3),
            opt("--methods"),
            flag("--explain"),
            flag("--quiet"),
        ),
        _ => anyhow::bail!("usage: methods stats <repo> ... | methods impact <repo> <file> ..."),
    }
}

fn ws(root: &Path, file_level: bool) -> Workspace {
    let mut w = support::workspace(root);
    if file_level {
        w.config.ruby.method_level = false;
    }
    w
}

fn fmt(e: &Edge) -> String {
    format!(
        "{} -> {} {:?} {:?} :{} {:?}/{:?}{} {}",
        e.from,
        e.to,
        e.kind,
        e.confidence,
        e.line,
        e.from_scope,
        e.to_scope,
        if e.inherit { " inherit" } else { "" },
        e.detail.as_deref().unwrap_or("")
    )
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if v.is_empty() { 0.0 } else { v[v.len() / 2] }
}

const EXTS: &[&str] = &[".rb", ".rake", ".ru", ".gemspec", ".erb", ".haml", ".slim", ".jbuilder", ".builder"];

fn stats(root: &Path, file_level: bool, copy: Option<String>, updates: usize, edges_out: Option<String>) -> anyhow::Result<()> {
    let w = ws(root, file_level);
    let mut times = Vec::new();
    let mut out = None;
    for _ in 0..3 {
        let t = Instant::now();
        let o = RubyExtractor::new().build(&w)?;
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        out = Some(o);
    }
    let out = out.unwrap();
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for e in &out.edges {
        *by_kind.entry(format!("{:?}", e.kind)).or_default() += 1;
    }
    println!("repo {} file_level={file_level}", root.display());
    println!("cold build ms: median {:.1} (runs {:?})", median(times.clone()), times.iter().map(|t| format!("{t:.0}")).collect::<Vec<_>>());
    println!("edges {} {:?}", out.edges.len(), by_kind);
    println!("scoped files {} scope ranges {}", out.facts.scopes.len(), out.facts.scopes.values().map(Vec::len).sum::<usize>());
    for (k, v) in &out.stats {
        if k.starts_with("ruby_method_") || k.starts_with("ruby_mention_") || k.starts_with("ruby_time_") {
            println!("  {k} = {v}");
        }
    }
    if let Some(p) = edges_out {
        let mut lines: Vec<String> = out.edges.iter().map(fmt).collect();
        lines.sort();
        let scopes: Vec<String> = out.facts.scopes.iter().map(|(f, v)| format!("SCOPES {f} {}", v.iter().map(|r| format!("{}:{}-{}", r.name, r.start, r.end)).collect::<Vec<_>>().join(" "))).collect();
        lines.extend(scopes);
        std::fs::write(&p, lines.join("\n") + "\n")?;
        println!("wrote {p}");
    }
    let Some(dir) = copy else { return Ok(()) };
    let dir = PathBuf::from(dir);
    if !dir.exists() {
        for f in support::workspace(root).files.into_iter().filter(|f| f.ends_with("Rakefile") || EXTS.iter().any(|e| f.ends_with(e))) {
            let to = dir.join(&f);
            std::fs::create_dir_all(to.parent().unwrap())?;
            std::fs::copy(root.join(&f), to)?;
        }
    }
    let w = ws(&dir, file_level);
    let mut ex = RubyExtractor::new();
    ex.build(&w)?;
    let mut rng = support::Rng::new(42);
    let cands: Vec<&String> = w.files.iter().filter(|f| f.ends_with(".rb") && (f.starts_with("app/") || f.starts_with("lib/"))).collect();
    let mut picked: BTreeSet<&String> = BTreeSet::new();
    while picked.len() < updates.min(cands.len()) {
        picked.insert(cands[rng.below(cands.len())]);
    }
    let mut ut = Vec::new();
    let mut phases: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for f in picked {
        let p = dir.join(f);
        let orig = std::fs::read_to_string(&p)?;
        // A real edit inside the file: a new method call line after the first `def`.
        let edited = match orig.find("\n  def ").and_then(|i| orig[i + 1..].find('\n').map(|j| i + 1 + j)) {
            Some(at) => format!("{}\n    ripple_probe_call{}", &orig[..at], &orig[at..]),
            None => format!("{orig}\n# ripple\n"),
        };
        std::fs::write(&p, edited)?;
        let cs = ChangeSet { modified: vec![f.clone()], removed: vec![] };
        let t = Instant::now();
        let o = ex.update(&w, &cs)?;
        ut.push(t.elapsed().as_secs_f64() * 1000.0);
        for (k, v) in o.stats.iter().filter(|(k, _)| k.starts_with("ruby_time_")) {
            phases.entry(k.clone()).or_default().push(*v as f64 / 1000.0);
        }
        std::fs::write(&p, &orig)?;
        ex.update(&w, &cs)?;
    }
    println!("update ms: median {:.1} over {} files (runs {:?})", median(ut.clone()), ut.len(), ut.iter().map(|t| format!("{t:.0}")).collect::<Vec<_>>());
    for (k, v) in phases {
        println!("  update {k} median {:.1} ms", median(v));
    }
    Ok(())
}

fn word_in(text: &str, name: &str) -> bool {
    let b = text.as_bytes();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let tail_is_word = name.as_bytes().last().is_some_and(|c| ident(*c));
    let mut from = 0;
    while let Some(i) = text[from..].find(name) {
        let s = from + i;
        let e = s + name.len();
        let before = s == 0 || !ident(b[s - 1]);
        let after = !tail_is_word || e >= b.len() || !ident(b[e]);
        if before && after {
            return true;
        }
        from = s + 1;
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn impact(root: &Path, file: &str, n: usize, seed: u64, depth: u32, methods: Option<String>, explain: bool, quiet: bool) -> anyhow::Result<()> {
    let w = ws(root, false);
    let t = Instant::now();
    let out = RubyExtractor::new().build(&w)?;
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;
    let ranges = out.facts.scopes.get(file).cloned().unwrap_or_default();
    let mut g = Graph::build(&out.edges, w.files.iter().map(String::as_str));
    if let Some(m) = &out.facts.mentions {
        g.set_mentions(m);
    }
    let Some(id) = g.id(file) else { anyhow::bail!("{file} not in graph") };
    let opts = QueryOpts { max_depth: depth, ..Default::default() };
    let run = |scopes: Option<Vec<String>>, o: QueryOpts| -> BTreeMap<String, u32> {
        g.impact_scoped(&[(id, scopes)], o).hits.iter().skip(1).map(|h| (g.path(h.node).to_string(), h.depth)).collect()
    };
    let whole = run(None, opts);
    let fl = run(None, QueryOpts { file_level: true, ..opts });
    let d1 = |m: &BTreeMap<String, u32>| m.values().filter(|d| **d == 1).count();
    println!("{} {file}: build {build_ms:.0} ms, {} scope ranges", root.display(), ranges.len());
    println!("(a) whole file: {} files (depth1 {}), file_level opts: {} (depth1 {})", whole.len(), d1(&whole), fl.len(), d1(&fl));
    let mut picks: Vec<String> = match methods {
        Some(m) => m.split(',').map(String::from).collect(),
        None => {
            let mut rng = support::Rng::new(seed);
            let mut set = BTreeSet::new();
            while set.len() < n.min(ranges.len()) {
                set.insert(ranges[rng.below(ranges.len())].name.clone());
            }
            set.into_iter().collect()
        }
    };
    picks.sort();
    let mut counts = Vec::new();
    let mut texts: Option<BTreeMap<String, String>> = None;
    for m in &picks {
        let r = run(Some(vec![m.clone()]), opts);
        counts.push(r.len() as f64);
        println!("  #{m}: {} files (depth1 {})", r.len(), d1(&r));
        if !quiet && !explain {
            println!("     {:?}", r.keys().take(8).collect::<Vec<_>>());
        }
        if !explain {
            continue;
        }
        // Dropped: depth-1 dependents in the file-level answer not reached at all.
        let dropped: Vec<&String> = fl.iter().filter(|(f, d)| **d == 1 && !r.contains_key(*f)).map(|(f, _)| f).collect();
        println!("     dropped depth-1 files: {}", dropped.len());
        let mut rng = support::Rng::new(seed ^ 0x5eed);
        let mut sample: BTreeSet<&String> = BTreeSet::new();
        while sample.len() < dropped.len().min(10) {
            sample.insert(dropped[rng.below(dropped.len())]);
        }
        for f in sample {
            let es: Vec<String> = out.edges.iter().filter(|e| &e.from == f && e.to == file).map(|e| format!("L{} {:?}/{:?} {}", e.line, e.from_scope, e.to_scope, e.detail.as_deref().unwrap_or(""))).take(4).collect();
            println!("       drop {f}: {es:?}");
        }
        // Oracle: files mentioning the name (word match) among file-level depth-1 dependents.
        let texts = texts.get_or_insert_with(|| w.files.iter().filter_map(|f| std::fs::read_to_string(w.root.join(f)).ok().map(|t| (f.clone(), t))).collect());
        let mentions: Vec<&String> = texts.iter().filter(|(f, t)| f.as_str() != file && word_in(t, m)).map(|(f, _)| f).collect();
        let dep_mentions: Vec<&&String> = mentions.iter().filter(|f| fl.get(f.as_str()) == Some(&1)).collect();
        let missed: Vec<&&&String> = dep_mentions.iter().filter(|f| !r.contains_key(f.as_str())).collect();
        let name_hit = mentions.iter().filter(|f| r.contains_key(f.as_str())).count();
        println!(
            "     oracle: {} files mention #{m}; {} of them are depth-1 dependents, scoped misses {}: {:?}; scoped reaches {} mentioning files",
            mentions.len(),
            dep_mentions.len(),
            missed.len(),
            missed.iter().take(10).collect::<Vec<_>>(),
            name_hit
        );
        // Precision sample: explanation for up to 5 depth-1 hits.
        let imp = g.impact_scoped(&[(id, Some(vec![m.clone()]))], opts);
        let hits: Vec<_> = imp.hits.iter().filter(|h| h.depth == 1).collect();
        let mut rng = support::Rng::new(seed ^ 0x9e3);
        let mut ps: BTreeSet<usize> = BTreeSet::new();
        while ps.len() < hits.len().min(5) {
            ps.insert(rng.below(hits.len()));
        }
        for i in ps {
            let h = hits[i];
            let path = imp.path(h.node);
            let steps: Vec<String> = path
                .iter()
                .filter(|(_, e)| *e != u32::MAX)
                .map(|(node, e)| {
                    let (from, to, d) = g.edge(*e);
                    let (fs, ts) = g.edge_scopes(*e);
                    let src = std::fs::read_to_string(w.root.join(g.path(from))).unwrap_or_default();
                    let line = src.lines().nth(d.line.saturating_sub(1) as usize).unwrap_or("").trim();
                    let _ = node;
                    format!("{}:{} [{:?}->{:?}] -> {} `{}` ({})", g.path(from), d.line, fs, ts, g.path(to), &line[..line.len().min(90)], g.edge_detail(*e).unwrap_or(""))
                })
                .collect();
            println!("       hit {}: {}", g.path(h.node), steps.join(" | "));
        }
    }
    if !counts.is_empty() {
        let mut s = counts.clone();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!("(b) {} methods: min {} median {} max {} (whole file {})", s.len(), s[0], median(counts), s[s.len() - 1], whole.len());
    }
    Ok(())
}
