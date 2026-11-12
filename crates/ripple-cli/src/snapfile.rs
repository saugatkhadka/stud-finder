//! A [`Snapshot`] on disk, so a sleeping daemon (see `server`) can answer
//! queries from a memory-mapped file instead of the heap.
//!
//! The file is a [`SectionFile`]: the graph's columns (node paths, edges,
//! both adjacency arrays, method mentions, metrics), workspace membership as
//! one bit per node, the scope table, and a JSON section with the small
//! fields (warnings, stats, seen times, ...). Loading maps it and checks
//! every invariant queries rely on; a file that fails any check, or was
//! written by another binary, for another root, or under another config, is
//! ignored (the caller then indexes from scratch). The policy index is
//! recomputed from the mapped node paths (it is a pure function of the
//! policy and the paths).
//!
//! Written with [`SectionWriter::write_atomic`] (temporary file + rename):
//! a reader sees the previous file or the complete new one, never a torn one.

use crate::engine::{FileSet, ScopeMap, Snapshot, load_policy};
use ripple_core::store::{SectionFile, SectionWriter};
use ripple_core::{Graph, Warning};
use ripple_policy::PolicyIndex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Bumped whenever the layout of any section changes.
pub const FORMAT: u32 = 1;
const META: u32 = 1;
const FILES: u32 = 10;
const SCOPES: u32 = 20;
const GRAPH: u32 = 1000;

#[derive(Serialize, Deserialize)]
struct Meta {
    key: String,
    root: PathBuf,
    config: serde_json::Value,
    generation: u64,
    warnings: Vec<Warning>,
    stats: BTreeMap<String, BTreeMap<String, u64>>,
    timings: BTreeMap<String, u64>,
    seen: Vec<(String, u64)>,
    base_time_ns: u64,
    built_at_ns: u64,
    listing: String,
    tombstones: usize,
}

fn ns(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

fn time(ns: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(ns)
}

/// What else determines a snapshot's contents besides the root, the config
/// and the files: this binary (so a rebuilt ripple re-indexes) and the
/// environment variables that change the pipeline.
fn key() -> String {
    let exe = std::env::current_exe().ok().and_then(|p| std::fs::metadata(p).ok()).map(|m| format!("{}:{}", m.len(), m.modified().map(ns).unwrap_or(0))).unwrap_or_default();
    let env: Vec<String> = ["RIPPLE_EXTRACTORS", crate::edgefile::ENV, "RIPPLE_NO_GRAPH_PATCH", "RIPPLE_RUBY_NO_FAST"]
        .iter()
        .map(|k| format!("{k}={}", std::env::var(k).unwrap_or_default()))
        .collect();
    format!("{FORMAT}/{}/{exe}/{}", env!("CARGO_PKG_VERSION"), env.join(","))
}

/// Writes `snap` to `path` atomically. Returns the file size.
pub fn save(snap: &Snapshot, path: &Path) -> Result<u64, String> {
    let paths = snap.graph.path_table();
    let bits = snap.files.to_bits(paths).ok_or("a workspace file is not a graph node")?;
    let seen = snap.seen.read().unwrap_or_else(|p| p.into_inner()).iter().map(|(k, v)| (k.clone(), ns(*v))).collect();
    let meta = Meta {
        key: key(),
        root: snap.root.clone(),
        config: serde_json::to_value(&snap.policy.config).map_err(|e| e.to_string())?,
        generation: snap.generation,
        warnings: snap.warnings.clone(),
        stats: snap.stats.clone(),
        timings: snap.timings.clone(),
        seen,
        base_time_ns: ns(snap.base_time),
        built_at_ns: ns(snap.built_at),
        listing: snap.listing.to_string(),
        tombstones: snap.tombstones,
    };
    let mut w = SectionWriter::new();
    w.put_bytes(META, &serde_json::to_vec(&meta).map_err(|e| e.to_string())?);
    w.put(FILES, &bits);
    snap.scopes.save(&mut w, SCOPES);
    snap.graph.save(&mut w, GRAPH);
    w.write_atomic(FORMAT, path).map_err(|e| format!("writing {}: {e}", path.display()))
}

/// Snapshots kept per directory by [`prune`], counting the one just written.
pub const PRUNE_KEEP: usize = 3;
/// [`prune`] deletes any other snapshot older than this.
pub const PRUNE_MAX_AGE: Duration = Duration::from_secs(3 * 24 * 3600);
/// [`prune`] never deletes a snapshot written more recently than this: another
/// daemon may have just saved it and be about to map it in its new image.
pub const PRUNE_MIN_AGE: Duration = Duration::from_secs(10 * 60);

/// Deletes stale snapshots next to `keep` (the file just written): other
/// `index-*.snap` files (one per root and `--config`, e.g. other worktrees
/// sharing the git directory) older than [`PRUNE_MAX_AGE`], or beyond the
/// newest [`PRUNE_KEEP`]. Files younger than [`PRUNE_MIN_AGE`] and `keep`
/// itself are never touched; a sleeping daemon that has its (older) file
/// mapped is unaffected, as a mapping outlives the file's name and a daemon
/// only reads its file right after writing it. Returns the deleted paths.
pub fn prune(keep: &Path, now: SystemTime) -> Vec<PathBuf> {
    let Some(dir) = keep.parent() else { return Vec::new() };
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut others: Vec<(SystemTime, PathBuf)> = rd
        .flatten()
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with("index-") && n.ends_with(".snap") && e.path() != keep
        })
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    others.sort_by_key(|o| std::cmp::Reverse(o.0));
    let mut deleted = Vec::new();
    for (i, (mtime, path)) in others.into_iter().enumerate() {
        let age = now.duration_since(mtime).unwrap_or_default();
        // `keep` is the newest; `i + 1` others are newer than or as new as this one.
        let surplus = i + 1 >= PRUNE_KEEP;
        if age >= PRUNE_MIN_AGE && (surplus || age >= PRUNE_MAX_AGE) && std::fs::remove_file(&path).is_ok() {
            deleted.push(path);
        }
    }
    deleted
}

