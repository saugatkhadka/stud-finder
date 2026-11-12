//! Soundness of incremental passes on real history: replays the last N
//! commits of a repository through one long-lived [`Engine`], one commit
//! per pass (every added, modified and deleted file of the commit), and
//! after every pass compares the published snapshot with a fresh
//! [`Engine::open`] of the same tree, with the incremental engine's
//! tombstones (edges kept for the former dependents of deleted files, which
//! a cold start cannot know) added to the fresh graph. Compared: the node set, every edge in
//! id order (endpoints, kind, confidence, line, detail, scopes, inherit),
//! both adjacency lists, fan-in/fan-out, per-node mentions, the unmentioned
//! set, per-node metrics, scope ranges and warnings. Stats are compared too
//! but only reported (reference-resolution counters can drift, see
//! `ripple-ruby`'s `top_level_fallback`), as are timings.
//!
//! The repository's working tree is moved to `HEAD~N` with `git checkout
//! --detach`, then forward one commit at a time, and put back on the
//! original branch / commit at the end (also on panic). Use a copy:
//!
//! ```text
//! RIPPLE_REPLAY_ROOT=/path/to/copy [RIPPLE_REPLAY_COMMITS=200] [RIPPLE_REPLAY_EVERY=1] \
//!   cargo test --release -p ripple-cli --test replay_equality -- --ignored --nocapture
//! ```
//!
//! Each compared snapshot is also written to disk and mapped back
//! ([`ripple_cli::snapfile`], what a sleeping daemon serves from); the mapped
//! one must answer every query exactly like the heap one (per-node queries
//! on about 2000 nodes).
//!
//! `RIPPLE_REPLAY_EVERY=k` compares with a fresh open only every k-th pass
//! (passes are still applied one commit at a time); use it on big repos.
//!
//! `RIPPLE_REPLAY_EDITS=N` instead keeps HEAD and applies N seeded random
//! edits (body-only statements and comments, new methods, constant
//! references, batches of 20), restoring each file afterwards.

use ripple_cli::engine::{Engine, Pipeline, Snapshot};
use ripple_cli::snapfile;
use ripple_core::Graph;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(root).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Puts the working tree back where it was.
struct Restore {
    root: PathBuf,
    to: String,
}

impl Drop for Restore {
    fn drop(&mut self) {
        let ok = git(&self.root, &["checkout", "-q", "-f", &self.to]).is_some();
        eprintln!("restored {} to {} ({})", self.root.display(), self.to, if ok { "ok" } else { "FAILED" });
    }
}

/// Everything a query can observe, as comparable strings; `g` is the graph
/// to describe (the snapshot's, or a reference built with tombstones).
fn digest(s: &Snapshot, g: &Graph, eng: &Engine) -> BTreeMap<&'static str, Vec<String>> {
    let mut d: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    d.insert("nodes", g.paths().map(String::from).collect::<Vec<_>>());
    d.insert(
        "edges",
        g.edges_for_test()
            .iter()
            .map(|e| format!("{} -> {} {:?} {:?} :{} {:?} {:?}/{:?} {}", e.from, e.to, e.kind, e.confidence, e.line, e.detail, e.from_scope, e.to_scope, e.inherit))
            .collect(),
    );
    let (fwd, rev) = g.adjacency_for_test();
    d.insert("forward", fwd.iter().map(|v| format!("{v:?}")).collect());
    d.insert("reverse", rev.iter().map(|v| format!("{v:?}")).collect());
    d.insert("fan", (0..g.node_count() as u32).map(|n| format!("{} {}", g.fan_in(n as _), g.fan_out(n as _))).collect());
    d.insert("mentions", (0..g.node_count() as u32).map(|n| format!("{:?}", g.mentions(n))).collect());
    d.insert("unmentioned", vec![format!("{:?}", eng.mention_table().map(|t| t.unmentioned.to_vec()))]);
    d.insert("metrics", (0..g.node_count() as u32).map(|n| format!("{:?} {:?}", g.metrics(n), g.method_metrics(n))).collect());
    d.insert("scopes", s.scopes.iter().map(|(f, v)| format!("{f} {v:?}")).collect());
    d.insert("warnings", s.warnings.iter().map(|w| format!("{} {:?} {}", w.code, w.file, w.message)).collect());
    d.insert("stats", stats(s));
    d
}

