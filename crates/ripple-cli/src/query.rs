//! Request execution against a [`Snapshot`]. Shared by the daemon and the
//! in-process (`--no-daemon`) path so both answer identically.

use crate::engine::Snapshot;
use crate::protocol::{ErrorBody, FileParams, Format, ImpactParams, WhyParams};
use ripple_core::{Confidence, Impact, NodeId, QueryOpts};
use ripple_policy::{ChangedFile, EvalOptions, FileStatus, Hop, Report, evaluate_with, render};
use serde::Serialize;
use serde_json::{Value, json};
use std::time::Instant;

pub type QueryResult = Result<String, ErrorBody>;

fn parse<T: serde::de::DeserializeOwned + Default>(params: &Value) -> Result<T, ErrorBody> {
    if params.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(params.clone()).map_err(|e| ErrorBody::new("bad_params", e.to_string()))
}

fn to_json<T: Serialize>(v: &T) -> QueryResult {
    serde_json::to_string(v).map_err(|e| ErrorBody::new("internal", e.to_string()))
}

/// Normalises a user path to repo-relative form.
pub fn clean(p: &str) -> String {
    let p = p.trim().trim_start_matches("./");
    p.trim_end_matches('/').to_string()
}

/// Executes a read-only method. `reload`/`shutdown` are handled by the caller.
pub fn execute(snap: &Snapshot, method: &str, params: &Value, pending: usize) -> QueryResult {
    match method {
        "impact" => {
            let p: ImpactParams = parse(params)?;
            impact_result(snap, &p, pending, 0)
        }
        "dependents" => neighbours(snap, &parse(params)?, false),
        "dependencies" => neighbours(snap, &parse(params)?, true),
        "why" => why(snap, &parse(params)?),
        "areas" => areas(snap, &parse::<FileParams>(params)?.file),
        "stats" => to_json(&stats(snap)),
        other => Err(ErrorBody::new("unknown_method", format!("unknown method `{other}`"))),
    }
}

/// The changed-file list of an impact request: explicit `changed` entries
/// plus bare `files`, whose status is inferred from disk and the index.
pub fn changed_list(snap: &Snapshot, p: &ImpactParams) -> Vec<ChangedFile> {
    let mut out: Vec<ChangedFile> = p.changed.iter().cloned().map(|mut c| {
        c.path = clean(&c.path);
        c
    }).collect();
    for f in &p.files {
        let f = clean(f);
        let status = if snap.root.join(&f).exists() {
            if snap.files.contains(&f) || snap.graph.id(&f).is_some() { FileStatus::Modified } else { FileStatus::Added }
        } else {
            FileStatus::Deleted
        };
        out.push(ChangedFile::new(f, status));
    }
    out
}

pub fn impact_report(snap: &Snapshot, p: &ImpactParams, pending: usize, refresh_us: u64) -> Report {
    let changed = changed_list(snap, p);
    let opts = EvalOptions { max_depth: p.max_depth, min_confidence: p.min_confidence, generation: snap.generation, limit: p.limit };
    let mut report = evaluate_with(&snap.policy, &snap.graph, &snap.index, &changed, &opts);
    report.timing.refresh_us = refresh_us;
    report.index.pending_changes = pending;
    // Extractor warnings about the changed files themselves are relevant here.
    let names: std::collections::HashSet<&str> = changed.iter().map(|c| c.path.as_str()).collect();
    let mut extra: Vec<_> = snap.warnings.iter().filter(|w| w.file.as_deref().is_some_and(|f| names.contains(f)) || w.code == "config_invalid").cloned().collect();
    if !extra.is_empty() {
        report.warnings.append(&mut extra);
        report.warnings.sort_by(|a, b| (&a.code, &a.file, &a.message).cmp(&(&b.code, &b.file, &b.message)));
        report.warnings.dedup();
    }
    report
}

pub fn impact_result(snap: &Snapshot, p: &ImpactParams, pending: usize, refresh_us: u64) -> QueryResult {
    let max_listed = p.max_listed.unwrap_or(snap.policy.config.ci.max_listed);
    // Text and markdown never list more than `max_listed`; don't build the rest.
    let report = if p.format == Format::Json {
        impact_report(snap, p, pending, refresh_us)
    } else {
        let mut q = p.clone();
        q.limit = Some(q.limit.map_or(max_listed, |l| l.min(max_listed)));
        impact_report(snap, &q, pending, refresh_us)
    };
    match p.format {
        Format::Json => Ok(render::json::render(&report, false)),
        Format::Text => to_json(&render::text::render(&report, max_listed, p.color)),
        Format::Markdown => to_json(&render::markdown::render(&report, max_listed)),
    }
}

