//! Request execution against a [`Snapshot`]. Shared by the daemon and the
//! in-process (`--no-daemon`) path so both answer identically.

use crate::engine::Snapshot;
use crate::historyd::HistoryInfo;
use crate::protocol::{ErrorBody, FileParams, Format, ImpactParams, RankParams, WhyParams};
use crate::signals::{RankMode, Sources};
use std::sync::Arc;
use ripple_core::{Confidence, QueryOpts, Warning};
use ripple_policy::{ChangedFile, EvalOptions, FileStatus, Incomplete, Language, Report, classify_change, def_names, evaluate_with, path_hops, render, scopes_for_hunks, summary_line};
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
pub fn execute(snap: &Arc<Snapshot>, method: &str, params: &Value, pending: usize, src: &Sources) -> QueryResult {
    match method {
        "impact" => {
            let p: ImpactParams = parse(params)?;
            impact_result(snap, &p, pending, 0, src).map(|(body, _)| body)
        }
        "dependents" => dependents(snap, &parse(params)?, src),
        "file" => file_profile(snap, &parse::<FileParams>(params)?.file, src),
        "rank" => rank(snap, &parse(params)?, src),
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
    for c in &mut out {
        if c.change.is_none() {
            c.change = Some(change_of(snap, c));
        }
    }
    out
}

/// What kind of change `c` is, from its hunks, `def` lines and this
/// snapshot's method ranges. The new file's own `def`s are read from disk so
/// a method moved within the file is not taken for a removed one.
fn change_of(snap: &Snapshot, c: &ChangedFile) -> ripple_policy::Change {
    let ruby = Language::of(&c.path) == Language::Ruby;
    let new_defs = (ruby && !c.def_lines.is_empty() && c.status != FileStatus::Deleted)
        .then(|| std::fs::read_to_string(snap.root.join(&c.path)).ok())
        .flatten()
        .map(|t| def_names(&t));
    classify_change(c, snap.scopes_of(&c.path), new_defs.as_deref())
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

/// The report of an impact request and the `history` envelope object.
/// Signals are looked up before evaluation (rules use them); `impact`
/// never computes a ranking in the daemon's foreground.
pub fn impact_report(snap: &Arc<Snapshot>, p: &ImpactParams, pending: usize, refresh_us: u64, src: &Sources) -> (Report, HistoryInfo) {
    let changed = changed_list(snap, p);
    let (signals, history) = src.input(snap, &changed, RankMode::Cached);
    let opts = EvalOptions {
        max_depth: p.max_depth,
        min_confidence: p.min_confidence,
        generation: snap.generation,
        limit: p.limit,
        all: p.all,
        policy: p.policy,
        file_level: p.file_level,
        root: Some(snap.root.clone()),
        signals: Some(signals),
    };
    let mut report = evaluate_with(&snap.policy, &snap.graph, &snap.index, &changed, &opts);
    report.timing.refresh_us = refresh_us;
    report.index.pending_changes = pending;
    // Reasons the answer may be incomplete that only the index knows.
    for c in &changed {
        let unresolved = snap.warnings.iter().filter(|w| w.file.as_deref() == Some(c.path.as_str()) && w.code.contains("unresolved")).count();
        if unresolved > 0 {
            report.incomplete.push(Incomplete {
                reason: "unresolved_references".into(),
                file: Some(c.path.clone()),
                message: format!("{unresolved} reference{} in this file could not be resolved (see warnings); what it relies on may be missing", if unresolved == 1 { "" } else { "s" }),
            });
        }
    }
    if pending > 0 {
        report.incomplete.push(Incomplete {
            reason: "stale_index".into(),
            file: None,
            message: format!("answered from an index {pending} file change{} behind the work tree (re-run with --sync to wait)", if pending == 1 { "" } else { "s" }),
        });
    }
    report.summary = summary_line(&report);
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
    (report, history)
}

/// The rendered report and the `history` envelope object.
pub fn impact_result(snap: &Arc<Snapshot>, p: &ImpactParams, pending: usize, refresh_us: u64, src: &Sources) -> Result<(String, HistoryInfo), ErrorBody> {
    let (report, history) = impact_report(snap, p, pending, refresh_us, src);
    let body = match p.format {
        Format::Json => Ok(render::json::render(&report, false)),
        // The report is already cut to its budget (or `limit`), unless `all`.
        Format::Text => to_json(&render::text::render(&report, p.max_listed.unwrap_or(usize::MAX), p.color)),
        Format::Markdown => to_json(&render::markdown::render(&report, p.max_listed.unwrap_or(snap.policy.config.ci.max_listed))),
    }?;
    Ok((body, history))
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

/// `dependents`: the impact report of one file (or some of its methods /
/// actions) as if it had changed, so it reads exactly like `impact`.
fn dependents(snap: &Arc<Snapshot>, p: &FileParams, src: &Sources) -> QueryResult {
    let file = clean(&p.file);
    snap.graph.id(&file).ok_or_else(|| not_found(snap, &file))?;
    let ip = ImpactParams {
        files: vec![file],
        scopes: p.scopes.clone(),
        max_depth: p.depth,
        min_confidence: p.min_confidence,
        all: p.all,
        limit: p.limit,
        ..Default::default()
    };
    impact_result(snap, &ip, 0, 0, src).map(|(body, _)| body)
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
    }))
}