/// Maps the snapshot at `path` if it is valid for `root` as configured now.
pub fn load(path: &Path, root: &Path) -> Result<Snapshot, String> {
    let f = SectionFile::open(path, FORMAT)?;
    let meta: Meta = serde_json::from_slice(f.bytes(META)?).map_err(|e| format!("bad metadata: {e}"))?;
    if meta.key != key() {
        return Err("written by another ripple binary or environment".into());
    }
    if meta.root != root {
        return Err(format!("written for {}", meta.root.display()));
    }
    let (policy, warning) = load_policy(root);
    if warning.is_some() || serde_json::to_value(&policy.config).ok().as_ref() != Some(&meta.config) {
        return Err("the configuration changed".into());
    }
    let listing = match meta.listing.as_str() {
        "git" => "git",
        "walk" => "walk",
        _ => return Err("bad listing".into()),
    };
    let graph = Graph::load(&f, GRAPH)?;
    let files = FileSet::from_bits(graph.path_table().clone(), f.slab(FILES)?)?;
    let scopes = ScopeMap::load(&f, SCOPES)?;
    let index = PolicyIndex::build(&policy, &graph);
    let seen: HashMap<String, SystemTime> = meta.seen.into_iter().map(|(k, v)| (k, time(v))).collect();
    Ok(Snapshot {
        generation: meta.generation,
        root: root.to_path_buf(),
        graph,
        policy: Arc::new(policy),
        index,
        warnings: meta.warnings,
        stats: meta.stats,
        timings: meta.timings,
        files,
        seen: Arc::new(RwLock::new(seen)),
        base_time: time(meta.base_time_ns),
        listing,
        tombstones: meta.tombstones,
        built_at: time(meta.built_at_ns),
        scopes,
    })
}

/// Everything a query can observe that differs between `a` and `b` (tests:
/// a loaded snapshot must answer exactly like the one it was saved from).
/// Per-node queries (`dependents`, `dependencies`, `file`, `areas`,
/// `impact` of the file, its scopes, its policy class) run for every
/// `every`-th node.
#[doc(hidden)]
pub fn differences(a: &Arc<Snapshot>, b: &Arc<Snapshot>, every: usize) -> Vec<String> {
    use crate::query;
    use crate::signals::Sources;
    use serde_json::json;
    let src = Sources::none();
    let mut bad = Vec::new();
    let mut check = |what: &str, x: String, y: String| {
        if x != y {
            // Show both sides from a little before the first difference.
            let at = x.bytes().zip(y.bytes()).take_while(|(p, q)| p == q).count();
            let from = |s: &str| {
                let mut i = at.saturating_sub(80);
                while !s.is_char_boundary(i) {
                    i -= 1;
                }
                s[i..].chars().take(240).collect::<String>()
            };
            bad.push(format!("{what}: ...{} | ...{}", from(&x), from(&y)));
        }
    };
    let sorted = |s: &Snapshot| {
        let mut v: Vec<&str> = s.files.iter().collect();
        v.sort_unstable();
        v.join("\n")
    };
    check("stats", query::stats(a).to_string(), query::stats(b).to_string());
    check("files", sorted(a), sorted(b));
    check("scopes", format!("{:?}", a.scopes.iter().collect::<Vec<_>>()), format!("{:?}", b.scopes.iter().collect::<Vec<_>>()));
    check("warnings", format!("{:?}", a.warnings), format!("{:?}", b.warnings));
    check("generation", a.generation.to_string(), b.generation.to_string());
    let all: Vec<String> = a.files.iter().map(String::from).collect();
    check("stale", format!("{:?}", a.stale_files(&all)), format!("{:?}", b.stale_files(&all)));
    check("nodes", a.graph.node_count().to_string(), b.graph.node_count().to_string());
    // Answers minus their `timing` (wall-clock microseconds).
    let q = |s: &Arc<Snapshot>, m: &str, p: serde_json::Value| match query::execute(s, m, &p, 0, &src) {
        Ok(v) => match serde_json::from_str::<serde_json::Value>(&v) {
            Ok(mut j) => {
                if let Some(o) = j.as_object_mut() {
                    o.remove("timing");
                }
                j.to_string()
            }
            Err(_) => v,
        },
        Err(e) => format!("error {e:?}"),
    };
    check("rank", q(a, "rank", json!({})), q(b, "rank", json!({})));
    for i in (0..a.graph.node_count()).step_by(every.max(1)) {
        let path = a.graph.path(i as u32);
        check(&format!("class {path}"), format!("{:?}", a.index.get(i as u32)), format!("{:?}", b.index.get(i as u32)));
        check(&format!("scopes_of {path}"), format!("{:?}", a.scopes_of(path)), format!("{:?}", b.scopes_of(path)));
        for m in ["dependents", "dependencies", "file", "areas"] {
            check(&format!("{m} {path}"), q(a, m, json!({ "file": path })), q(b, m, json!({ "file": path })));
        }
        check(&format!("impact {path}"), q(a, "impact", json!({ "files": [path] })), q(b, "impact", json!({ "files": [path] })));
    }
    bad
}
