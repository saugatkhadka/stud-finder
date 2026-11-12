//! Replays real single-file edits from git history through the engine to
//! measure how often an edit is *local* ("body-only"), and to check the
//! rule a fast path would rely on:
//!
//! > If an edit leaves the edited file's own outgoing facts unchanged —
//! > the targets of its edges (kind, confidence, detail, `to_scope`,
//! > `inherit`; not lines or `from_scope`) and its method definitions (names,
//! > in order) — then no other file's edges, mentions or warnings change.
//! > Scope ranges and the workspace-wide "unmentioned" set may still change
//! > (a method of another file can become (un)mentioned), so a fast path
//! > must recompute those (`scoped::compute`, no Rubydex resolve).
//!
//! Each edit (parent -> commit version of one modified file) is applied on
//! top of HEAD with the real pipeline, compared with the index before it,
//! and the file is restored. Modifies the repo while it runs (restores every
//! file it touches); use a copy:
//!
//! ```text
//! RIPPLE_REPLAY_ROOT=/path/to/copy [RIPPLE_REPLAY_COMMITS=200] [RIPPLE_REPLAY_EXT=.rb] \
//!   cargo test --release -p ripple-cli --test replay -- --ignored --nocapture
//! ```

use ripple_cli::engine::{Engine, Pipeline, Snapshot};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

fn git(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git").arg("-C").arg(root).args(args).output().ok()?;
    out.status.success().then_some(out.stdout)
}

type EdgeKey = (String, String, String, String, Option<String>, Option<String>, bool);

/// Every edge as a comparable tuple; for edges from `file`, without line and `from_scope`.
fn edges(s: &Snapshot, file: &str) -> (BTreeMap<EdgeKey, usize>, BTreeMap<EdgeKey, usize>) {
    let g = &s.graph;
    let mut own = BTreeMap::new();
    let mut rest = BTreeMap::new();
    for e in 0..g.edge_count() as u32 {
        let (f, t, d) = g.edge(e);
        let (fs, ts) = g.edge_scopes(e);
        let from = g.path(f).to_string();
        let mine = from == file;
        let line = if mine { String::new() } else { d.line.to_string() };
        let key = (from, g.path(t).to_string(), format!("{:?}/{:?}/{line}", d.kind, d.confidence), g.edge_detail(e).unwrap_or("").to_string(), if mine { None } else { fs.map(str::to_string) }, ts.map(str::to_string), g.edge_inherits(e));
        *(if mine { &mut own } else { &mut rest }).entry(key).or_insert(0) += 1;
    }
    (own, rest)
}

struct View {
    own_edges: BTreeMap<EdgeKey, usize>,
    own_scope_names: Vec<String>,
    own_defs: Option<Vec<u32>>,
    rest_edges: BTreeMap<EdgeKey, usize>,
    rest_scopes: usize,
    rest_mentions: u64,
    unmentioned: Option<Vec<u32>>,
    warnings_rest: Vec<String>,
}

fn view(s: &Snapshot, eng: &Engine, file: &str) -> View {
    let (own_edges, rest_edges) = edges(s, file);
    let mut own_scope_names: Vec<String> = s.scopes.get(file).map(|v| v.iter().map(|r| r.name.clone()).collect()).unwrap_or_default();
    own_scope_names.sort();
    let g = &s.graph;
    let own_defs = g.id(file).and_then(|n| g.mentions(n)).map(|m| m.defs.to_vec());
    // Fingerprints of everything else (hash-like digests keep memory small).
    let mut h: u64 = 0;
    for n in 0..g.node_count() as u32 {
        if g.path(n) != file
            && let Some(m) = g.mentions(n)
        {
            h = h.wrapping_mul(31).wrapping_add(fx(&format!("{n}{:?}{:?}{:?}{:?}{}{}", m.pairs, m.defs, m.implicit, m.self_dynamic, m.opaque, m.controller)));
        }
    }
    let rest_scopes = s.scopes.iter().filter(|(f, _)| *f != file).map(|(f, v)| fx(&format!("{f}{v:?}")) as usize).fold(0usize, |a, b| a.wrapping_mul(31).wrapping_add(b));
    let unmentioned = eng.mention_table().map(|t| t.unmentioned.to_vec());
    let warnings_rest = s.warnings.iter().filter(|w| w.file.as_deref() != Some(file)).map(|w| format!("{}:{:?}:{}", w.code, w.file, w.message)).collect();
    View { own_edges, own_scope_names, own_defs, rest_edges, rest_scopes, rest_mentions: h, unmentioned, warnings_rest }
}