#[derive(Serialize)]
struct Neighbour {
    file: String,
    depth: u32,
    /// The file one hop closer to the queried file.
    via: String,
    kind: ripple_core::EdgeKind,
    confidence: Confidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

fn not_found(snap: &Snapshot, file: &str) -> ErrorBody {
    let hint = if snap.root.join(file).exists() { "it exists but is not part of the workspace (ignored?)" } else { "no such file" };
    ErrorBody::new("not_in_graph", format!("`{file}` is not in the index ({hint})"))
}

fn neighbours(snap: &Snapshot, p: &FileParams, forward: bool) -> QueryResult {
    let file = clean(&p.file);
    let node = snap.graph.id(&file).ok_or_else(|| not_found(snap, &file))?;
    let opts = QueryOpts { max_depth: p.depth.unwrap_or(1), min_confidence: p.min_confidence.unwrap_or(Confidence::Heuristic) };
    let imp = if forward { snap.graph.dependencies_closure(&[node], opts) } else { snap.graph.impact(&[node], opts) };
    let files: Vec<Neighbour> = imp
        .impacted()
        .map(|h| {
            let (_, _, d) = snap.graph.edge(h.edge);
            Neighbour {
                file: snap.graph.path(h.node).to_string(),
                depth: h.depth,
                via: snap.graph.path(h.via).to_string(),
                kind: d.kind,
                confidence: d.confidence,
                line: (d.line != 0).then_some(d.line),
                detail: snap.graph.edge_detail(h.edge).map(str::to_string),
            }
        })
        .collect();
    to_json(&json!({
        "file": file,
        "direction": if forward { "dependencies" } else { "dependents" },
        "depth": opts.max_depth,
        "count": files.len(),
        "files": files,
    }))
}

/// Hops of a BFS path from `node` back to the seed, in BFS order.
fn hops(snap: &Snapshot, imp: &Impact, node: NodeId) -> Vec<Hop> {
    imp.path(node)
        .into_iter()
        .map(|(n, e)| {
            if e == u32::MAX {
                Hop { file: snap.graph.path(n).to_string(), kind: None, confidence: None, line: None, detail: None }
            } else {
                let (_, _, d) = snap.graph.edge(e);
                Hop {
                    file: snap.graph.path(n).to_string(),
                    kind: Some(d.kind),
                    confidence: Some(d.confidence),
                    line: (d.line != 0).then_some(d.line),
                    detail: snap.graph.edge_detail(e).map(str::to_string),
                }
            }
        })
        .collect()
}

fn why(snap: &Snapshot, p: &WhyParams) -> QueryResult {
    let (from, to) = (clean(&p.from), clean(&p.to));
    let a = snap.graph.id(&from).ok_or_else(|| not_found(snap, &from))?;
    let b = snap.graph.id(&to).ok_or_else(|| not_found(snap, &to))?;
    // `from` depends on `to` <=> `from` is in the reverse closure of `to`.
    let imp = snap.graph.impact(&[b], QueryOpts::default());
    if imp.contains(a) {
        return to_json(&json!({"from": from, "to": to, "found": true, "direction": "from_depends_on_to", "path": hops(snap, &imp, a)}));
    }
    let imp = snap.graph.impact(&[a], QueryOpts::default());
    if imp.contains(b) {
        return to_json(&json!({"from": from, "to": to, "found": true, "direction": "to_depends_on_from", "path": hops(snap, &imp, b)}));
    }
    to_json(&json!({"from": from, "to": to, "found": false, "path": []}))
}

fn areas(snap: &Snapshot, file: &str) -> QueryResult {
    let file = clean(file);
    let pol = &snap.policy;
    let class = pol.classify(&file);
    let areas: Vec<Value> = class
        .areas
        .iter()
        .map(|&a| {
            let a = pol.area(a);
            json!({"area": a.name, "tier": a.tier, "owners": a.owners, "description": a.description})
        })
        .collect();
    let node = snap.graph.id(&file);
    to_json(&json!({
        "file": file,
        "areas": areas,
        "language": class.language,
        "is_test": class.test,
        "is_global": class.global,
        "is_ignored": pol.is_ignored(&file),
        "in_graph": node.is_some(),
        "dependents": node.map(|n| snap.graph.fan_in(n)).unwrap_or(0),
        "dependencies": node.map(|n| snap.graph.fan_out(n)).unwrap_or(0),
    }))
}

pub fn stats(snap: &Snapshot) -> Value {
    let mut by_code = std::collections::BTreeMap::<&str, usize>::new();
    for w in &snap.warnings {
        *by_code.entry(w.code.as_str()).or_default() += 1;
    }
    let built = snap.built_at.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    json!({
        "generation": snap.generation,
        "root": snap.root,
        "files": snap.files.len(),
        "nodes": snap.graph.node_count(),
        "edges": snap.graph.edge_count(),
        "listing": snap.listing,
        "tombstones": snap.tombstones,
        "built_at_unix_ms": built,
        "timings_us": snap.timings,
        "extractors": snap.stats,
        "warnings_total": snap.warnings.len(),
        "warnings_by_code": by_code,
        "warnings": snap.warnings.iter().take(50).collect::<Vec<_>>(),
        "policy": {
            "source": snap.policy.source,
            "default_rules": snap.policy.uses_default_rules(),
            "rules": snap.policy.rules().map(|r| r.id.clone()).collect::<Vec<_>>(),
            "areas": snap.policy.areas().iter().map(|a| a.name.clone()).collect::<Vec<_>>(),
        },
    })
}

/// Times a closure in microseconds.
pub fn timed<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let t = Instant::now();
    let v = f();
    (v, t.elapsed().as_micros() as u64)
}