fn per_run(k: &str) -> bool {
    k.contains("_us") || k.contains("time") || k.ends_with("_this_run") || k.contains("parsed") || k.contains("shadow_docs") || k.contains("fell_back") || k.contains("rebuilt")
}

fn stats(s: &Snapshot) -> Vec<String> {
    s.stats.iter().flat_map(|(ex, m)| m.iter().filter(|(k, _)| !per_run(k)).map(move |(k, v)| format!("{ex}.{k}={v}"))).collect()
}

/// Compares; returns the names of the parts that differ (stats excluded).
fn compare(label: &str, inc: &BTreeMap<&'static str, Vec<String>>, fresh: &BTreeMap<&'static str, Vec<String>>) -> Vec<&'static str> {
    let mut bad = Vec::new();
    for (k, a) in fresh {
        let b = &inc[k];
        if a == b {
            continue;
        }
        let (sa, sb): (BTreeSet<&String>, BTreeSet<&String>) = (a.iter().collect(), b.iter().collect());
        let n = if *k == "stats" { 6 } else { 12 };
        for l in sa.difference(&sb).take(n) {
            eprintln!("  {label} {k} missing in incremental: {l}");
        }
        for l in sb.difference(&sa).take(n) {
            eprintln!("  {label} {k} extra in incremental:   {l}");
        }
        if sa == sb {
            eprintln!("  {label} {k}: same elements, different order");
        }
        if *k != "stats" {
            bad.push(*k);
        }
    }
    bad
}

struct Tally {
    passes: usize,
    compared: usize,
    patched: usize,
    ruby_fast: u64,
    ruby_slow: u64,
    ruby_rebuilt: u64,
    fast_only_passes: usize,
    stat_drift: usize,
    failures: Vec<String>,
    total_us: Vec<u64>,
}

impl Tally {
    fn record(&mut self, s: &Snapshot) {
        self.passes += 1;
        self.patched += usize::from(s.timings.contains_key("graph_patch_us"));
        let r = |k: &str| s.stats.get("ruby").and_then(|m| m.get(k)).copied().unwrap_or(0);
        self.ruby_fast += r("ruby_fast_files_this_run");
        self.ruby_slow += r("ruby_slow_files_this_run");
        self.ruby_rebuilt += r("ruby_slow_batch_rebuilt_this_run");
        if r("ruby_slow_files_this_run") == 0 {
            self.fast_only_passes += 1;
        }
        self.total_us.push(s.timings.get("total_us").copied().unwrap_or(0));
    }

    fn check(&mut self, label: &str, s: &Arc<Snapshot>, eng: &Engine, root: &Path) {
        let inc = digest(s, &s.graph, eng);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.snap");
        let t = Instant::now();
        let size = snapfile::save(s, &path).unwrap();
        let saved = t.elapsed();
        let loaded = Arc::new(snapfile::load(&path, root).unwrap());
        let diff = snapfile::differences(s, &loaded, (s.graph.node_count() / 2000).max(1));
        eprintln!("  {label} snapshot {:.1} MB, saved in {} ms, {} differences", size as f64 / 1e6, saved.as_millis(), diff.len());
        for d in diff.iter().take(12) {
            eprintln!("  {label} snapshot differs: {d}");
        }
        if !diff.is_empty() {
            self.failures.push(format!("{label}: mapped snapshot differs"));
        }
        let (fresh_eng, fresh) = Engine::open(root, Pipeline::standard()).unwrap();
        let mut files: Vec<&str> = fresh.files.iter().collect();
        files.sort_unstable();
        let mut reference = Graph::build(fresh_eng.output_edges().chain(eng.tombstone_edges()), files);
        if let Some(m) = fresh_eng.mention_table() {
            reference.set_mentions(&m);
        }
        reference.set_metrics(fresh.graph.metrics_tables().iter().cloned());
        let fresh_d = digest(&fresh, &reference, &fresh_eng);
        self.compared += 1;
        if inc["stats"] != fresh_d["stats"] {
            self.stat_drift += 1;
        }
        let bad = compare(label, &inc, &fresh_d);
        if !bad.is_empty() {
            eprintln!("COUNTEREXAMPLE {label}: {bad:?}");
            self.failures.push(format!("{label}: {bad:?}"));
        }
    }