/// Metrics of one file from the graph and the looked-up signals (the same
/// object as `changed[].metrics`).
fn metrics_for(snap: &Snapshot, file: &str, sig: &ripple_policy::SignalInput) -> ripple_policy::Metrics {
    let mut m = ripple_policy::Metrics::default();
    if let Some(fm) = snap.graph.metrics_of(file) {
        m.complexity = Some(fm.complexity);
        m.max_method = (fm.methods > 0).then_some(u32::from(fm.max_method));
        m.lines = Some(fm.lines);
    }
    if let Some(f) = sig.files.get(file) {
        m.churn = f.churn.filter(|_| sig.status.churn.is_ready());
        m.coverage = f.coverage.filter(|_| sig.status.coverage.is_ready());
        m.hotspot = f.hotspot.filter(|_| sig.status.hotspot.is_ready());
    }
    m
}

/// Signal states, with complexity from the graph.
fn signal_states(snap: &Snapshot, mut s: ripple_policy::Signals) -> ripple_policy::Signals {
    if snap.graph.metrics_tables().is_empty() {
        s.complexity = ripple_policy::SignalStatus::new(ripple_policy::SignalState::Unavailable, Some("the index has no metrics".to_string()));
    }
    s
}

/// `file`: one file's profile.
fn file_profile(snap: &Arc<Snapshot>, file: &str, src: &Sources) -> QueryResult {
    let file = clean(file);
    let g = &snap.graph;
    let node = g.id(&file);
    if node.is_none() && !snap.files.contains(&file) {
        return Err(not_found(snap, &file));
    }
    let pol = &snap.policy;
    let class = node.and_then(|n| snap.index.get(n)).cloned().unwrap_or_else(|| pol.classify(&file));
    let areas: Vec<Value> = class.areas.iter().map(|&a| pol.area(a)).map(|a| json!({"area": a.name, "tier": a.tier, "owners": a.owners})).collect();
    let owners: Vec<String> = class.areas.iter().flat_map(|&a| pol.area(a).owners.iter().cloned()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let cf = ChangedFile::new(file.clone(), FileStatus::Modified);
    let (sig, _) = src.input(snap, &[cf], RankMode::Any);
    let metrics = metrics_for(snap, &file, &sig);
    let cov = src.coverage.ready().filter(|_| sig.status.coverage.is_ready());
    // Methods (Ruby): named from the scope table by their `def` line.
    let scopes = snap.scopes_of(&file).unwrap_or_default();
    let mut methods: Vec<Value> = Vec::new();
    let mut uncovered: Vec<String> = Vec::new();
    let mut method_count = 0;
    if let Some(n) = node {
        let mm = g.method_metrics(n);
        method_count = mm.len();
        // The scope table keeps one range per name; methods it does not
        // name (redefinitions, `def self.x`) are named from their `def` line.
        let text = std::fs::read_to_string(snap.root.join(&file)).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        let name_of = |start: u32| {
            scopes.iter().find(|r| r.start == start).map(|r| r.name.clone()).or_else(|| {
                let l = lines.get((start as usize).checked_sub(1)?)?.trim_start();
                let rest = l.strip_prefix("def ")?.trim_start();
                let name: String = rest.chars().take_while(|c| !matches!(c, '(' | ' ' | ';' | '\n')).collect();
                (!name.is_empty()).then_some(name)
            })
        };
        // The body without the `def` line, which runs when the class loads
        // (SimpleCov counts it as covered even for a method never called).
        let body = |m: &&ripple_core::MethodMetric| if m.end() > m.start { (m.start + 1, m.end()) } else { (m.start, m.end()) };
        let mut sorted: Vec<_> = mm.iter().collect();
        sorted.sort_by(|a, b| b.complexity.cmp(&a.complexity).then(a.start.cmp(&b.start)));
        for m in sorted.iter().take(5) {
            let mut v = json!({"name": name_of(m.start), "start": m.start, "end": m.end(), "complexity": m.complexity});
            if let Some(c) = cov.and_then(|c| c.lines(&file, &[body(m)])).and_then(|l| l.percent()) {
                v["coverage"] = json!(ripple_policy::round1(c));
            }
            methods.push(v);
        }
        if let Some(c) = cov.filter(|c| c.file(&file).is_some()) {
            for m in mm {
                if c.lines(&file, &[body(&m)]).is_some_and(|l| l.coverable > 0 && l.covered == 0) {
                    uncovered.push(name_of(m.start).unwrap_or_else(|| format!("line {}", m.start)));
                }
            }
        }
    }
    let (mut deps_n, mut dependents, mut tests, mut dependencies) = (Vec::new(), Vec::new(), Vec::new(), 0usize);
    if let Some(n) = node {
        let mut seen = std::collections::HashSet::new();
        for (d, _) in g.dependents(n) {
            if !seen.insert(d) {
                continue;
            }
            if snap.index.get(d).is_some_and(|c| c.test) {
                tests.push(g.path(d).to_string());
            } else {
                deps_n.push(d);
            }
        }
        deps_n.sort_by(|&a, &b| g.fan_in(b).cmp(&g.fan_in(a)).then_with(|| g.path(a).cmp(g.path(b))));
        dependents = deps_n.iter().take(5).map(|&d| json!({"file": g.path(d), "dependents": g.fan_in(d)})).collect();
        tests.sort();
        dependencies = g.dependencies(n).map(|(d, _)| d).collect::<std::collections::HashSet<_>>().len();
    }
    let tests_total = tests.len();
    tests.truncate(10);
    let partners: Vec<ripple_policy::CoChangeEntry> = if sig.status.cochange.is_ready() {
        let q = ripple_history::PartnerQuery::from_config(&pol.config.history);
        src.history.lookup(std::slice::from_ref(&file), &q, |p| p == file).usually_changes_with.iter().map(crate::signals::co_entry).collect()
    } else {
        Vec::new()
    };
    to_json(&json!({
        "file": file,
        "language": class.language,
        "areas": areas,
        "tier": pol.max_tier(&class.areas),
        "owners": owners,
        "is_test": class.test,
        "is_global": class.global,
        "in_graph": node.is_some(),
        "ranked": metrics.hotspot.map(|h| h.of),
        "metrics": metrics,
        "methods": method_count,
        "top_methods": methods,
        "uncovered_methods": uncovered,
        "dependents": {"count": deps_n.len(), "top": dependents},
        "dependencies": {"count": dependencies},
        "tests": {"count": tests_total, "files": tests},
        "usually_changes_with": partners,
        "signals": signal_states(snap, sig.status),
    }))
}

/// `rank`: the riskiest non-test source files.
fn rank(snap: &Arc<Snapshot>, p: &RankParams, src: &Sources) -> QueryResult {
    let filter = ripple_policy::PathFilter::new(&p.path).map_err(|e| ErrorBody::new("bad_params", format!("--path: {e}")))?;
    let area = match &p.area {
        Some(a) => Some(snap.policy.areas().iter().position(|x| &x.name == a).ok_or_else(|| ErrorBody::new("bad_params", format!("no area `{a}` in ripple.yml")))? as u16),
        None => None,
    };
    let by = p.by.as_deref().unwrap_or("score");
    if !["score", "churn", "complexity", "coverage"].contains(&by) {
        return Err(ErrorBody::new("bad_params", format!("--by {by}: expected score, churn, complexity or coverage")));
    }
    let (ranking, hotspot) = src.ranking(snap, RankMode::Fresh);
    let (churn, cochange) = src.history_status();
    let signals = signal_states(snap, ripple_policy::Signals { complexity: ripple_policy::SignalStatus::ready(), churn, cochange, coverage: src.coverage.status(), hotspot });
    let Some(r) = ranking else {
        return to_json(&json!({"by": by, "formula": crate::signals::rank_formula(&snap.policy.config.rank.weights), "files": 0, "matching": 0, "rows": [], "signals": signals}));
    };
    let mut rows: Vec<&crate::signals::RankRow> = r.rows.iter().filter(|x| filter.is_match(&x.path) && area.is_none_or(|a| x.areas.contains(&a))).collect();
    match by {
        "churn" => rows.sort_by(|a, b| (b.recent_commits, b.commits).cmp(&(a.recent_commits, a.commits)).then(a.rank.cmp(&b.rank))),
        "complexity" => rows.sort_by(|a, b| (b.complexity, b.max_method).cmp(&(a.complexity, a.max_method)).then(a.rank.cmp(&b.rank))),
        "coverage" => {
            rows.retain(|x| x.coverage.is_some());
            rows.sort_by(|a, b| a.coverage.unwrap_or(0.0).total_cmp(&b.coverage.unwrap_or(0.0)).then(a.rank.cmp(&b.rank)));
        }
        _ => {}
    }
    let matching = rows.len();
    rows.truncate(p.limit.unwrap_or(20));
    let cutoff = r.cutoff();
    let rows: Vec<Value> = rows
        .into_iter()
        .map(|x| {
            let mut v = serde_json::to_value(x).unwrap_or(Value::Null);
            v["hotspot"] = json!(x.rank <= cutoff);
            v
        })
        .collect();
    to_json(&json!({
        "by": by,
        "formula": crate::signals::rank_formula(&snap.policy.config.rank.weights),
        "files": r.rows.len(),
        "matching": matching,
        "hotspot_percent": r.hotspot_percent,
        "hotspot_cutoff": cutoff,
        "generation": r.key.generation,
        "compute_us": r.compute_us,
        "rows": rows,
        "signals": signals,
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
