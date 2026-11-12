//! `ripple bench ...` (hidden): synthetic graphs and latency measurements.
//!
//! These exist so the numbers in the README can be reproduced; see the
//! README's "Performance" section for the exact invocations.

use crate::client::Client;
use crate::engine::{Engine, Pipeline};
use crate::files;
use crate::paths::DaemonPaths;
use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde_json::json;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

#[derive(Args)]
pub struct BenchArgs {
    #[command(subcommand)]
    cmd: BenchCmd,
}

#[derive(Subcommand)]
enum BenchCmd {
    /// Write a synthetic, skewed, acyclic edge file over the repo's files.
    GenEdges {
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 400_000)]
        edges: usize,
        #[arg(long, default_value_t = 42)]
        seed: u64,
        /// Skew exponent: higher concentrates more edges on a few hubs.
        #[arg(long, default_value_t = 2.5)]
        skew: f64,
    },
    /// Time the file listing.
    ListFiles {
        #[arg(long, default_value_t = 10)]
        runs: usize,
    },
    /// Cold in-process build; prints per-phase timings.
    Build,
    /// Query a running daemon N times per seed class; reports server_us and round trip.
    Server {
        /// The edge file used by the daemon (to pick small and hub seeds by fan-in).
        #[arg(long)]
        edges: PathBuf,
        #[arg(long, default_value_t = 1000)]
        queries: usize,
        /// Passed as `limit` (0 = full report).
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// In-process breakdown for one seed: BFS, policy evaluation, JSON rendering.
    Query {
        #[arg(long)]
        file: String,
        #[arg(long, default_value_t = 50)]
        runs: usize,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Spawn `ripple impact --files FILE --format json` N times; wall-clock percentiles.
    E2e {
        #[arg(long)]
        file: String,
        #[arg(long, default_value_t = 200)]
        runs: usize,
        #[arg(long)]
        limit: Option<usize>,
    },
}

pub fn run(root: &Path, a: BenchArgs) -> Result<u8> {
    match a.cmd {
        BenchCmd::GenEdges { out, edges, seed, skew } => gen_edges(root, &out, edges, seed, skew),
        BenchCmd::ListFiles { runs } => list_files(root, runs),
        BenchCmd::Build => build(root),
        BenchCmd::Server { edges, queries, limit } => server(root, &edges, queries, limit),
        BenchCmd::E2e { file, runs, limit } => e2e(root, &file, runs, limit),
        BenchCmd::Query { file, runs, limit } => query(root, &file, runs, limit),
    }
}

/// splitmix64
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

fn summary(label: &str, mut v: Vec<u64>, unit: &str) -> String {
    v.sort_unstable();
    let mean = v.iter().sum::<u64>() as f64 / v.len().max(1) as f64;
    format!(
        "{label}: n={} p50={}{unit} p90={}{unit} p99={}{unit} max={}{unit} mean={mean:.0}{unit}",
        v.len(),
        pct(&v, 0.5),
        pct(&v, 0.9),
        pct(&v, 0.99),
        v.last().copied().unwrap_or(0)
    )
}

fn workspace_files(root: &Path) -> Result<Vec<String>> {
    let policy = ripple_policy::Policy::load(root).unwrap_or_else(|_| ripple_policy::Policy::compile(Default::default()).expect("defaults"));
    Ok(files::list_files(root, &policy)?.0)
}

/// Layered DAG: every file gets a random rank; an edge goes from a file of
/// rank r to one of rank floor(r * u^skew), so low ranks become hubs and
/// there are no cycles.
fn gen_edges(root: &Path, out: &Path, n: usize, seed: u64, skew: f64) -> Result<u8> {
    let files = workspace_files(root)?;
    if files.len() < 2 {
        bail!("need at least two files");
    }
    let mut rng = Rng(seed);
    let mut by_rank = files.clone();
    for i in (1..by_rank.len()).rev() {
        let j = rng.below(i + 1);
        by_rank.swap(i, j);
    }
    let mut w = std::io::BufWriter::new(std::fs::File::create(out).with_context(|| out.display().to_string())?);
    writeln!(w, "# synthetic: {n} edges over {} files, seed {seed}, skew {skew}", files.len())?;
    let mut written = 0;
    while written < n {
        let r_from = 1 + rng.below(by_rank.len() - 1);
        let r_to = ((r_from as f64) * rng.unit().powf(skew)) as usize;
        if r_to >= r_from {
            continue;
        }
        writeln!(w, "{} {}", by_rank[r_from], by_rank[r_to])?;
        written += 1;
    }
    w.flush()?;
    println!("wrote {written} edges over {} files to {}", files.len(), out.display());
    Ok(0)
}

fn list_files(root: &Path, runs: usize) -> Result<u8> {
    let policy = ripple_policy::Policy::load(root).unwrap_or_else(|_| ripple_policy::Policy::compile(Default::default()).expect("defaults"));
    let mut times = Vec::new();
    let mut count = 0;
    let mut how = "";
    for _ in 0..runs {
        let t = Instant::now();
        let (f, l) = files::list_files(root, &policy)?;
        times.push(t.elapsed().as_micros() as u64);
        count = f.len();
        how = l.as_str();
    }
    println!("{count} files via {how}");
    println!("{}", summary("list_files", times, "us"));
    Ok(0)
}

fn build(root: &Path) -> Result<u8> {
    let t = Instant::now();
    let (_, snap) = Engine::open(root, Pipeline::standard())?;
    println!("cold build {} ms: {} nodes, {} edges, {} files", t.elapsed().as_millis(), snap.graph.node_count(), snap.graph.edge_count(), snap.files.len());
    for (k, v) in &snap.timings {
        println!("  {k:<24} {:>10.1} ms", *v as f64 / 1000.0);
    }
    Ok(0)
}

fn server(root: &Path, edges: &Path, queries: usize, limit: usize) -> Result<u8> {
    let text = std::fs::read_to_string(edges)?;
    let mut fan_in: HashMap<&str, usize> = HashMap::new();
    for l in text.lines().filter(|l| !l.starts_with('#')) {
        let mut it = l.split_whitespace();
        if let (Some(_), Some(to)) = (it.next(), it.next()) {
            *fan_in.entry(to).or_default() += 1;
        }
    }
    let mut ranked: Vec<(&str, usize)> = fan_in.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let hubs: Vec<&str> = ranked.iter().take(20).map(|x| x.0).collect();
    // "Small": files with fan-in 1..=3.
    let small: Vec<&str> = ranked.iter().filter(|x| (1..=3).contains(&x.1)).map(|x| x.0).take(500).collect();
    if small.is_empty() || hubs.is_empty() {
        bail!("edge file has no usable seeds");
    }
    let paths = DaemonPaths::for_root(root)?;
    let mut c = Client::connect(&paths).context("no daemon running for this root (start one with `ripple daemon start`)")?;
    let lim = (limit > 0).then_some(limit);
    // Warm-up: waits for the initial index if the daemon was just started.
    c.call("impact", &json!({"files": [small[0]], "format": "json"}))?;
    for (label, seeds) in [("small", &small), ("hub", &hubs)] {
        let (mut srv, mut rtt, mut impacted) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..queries {
            let f = seeds[i % seeds.len()];
            let t = Instant::now();
            let r = c.call("impact", &json!({"files": [f], "format": "json", "limit": lim}))?;
            rtt.push(t.elapsed().as_micros() as u64);
            srv.push(r.server_us);
            let body = r.result.map_err(|e| anyhow::anyhow!(e.message))?;
            if i < seeds.len() {
                let v: serde_json::Value = serde_json::from_str(&body)?;
                impacted.push(v["counts"]["impacted"].as_u64().unwrap_or(0));
            }
        }
        impacted.sort_unstable();
        println!(
            "{label} seeds ({}; impacted files p50={} max={}; limit={}):",
            seeds.len(),
            pct(&impacted, 0.5),
            impacted.last().copied().unwrap_or(0),
            lim.map_or("none".to_string(), |l| l.to_string())
        );
        println!("  {}", summary("server_us", srv, "us"));
        println!("  {}", summary("round_trip", rtt, "us"));
    }
    Ok(0)
}

fn e2e(root: &Path, file: &str, runs: usize, limit: Option<usize>) -> Result<u8> {
    let exe = std::env::current_exe()?;
    let mut times = Vec::new();
    let mut bytes = 0;
    for _ in 0..runs {
        let mut cmd = Command::new(&exe);
        cmd.arg("--root").arg(root).args(["impact", "--format", "json", "--files", file]);
        if let Some(l) = limit {
            cmd.arg("--limit").arg(l.to_string());
        }
        let t = Instant::now();
        let out = cmd.stderr(Stdio::inherit()).output()?;
        times.push(t.elapsed().as_micros() as u64);
        if !out.status.success() {
            bail!("ripple impact failed: {}", out.status);
        }
        bytes = out.stdout.len();
    }
    println!("e2e `ripple impact --files {file} --format json` ({bytes} bytes of output):");
    println!("  {}", summary("wall", times, "us"));
    Ok(0)
}

fn query(root: &Path, file: &str, runs: usize, limit: Option<usize>) -> Result<u8> {
    use ripple_core::{Confidence, QueryOpts};
    let (_, snap) = Engine::open(root, Pipeline::standard())?;
    let node = snap.graph.id(file).context("file not in graph")?;
    let (mut bfs, mut eval, mut json) = (Vec::new(), Vec::new(), Vec::new());
    let mut hits = 0;
    let mut bytes = 0;
    for _ in 0..runs {
        let t = Instant::now();
        let imp = snap.graph.impact(&[node], QueryOpts { max_depth: 0, min_confidence: Confidence::Heuristic, file_level: false });
        bfs.push(t.elapsed().as_micros() as u64);
        hits = imp.hits.len();
        let changed = vec![ripple_policy::ChangedFile::modified(file)];
        let opts = ripple_policy::EvalOptions { limit, ..Default::default() };
        let t = Instant::now();
        let report = ripple_policy::evaluate_with(&snap.policy, &snap.graph, &snap.index, &changed, &opts);
        eval.push(t.elapsed().as_micros() as u64);
        let t = Instant::now();
        let s = ripple_policy::render::json::render(&report, false);
        json.push(t.elapsed().as_micros() as u64);
        bytes = s.len();
    }
    println!("{file}: {hits} hits, {bytes} bytes of JSON");
    println!("  {}", summary("graph.impact", bfs, "us"));
    println!("  {}", summary("evaluate_with", eval, "us"));
    println!("  {}", summary("render json", json, "us"));
    Ok(0)
}
