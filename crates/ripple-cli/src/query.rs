//! Request execution against a [`Snapshot`]. Shared by the daemon and the
//! in-process (`--no-daemon`) path so both answer identically.

use crate::engine::Snapshot;
use crate::protocol::{ErrorBody, FileParams, Format, ImpactParams, WhyParams};
use ripple_core::{Confidence, QueryOpts, Warning};
use ripple_policy::{ChangedFile, EvalOptions, FileStatus, Report, evaluate_with, path_hops, render, scopes_for_hunks};
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
///
/// Seed scopes: `scopes` from the request apply to the bare `files`; a
/// `changed` entry's own `scopes` are kept; otherwise its `hunks` are mapped
/// onto the file's scope ranges in this snapshot (see
/// [`ripple_policy::scopes_for_hunks`]). `file_level` drops all of it.
pub fn changed_list(snap: &Snapshot, p: &ImpactParams) -> Vec<ChangedFile> {
    let mut out: Vec<ChangedFile> = p
        .changed
        .iter()
        .cloned()
        .map(|mut c| {
            c.path = clean(&c.path);
            if c.scopes.is_none() {
                c.scopes = scopes_for_hunks(c.status, c.hunks.as_deref(), snap.scopes_of(&c.path));
            }
            c
        })
        .collect();
    for f in &p.files {
        let f = clean(f);
        let status = if snap.root.join(&f).exists() {
            if snap.files.contains(&f) || snap.graph.id(&f).is_some() { FileStatus::Modified } else { FileStatus::Added }
        } else {
            FileStatus::Deleted
        };
        let mut c = ChangedFile::new(f, status);
        if !p.scopes.is_empty() && status == FileStatus::Modified {
            c.scopes = Some(p.scopes.clone());
        }
        out.push(c);
    }
    if p.file_level {
        for c in &mut out {
            c.scopes = None;
        }
    }
    out
}

/// Requested scope names a file's scope table does not know (only checked
/// for files that have a table: controller actions may exist on edges only).
fn unknown_scopes(snap: &Snapshot, file: &str, scopes: &[String]) -> Vec<String> {
    match snap.scopes_of(file) {
        Some(ranges) => scopes.iter().filter(|s| *s != ripple_core::model::SCOPE_BODY && !ranges.iter().any(|r| &r.name == *s)).cloned().collect(),
        None => Vec::new(),
    }
}