    fn summary(&mut self, what: &str) {
        self.total_us.sort_unstable();
        let p = |q: f64| self.total_us.get(((self.total_us.len().max(1) - 1) as f64 * q) as usize).copied().unwrap_or(0) as f64 / 1000.0;
        eprintln!(
            "{what}: {} passes, {} compared with a fresh open, {} counterexamples; graph patched in {} passes; ruby files fast {} / slow {} (passes with no slow file: {}), batches rebuilt {}; stat drift in {} comparisons; pass total p50 {:.1} ms p90 {:.1} ms max {:.1} ms",
            self.passes,
            self.compared,
            self.failures.len(),
            self.patched,
            self.ruby_fast,
            self.ruby_slow,
            self.fast_only_passes,
            self.ruby_rebuilt,
            self.stat_drift,
            p(0.5),
            p(0.9),
            p(1.0)
        );
        for f in &self.failures {
            eprintln!("  {f}");
        }
    }
}

fn tally() -> Tally {
    Tally { passes: 0, compared: 0, patched: 0, ruby_fast: 0, ruby_slow: 0, ruby_rebuilt: 0, fast_only_passes: 0, stat_drift: 0, failures: Vec::new(), total_us: Vec::new() }
}

#[test]
#[ignore = "needs RIPPLE_REPLAY_ROOT (a git checkout it may modify); slow"]
fn replay_equals_rebuild() {
    let Some(root) = std::env::var_os("RIPPLE_REPLAY_ROOT") else {
        panic!("set RIPPLE_REPLAY_ROOT");
    };
    let root = Path::new(&root).canonicalize().unwrap();
    if let Some(n) = std::env::var("RIPPLE_REPLAY_EDITS").ok().and_then(|v| v.parse().ok()) {
        return random_edits(&root, n);
    }
    let n: usize = std::env::var("RIPPLE_REPLAY_COMMITS").ok().and_then(|v| v.parse().ok()).unwrap_or(200);
    let every: usize = std::env::var("RIPPLE_REPLAY_EVERY").ok().and_then(|v| v.parse().ok()).unwrap_or(1).max(1);
    assert!(git(&root, &["status", "--porcelain", "--untracked-files=no"]).unwrap().trim().is_empty(), "tracked files are modified; refusing to check out");
    let branch = git(&root, &["symbolic-ref", "-q", "--short", "HEAD"]).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let head = git(&root, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let commits: Vec<String> = git(&root, &["rev-list", "--first-parent", "--reverse", "-n", &n.to_string(), "HEAD"]).unwrap().lines().map(String::from).collect();
    let _restore = Restore { root: root.clone(), to: branch.unwrap_or(head) };
    let base = format!("{}^", commits[0]);
    git(&root, &["checkout", "-q", "--detach", &base]).expect("checkout base");
    let (mut eng, _) = Engine::open(&root, Pipeline::standard()).unwrap();
    let mut t = tally();
    let mut prev = base;
    for (i, c) in commits.iter().enumerate() {
        let changed: BTreeSet<String> = git(&root, &["diff", "--name-only", "--no-renames", &prev, c]).unwrap().lines().map(String::from).collect();
        git(&root, &["checkout", "-q", "--detach", c]).expect("checkout");
        prev = c.clone();
        let start = Instant::now();
        let s = eng.apply_paths(&changed).unwrap();
        let wall = start.elapsed().as_millis();
        t.record(&s);
        let r = |k: &str| s.stats.get("ruby").and_then(|m| m.get(k)).copied().unwrap_or(0);
        eprintln!(
            "{i:03} {} {} files: {wall} ms (ruby fast {} slow {}{}{})",
            &c[..10],
            changed.len(),
            r("ruby_fast_files_this_run"),
            r("ruby_slow_files_this_run"),
            if s.timings.contains_key("graph_patch_us") { ", graph patched" } else { "" },
            if r("ruby_slow_batch_rebuilt_this_run") > 0 { ", ruby rebuilt" } else { "" }
        );
        if (i + 1) % every == 0 || i + 1 == commits.len() {
            t.check(&format!("commit {i} {}", &c[..10]), &s, &eng, &root);
        }
    }
    t.summary(&format!("replay of {} commits of {}", commits.len(), root.display()));
    assert!(t.failures.is_empty(), "{} counterexamples", t.failures.len());
}

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

/// One random edit of `src`; `None` when it does not apply.
fn edit(rng: &mut Rng, src: &str, step: usize, konst: &str) -> Option<(String, &'static str)> {
    let lines: Vec<&str> = src.lines().collect();
    let defs: Vec<usize> = (0..lines.len()).filter(|&i| lines[i].trim_start().starts_with("def ") && !lines[i].trim_end().ends_with(" end")).collect();
    let insert = |at: usize, text: String| {
        let mut v: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        v.insert(at, text);
        v.join("\n") + "\n"
    };
    let indent = |i: usize| lines[i].len() - lines[i].trim_start().len();
    Some(match rng.below(6) {
        0 | 1 => {
            let at = *defs.get(rng.below(defs.len().max(1)))?;
            (insert(at + 1, format!("{}  ripple_tmp_{step} = [1, 2].sum", " ".repeat(indent(at)))), "body statement")
        }
        2 => (format!("{src}\n# ripple comment {step}\n"), "comment"),
        3 => {
            let at = *defs.get(rng.below(defs.len().max(1)))?;
            (insert(at, format!("{0}def ripple_new_{step}\n{0}  1\n{0}end\n", " ".repeat(indent(at)))), "new method")
        }
        4 => {
            let at = *defs.get(rng.below(defs.len().max(1)))?;
            (insert(at + 1, format!("{}  ::{konst}", " ".repeat(indent(at)))), "constant reference")
        }
        _ => {
            let at = *defs.get(rng.below(defs.len().max(1)))?;
            let end = (at + 1..lines.len()).find(|&j| lines[j].trim_end() == format!("{}end", " ".repeat(indent(at))))?;
            let kept: Vec<&str> = lines.iter().enumerate().filter(|(j, _)| *j < at || *j > end).map(|(_, l)| *l).collect();
            (kept.join("\n") + "\n", "remove method")
        }
    })
}

fn random_edits(root: &Path, n: usize) {
    let seed: u64 = std::env::var("RIPPLE_REPLAY_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let mut rng = Rng(0x9e3779b97f4a7c15 ^ seed.wrapping_mul(7919));
    // Edited files are restored after each step; this covers a panic.
    let _restore = Restore { root: root.to_path_buf(), to: "HEAD".into() };
    let (mut eng, snap) = Engine::open(root, Pipeline::standard()).unwrap();
    let mut rb: Vec<String> = snap.files.iter().filter(|f| f.ends_with(".rb") && (f.starts_with("app/") || f.starts_with("lib/") || f.starts_with("spec/"))).map(String::from).collect();
    rb.sort();
    let models: Vec<&String> = rb.iter().filter(|f| f.starts_with("app/models/") && !f.contains("concerns")).collect();
    let konst = |rng: &mut Rng| -> String {
        let f = models[rng.below(models.len())];
        f.trim_start_matches("app/models/").trim_end_matches(".rb").split('/').map(|s| s.split('_').map(|w| w[..1].to_uppercase() + &w[1..]).collect::<String>()).collect::<Vec<_>>().join("::")
    };
    drop(snap);
    let mut t = tally();
    for step in 0..n {
        let k = if step % 10 == 9 { 20 } else { 1 };
        let mut originals: BTreeMap<String, String> = BTreeMap::new();
        let mut what = Vec::new();
        while originals.len() < k {
            let f = rb[rng.below(rb.len())].clone();
            if originals.contains_key(&f) {
                continue;
            }
            let src = std::fs::read_to_string(root.join(&f)).unwrap_or_default();
            let c = konst(&mut rng);
            let Some((new, kind)) = edit(&mut rng, &src, step, &c) else { continue };
            std::fs::write(root.join(&f), new).unwrap();
            what.push(format!("{kind} in {f}"));
            originals.insert(f, src);
        }
        let paths: BTreeSet<String> = originals.keys().cloned().collect();
        let s = eng.apply_paths(&paths).unwrap();
        t.record(&s);
        eprintln!("{step:03} {} ms: {}{}", s.timings.get("total_us").copied().unwrap_or(0) / 1000, what.iter().take(3).cloned().collect::<Vec<_>>().join("; "), if what.len() > 3 { " ..." } else { "" });
        t.check(&format!("edit step {step}"), &s, &eng, root);
        for (f, src) in &originals {
            std::fs::write(root.join(f), src).unwrap();
        }
        let s = eng.apply_paths(&paths).unwrap();
        t.record(&s);
        if step % 5 == 4 {
            t.check(&format!("restore step {step}"), &s, &eng, root);
        }
    }
    t.summary(&format!("{n} random edit steps on {}", root.display()));
    assert!(t.failures.is_empty(), "{} counterexamples", t.failures.len());
}