fn fx(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[test]
#[ignore]
fn replay_body_only_edits() {
    let Some(root) = std::env::var_os("RIPPLE_REPLAY_ROOT") else {
        eprintln!("RIPPLE_REPLAY_ROOT not set; skipping");
        return;
    };
    let root = Path::new(&root).canonicalize().unwrap();
    let commits: usize = std::env::var("RIPPLE_REPLAY_COMMITS").ok().and_then(|v| v.parse().ok()).unwrap_or(200);
    let ext = std::env::var("RIPPLE_REPLAY_EXT").unwrap_or_else(|_| ".rb".into());
    let log = String::from_utf8(git(&root, &["log", "--no-merges", "--format=%H", "-n", &commits.to_string()]).unwrap()).unwrap();
    let mut edits: Vec<(String, String)> = Vec::new();
    for c in log.lines() {
        let out = String::from_utf8(git(&root, &["diff-tree", "--no-commit-id", "-r", "--name-status", "--no-renames", &format!("{c}^"), c]).unwrap_or_default()).unwrap();
        for l in out.lines() {
            if let Some(f) = l.strip_prefix("M\t")
                && f.ends_with(ext.as_str())
                && root.join(f).is_file()
            {
                edits.push((c.to_string(), f.to_string()));
            }
        }
    }
    eprintln!("{} edits of {ext} files from the last {commits} commits", edits.len());
    let (mut eng, _) = Engine::open(&root, Pipeline::standard()).unwrap();
    let (mut local, mut structural, mut violations, mut skipped, mut scope_flips) = (0, 0, 0, 0, 0);
    let mut local_us: Vec<u64> = Vec::new();
    let mut ruby_breakdown: BTreeMap<String, u64> = BTreeMap::new();
    for (c, f) in &edits {
        let head = std::fs::read(root.join(f)).unwrap();
        let (Some(old), Some(new)) = (git(&root, &["show", &format!("{c}^:{f}")]), git(&root, &["show", &format!("{c}:{f}")])) else {
            skipped += 1;
            continue;
        };
        let one = BTreeSet::from([f.clone()]);
        std::fs::write(root.join(f), &old).unwrap();
        let a = eng.apply_paths(&one).unwrap();
        let va = view(&a, &eng, f);
        std::fs::write(root.join(f), &new).unwrap();
        let b = eng.apply_paths(&one).unwrap();
        let vb = view(&b, &eng, f);
        std::fs::write(root.join(f), &head).unwrap();
        eng.apply_paths(&one).unwrap();

        let own_same = va.own_edges == vb.own_edges && va.own_defs == vb.own_defs;
        let rest_same = va.rest_edges == vb.rest_edges && va.rest_mentions == vb.rest_mentions && va.warnings_rest == vb.warnings_rest;
        if own_same {
            if va.unmentioned != vb.unmentioned || va.rest_scopes != vb.rest_scopes || va.own_scope_names != vb.own_scope_names {
                scope_flips += 1;
            }
            local += 1;
            local_us.push(b.timings.get("total_us").copied().unwrap_or(0));
            for (k, v) in b.stats.get("ruby").into_iter().flatten() {
                if k.starts_with("ruby_time_") {
                    *ruby_breakdown.entry(k.clone()).or_default() += v;
                }
            }
            if !rest_same {
                violations += 1;
                eprintln!("VIOLATION {c} {f}: rest edges same {} mentions {} warnings {}", va.rest_edges == vb.rest_edges, va.rest_mentions == vb.rest_mentions, va.warnings_rest == vb.warnings_rest);
            }
        } else {
            structural += 1;
        }
    }
    local_us.sort_unstable();
    let p = |q: f64| local_us.get(((local_us.len().max(1) - 1) as f64 * q) as usize).copied().unwrap_or(0);
    eprintln!("edits: {} local (body-only), {} structural, {} skipped; rule violations: {violations}; local edits that changed scopes / unmentioned elsewhere: {scope_flips}", local, structural, skipped);
    eprintln!("full update pass for local edits: p50 {} us, p90 {} us", p(0.5), p(0.9));
    let n = local.max(1) as u64;
    eprintln!("mean ruby phases over local edits (us): {:?}", ruby_breakdown.iter().map(|(k, v)| (k.trim_start_matches("ruby_time_"), v / n)).collect::<Vec<_>>());
    assert_eq!(violations, 0, "an edit with unchanged own facts changed the rest of the index");
}