/// For a deleted file the index no longer knows: JS/TS files whose relative
/// imports now fail to resolve to it (from the extractor's
/// `js_unresolved_import` warnings). Ruby constant references to a deleted
/// file cannot be recovered this way.
fn still_importing(snap: &Snapshot, deleted: &str) -> Vec<String> {
    let stem = deleted.rsplit_once('.').map_or(deleted, |(s, _)| s);
    let mut out: Vec<String> = snap
        .warnings
        .iter()
        .filter(|w| w.code == "js_unresolved_import")
        .filter_map(|w| {
            let file = w.file.as_deref()?;
            let spec = w.message.rsplit('\'').nth(1)?;
            if !spec.starts_with('.') {
                return None;
            }
            let dir = file.rsplit_once('/').map_or("", |(d, _)| d);
            let mut parts: Vec<&str> = if dir.is_empty() { Vec::new() } else { dir.split('/').collect() };
            for seg in spec.split('/') {
                match seg {
                    "." | "" => {}
                    ".." => {
                        parts.pop()?;
                    }
                    s => parts.push(s),
                }
            }
            let target = parts.join("/");
            let hit = target == deleted || target == stem || format!("{target}/index") == stem;
            hit.then(|| file.to_string())
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

pub fn impact_report(snap: &Snapshot, p: &ImpactParams, pending: usize, refresh_us: u64) -> Report {
    let changed = changed_list(snap, p);
    let opts = EvalOptions { max_depth: p.max_depth, min_confidence: p.min_confidence, generation: snap.generation, limit: p.limit, file_level: p.file_level };
    let mut report = evaluate_with(&snap.policy, &snap.graph, &snap.index, &changed, &opts);
    report.timing.refresh_us = refresh_us;
    report.index.pending_changes = pending;
    // Extractor warnings about the changed files themselves are relevant here.
    let names: std::collections::HashSet<&str> = changed.iter().map(|c| c.path.as_str()).collect();
    let mut extra: Vec<_> = snap.warnings.iter().filter(|w| w.file.as_deref().is_some_and(|f| names.contains(f)) || w.code == "config_invalid").cloned().collect();
    for c in &changed {
        if let Some(scopes) = &c.scopes {
            let unknown = unknown_scopes(snap, &c.path, scopes);
            if !unknown.is_empty() {
                extra.push(Warning::new("scope_unknown", format!("no method or action named {} in this file; nothing can depend on it", unknown.join(", "))).file(&c.path));
            }
        }
        if c.status == FileStatus::Deleted && snap.graph.id(&c.path).is_none() {
            let importers = still_importing(snap, &c.path);
            if !importers.is_empty() {
                let shown: Vec<&str> = importers.iter().take(10).map(String::as_str).collect();
                let more = if importers.len() > 10 { format!(" and {} more", importers.len() - 10) } else { String::new() };
                extra.push(Warning::new("deleted_file_still_imported", format!("{} file{} still import{} it and will break: {}{more}", importers.len(), if importers.len() == 1 { "" } else { "s" }, if importers.len() == 1 { "s" } else { "" }, shown.join(", "))).file(&c.path));
            }
        }
    }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    from_scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to_scope: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    inherit: bool,
}

fn not_found(snap: &Snapshot, file: &str) -> ErrorBody {
    let hint = if snap.root.join(file).exists() { "it exists but is not part of the workspace (ignored?)" } else { "no such file" };
    ErrorBody::new("not_in_graph", format!("`{file}` is not in the index ({hint})"))
}

fn neighbours(snap: &Snapshot, p: &FileParams, forward: bool) -> QueryResult {
    let file = clean(&p.file);
    let node = snap.graph.id(&file).ok_or_else(|| not_found(snap, &file))?;
    let opts = QueryOpts { max_depth: p.depth.unwrap_or(1), min_confidence: p.min_confidence.unwrap_or(Confidence::Heuristic), file_level: false };
    if forward && !p.scopes.is_empty() {
        return Err(ErrorBody::new("bad_params", "`scopes` applies to dependents, not dependencies"));
    }
    let scopes = (!p.scopes.is_empty()).then(|| p.scopes.clone());
    let imp = if forward { snap.graph.dependencies_closure(&[node], opts) } else { snap.graph.impact_scoped(&[(node, scopes)], opts) };
    let files: Vec<Neighbour> = imp
        .impacted()
        .map(|h| {
            let (_, _, d) = snap.graph.edge(h.edge);
            let (from_scope, to_scope, inherit) = if forward { (None, None, false) } else { shown_scopes(snap, h.edge) };
            Neighbour {
                file: snap.graph.path(h.node).to_string(),
                depth: h.depth,
                via: snap.graph.path(h.via).to_string(),
                kind: d.kind,
                confidence: d.confidence,
                line: (d.line != 0).then_some(d.line),
                detail: snap.graph.edge_detail(h.edge).map(str::to_string),
                from_scope,
                to_scope,
                inherit,
            }
        })
        .collect();
    let unknown = unknown_scopes(snap, &file, &p.scopes);
    to_json(&json!({
        "file": file,
        "scopes": p.scopes,
        "unknown_scopes": unknown,
        "direction": if forward { "dependencies" } else { "dependents" },
        "depth": opts.max_depth,
        "count": files.len(),
        "files": files,
    }))
}

/// Edge scopes as shown to readers (no `@body`; none on inheritance edges).
fn shown_scopes(snap: &Snapshot, edge: u32) -> (Option<String>, Option<String>, bool) {
    if snap.graph.edge_inherits(edge) {
        return (None, None, true);
    }
    let (f, t) = snap.graph.edge_scopes(edge);
    let keep = |s: Option<&str>| s.filter(|s| *s != ripple_core::model::SCOPE_BODY).map(str::to_string);
    (keep(f), keep(t), false)
}

/// Scope-aware: a path only counts when the impact search, which follows
/// method / action scopes on edges, actually gets through it. With `scopes`,
/// only paths starting at those methods / actions of `to` count.
fn why(snap: &Snapshot, p: &WhyParams) -> QueryResult {
    let (from, to) = (clean(&p.from), clean(&p.to));
    let a = snap.graph.id(&from).ok_or_else(|| not_found(snap, &from))?;
    let b = snap.graph.id(&to).ok_or_else(|| not_found(snap, &to))?;
    let scopes = (!p.scopes.is_empty()).then(|| p.scopes.clone());
    // `from` depends on `to` <=> `from` is in the reverse closure of `to`.
    let imp = snap.graph.impact_scoped(&[(b, scopes.clone())], QueryOpts::default());
    if imp.contains(a) {
        return to_json(&json!({"from": from, "to": to, "scopes": p.scopes, "found": true, "direction": "from_depends_on_to", "path": path_hops(&snap.graph, &imp, a)}));
    }
    if scopes.is_none() {
        let imp = snap.graph.impact(&[a], QueryOpts::default());
        if imp.contains(b) {
            return to_json(&json!({"from": from, "to": to, "scopes": p.scopes, "found": true, "direction": "to_depends_on_from", "path": path_hops(&snap.graph, &imp, b)}));
        }
    }
    // Say whether ignoring scopes would have found one: the files are linked,
    // just not through the parts that matter.
    let whole = snap.graph.impact_scoped(&[(b, None)], QueryOpts { file_level: true, ..QueryOpts::default() }).contains(a)
        || snap.graph.impact_scoped(&[(a, None)], QueryOpts { file_level: true, ..QueryOpts::default() }).contains(b);
    to_json(&json!({"from": from, "to": to, "scopes": p.scopes, "found": false, "file_level_path": whole, "unknown_scopes": unknown_scopes(snap, &to, &p.scopes), "path": []}))
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
        "files_with_scopes": snap.scopes.len(),
        "scopes": snap.scopes.values().map(Vec::len).sum::<usize>(),
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
