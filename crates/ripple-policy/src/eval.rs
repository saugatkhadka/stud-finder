//! Rule evaluation: impact query + `when` conditions -> [`Report`].
//!
//! Semantics, condition by condition (all conditions of a rule AND together;
//! a rule with an empty `when` always fires):
//! * `touches` / `touches_tier`: some changed file (its new or, for renames,
//!   old path) is in a listed area / matches a listed glob / is in an area of
//!   at least that tier. Ignored files never count.
//! * `reaches` / `reaches_tier` (OR-ed with each other): some *impacted*
//!   non-test file (depth >= 1, never a changed file) satisfies the same test,
//!   at depth <= `within` (default `query.max_depth`, 0 = unlimited).
//! * `impacted_files_over`: impacted non-test files (excluding changed) > N.
//! * `changed_files_over`: changed files (after `ignore`) > N.
//! * `changed_lines_over`: sum of added + deleted lines > N. Never fires when
//!   no changed file carries line counts (no diff available).
//! * `crosses_boundary: B`: holds iff (some impacted non-test file is on the
//!   other side of the Ruby/JS boundary from the changed file it traces back
//!   to) == B. Ruby and templates are the server side, JS/TS the client side.
//! * `touches_global: B`: holds iff (a changed file matches `global_files`) == B.
//! * `untested: B`: holds iff (some changed, non-deleted, non-test source
//!   file has no test file among its transitive dependents) == B.
//! * `min_confidence`: edges below it are ignored for this rule's `reaches*`,
//!   `impacted_files_over`, `crosses_boundary` and `untested`; default is the
//!   query's effective minimum (config `query.min_confidence` or CLI flag).

use crate::changed::{ChangedFile, FileStatus};
use crate::policy::{CompiledRule, FileClass, Policy, PolicyIndex};
use crate::report::*;
use ripple_core::{Confidence, Graph, Impact, NodeId, QueryOpts, Risk, Warning};
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::time::Instant;

/// Per-request overrides.
#[derive(Debug, Clone, Default)]
pub struct EvalOptions {
    /// Overrides `query.max_depth` (0 = unlimited).
    pub max_depth: Option<u32>,
    /// Overrides `query.min_confidence`.
    pub min_confidence: Option<Confidence>,
    /// Index generation to stamp on the report.
    pub generation: u64,
    /// Keep at most this many entries in `impacted.files` and `tests_to_run`.
    /// Counts and reasons are always computed over the full set.
    pub limit: Option<usize>,
}

/// Evaluates with default options, classifying the graph on the fly.
/// Long-lived callers should cache a [`PolicyIndex`] and use [`evaluate_with`].
pub fn evaluate(policy: &Policy, graph: &Graph, changed: &[ChangedFile]) -> Report {
    let index = PolicyIndex::build(policy, graph);
    evaluate_with(policy, graph, &index, changed, &EvalOptions::default())
}

/// One BFS result plus per-hit derived data (indexed like `impact.hits`).
struct Reach {
    conf: Confidence,
    /// BFS depth cap, 0 = unlimited.
    depth: u32,
    impact: Impact,
    /// Seed each hit traces back to.
    root: Vec<NodeId>,
    /// Weakest confidence along each hit's path (`Exact` for seeds).
    minc: Vec<Confidence>,
}

fn covers(have: u32, need: u32) -> bool {
    have == 0 || (need != 0 && need <= have)
}

fn within(depth: u32, limit: u32) -> bool {
    depth >= 1 && (limit == 0 || depth <= limit)
}

struct Changed {
    file: ChangedFile,
    class: FileClass,
    /// Areas of the new path and (for renames) the old path.
    touch_areas: Vec<u16>,
    node: Option<NodeId>,
}

struct Ctx<'a> {
    policy: &'a Policy,
    graph: &'a Graph,
    index: &'a PolicyIndex,
    changed: Vec<Changed>,
    seeds: Vec<NodeId>,
    eff: QueryOpts,
    reaches: Vec<Reach>,
    has_lines: bool,
}

pub fn evaluate_with(policy: &Policy, graph: &Graph, index: &PolicyIndex, changed: &[ChangedFile], opts: &EvalOptions) -> Report {
    let local;
    let index = if index.len() == graph.node_count() {
        index
    } else {
        local = PolicyIndex::build(policy, graph);
        &local
    };
    let started = Instant::now();
    let mut warnings = Vec::new();

    // Normalise the changed set: drop ignored, dedup, sort.
    let mut files: Vec<ChangedFile> = Vec::new();
    let mut seen = HashSet::new();
    for f in changed {
        if policy.is_ignored(&f.path) {
            warnings.push(Warning::new("changed_file_ignored", "matches `ignore` in ripple.yml; not analysed").file(&f.path));
            continue;
        }
        if seen.insert(f.path.clone()) {
            files.push(f.clone());
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let class_of = |path: &str| -> FileClass {
        match graph.id(path).and_then(|n| index.get(n)) {
            Some(c) => c.clone(),
            None => policy.classify(path),
        }
    };
    let mut seeds = Vec::new();
    let changed: Vec<Changed> = files
        .into_iter()
        .map(|file| {
            let class = class_of(&file.path);
            let node = graph.id(&file.path);
            let mut touch_areas = class.areas.clone();
            seeds.extend(node);
            if let Some(old) = &file.old_path {
                touch_areas.extend(class_of(old).areas);
                seeds.extend(graph.id(old));
            }
            touch_areas.sort_unstable();
            touch_areas.dedup();
            Changed { file, class, touch_areas, node }
        })
        .collect();
    let has_lines = changed.iter().any(|c| c.file.added.is_some() || c.file.deleted.is_some());

    let cfg = &policy.config.query;
    let eff = QueryOpts {
        max_depth: opts.max_depth.unwrap_or(cfg.max_depth),
        min_confidence: opts.min_confidence.unwrap_or(cfg.min_confidence),
        file_level: false,
    };
    let mut ctx = Ctx { policy, graph, index, changed, seeds, eff, reaches: Vec::new(), has_lines };

    let q0 = Instant::now();
    ctx.ensure_reach(eff.min_confidence, eff.max_depth);
    let query_us = q0.elapsed().as_micros() as u64;
    for r in &policy.rules {
        let (conf, depth) = ctx.rule_reach_needs(r);
        ctx.ensure_reach(conf, depth);
    }

    // Rules.
    let mut reasons = Vec::new();
    let mut untested_cache: BTreeMap<Confidence, Vec<String>> = BTreeMap::new();
    for r in &policy.rules {
        if let Some((evidence, message)) = ctx.eval_rule(r, &mut untested_cache) {
            reasons.push(Reason {
                rule: r.rule.id.clone(),
                risk: r.rule.risk,
                message,
                description: r.rule.description.clone(),
                require: r.rule.require.clone(),
                builtin: policy.uses_default_rules(),
                evidence,
            });
        }
        if r.rule.when.changed_lines_over.is_some() && !has_lines && !ctx.changed.is_empty() {
            warnings.push(Warning::new(
                "no_line_counts",
                format!("rule `{}` uses changed_lines_over but no diff line counts were provided (use --base or --staged)", r.rule.id),
            ));
        }
    }
    reasons.sort_by(|a, b| (std::cmp::Reverse(a.risk), &a.rule).cmp(&(std::cmp::Reverse(b.risk), &b.rule)));
    let risk = reasons.iter().map(|r| r.risk).max().unwrap_or(Risk::Low);
    let require: Vec<String> = reasons.iter().flat_map(|r| r.require.iter().cloned()).collect::<BTreeSet<_>>().into_iter().collect();
    // The built-in rules come in ladders (`touches_critical` > `touches_high`,
    // `impacted_over_200` > `impacted_over_50`, ...). When a higher rung fired,
    // the lower one says nothing new: keep the top of each ladder only.
    // Risk and `require` above were computed before this, so nothing is lost.
    if policy.uses_default_rules() {
        const LADDERS: [&str; 3] = ["default.touches_critical|default.touches_high", "default.impacted_over_", "default.reaches_critical"];
        let ladder = |id: &str| LADDERS.iter().position(|l| l.split('|').any(|p| id.starts_with(p)));
        let mut seen = BTreeSet::new();
        // `reasons` is sorted highest risk first.
        reasons.retain(|r| match ladder(&r.rule) {
            Some(l) => seen.insert(l),
            None => true,
        });
    }

    let main = &ctx.reaches[0];
    let changed_entries: Vec<ChangedEntry> = ctx
        .changed
        .iter()
        .map(|c| ChangedEntry {
            path: c.file.path.clone(),
            status: c.file.status,
            old_path: c.file.old_path.clone(),
            added: c.file.added,
            deleted: c.file.deleted,
            first_line: c.file.first_line,
            language: c.class.language,
            areas: policy.area_names(&c.class.areas),
            dependents: c.node.map(|n| graph.fan_in(n)).unwrap_or(0),
            in_graph: c.node.is_some(),
            is_test: c.class.test,
            is_global: c.class.global,
        })
        .collect();

    for c in &ctx.changed {
        if c.class.global {
            warnings.push(
                Warning::new(
                    "global_file_changed",
                    "global: static impact is not reliable for this file (it matches `global_files`; anything in the app may depend on it)",
                )
                .file(&c.file.path),
            );
        }
        if c.node.is_none() {
            if c.file.status == FileStatus::Deleted {
                warnings.push(
                    Warning::new(
                        "deleted_file_not_indexed",
                        "deleted file is not in the index, so the files that depended on it cannot be listed; their references to it are now broken",
                    )
                    .file(&c.file.path),
                );
            } else if c.class.language.is_source() {
                warnings.push(
                    Warning::new("changed_file_not_indexed", "file is not in the dependency graph; its impact is unknown, not empty")
                        .file(&c.file.path),
                );
            }
        }
    }

    // Impacted files and tests.
    let mut summary = ImpactedSummary::default();
    let mut tests_to_run: Vec<ImpactedFile> = ctx
        .changed
        .iter()
        .filter(|c| c.class.test)
        .map(|c| ImpactedFile {
            file: c.file.path.clone(),
            depth: 0,
            via: None,
            root: c.file.path.clone(),
            kind: None,
            confidence: None,
            path_confidence: None,
            line: None,
            detail: None,
            language: c.class.language,
            areas: policy.area_names(&c.class.areas),
        })
        .collect();
    // Entries past `limit` are counted but never materialised (hub seeds can
    // reach tens of thousands of files). Hits are in BFS order, so the kept
    // prefix is the nearest files.
    let cap = opts.limit.unwrap_or(usize::MAX);
    let mut tests_total = tests_to_run.len();
    for (i, hit) in main.impact.hits.iter().enumerate() {
        if hit.depth == 0 {
            continue;
        }
        let class = ctx.class(hit.node);
        let (_, _, data) = graph.edge(hit.edge);
        let entry = || ImpactedFile {
            file: graph.path(hit.node).to_string(),
            depth: hit.depth,
            via: Some(graph.path(hit.via).to_string()),
            root: graph.path(main.root[i]).to_string(),
            kind: Some(data.kind),
            confidence: Some(data.confidence),
            path_confidence: Some(main.minc[i]),
            line: (data.line != 0).then_some(data.line),
            detail: graph.edge_detail(hit.edge).map(str::to_string),
            language: class.language,
            areas: policy.area_names(&class.areas),
        };
        summary.max_depth = summary.max_depth.max(hit.depth);
        if class.test {
            summary.tests += 1;
            tests_total += 1;
            if tests_to_run.len() < cap {
                tests_to_run.push(entry());
            }
        } else {
            summary.total += 1;
            *summary.by_depth.entry(hit.depth).or_default() += 1;
            *summary.by_language.entry(class.language).or_default() += 1;
            *summary.by_edge_kind.entry(data.kind).or_default() += 1;
            *summary.by_min_confidence.entry(main.minc[i]).or_default() += 1;
            if summary.files.len() < cap {
                summary.files.push(entry());
            }
        }
    }
    if let Some(limit) = opts.limit {
        tests_to_run.truncate(limit); // changed test files may exceed it on their own
        summary.truncated = summary.total > summary.files.len() || tests_total > tests_to_run.len();
    }

    // Areas reached: changed files first (depth 0), then impacted non-test files in BFS order.
    let mut areas: BTreeMap<u16, AreaReached> = BTreeMap::new();
    let mut bump = |area: u16, depth: u32, example: &dyn Fn() -> ImpactPath| {
        let a = policy.area(area);
        let e = areas.entry(area).or_insert_with(|| AreaReached {
            area: a.name.clone(),
            tier: a.tier,
            owners: a.owners.clone(),
            touched: false,
            files: 0,
            min_depth: depth,
            example: example(),
        });
        e.files += 1;
        e.touched |= depth == 0;
    };
    for c in ctx.changed.iter().filter(|c| !c.class.test) {
        let ex = || ImpactPath { file: c.file.path.clone(), depth: 0, path: vec![hop0(&c.file.path)] };
        for &a in &c.touch_areas {
            bump(a, 0, &ex);
        }
    }
    for hit in main.impact.hits.iter().filter(|h| h.depth > 0) {
        let class = ctx.class(hit.node);
        if class.test {
            continue;
        }
        for &a in &class.areas {
            bump(a, hit.depth, &|| ctx.impact_path(main, hit.node));
        }
    }
    let mut areas_reached: Vec<AreaReached> = areas.into_values().collect();
    areas_reached.sort_by(|a, b| (std::cmp::Reverse(a.tier), &a.area).cmp(&(std::cmp::Reverse(b.tier), &b.area)));
    let owners: Vec<String> = areas_reached.iter().flat_map(|a| a.owners.iter().cloned()).collect::<BTreeSet<_>>().into_iter().collect();

    warnings.sort_by(|a, b| (&a.code, &a.file, &a.message).cmp(&(&b.code, &b.file, &b.message)));
    warnings.dedup();

    let summary_line = format!(
        "{} risk: {} changed file{}, {} impacted file{}{}, {} test{} to run, {} rule{} fired",
        risk_word(risk),
        changed_entries.len(),
        plural(changed_entries.len()),
        summary.total,
        plural(summary.total),
        if summary.total > 0 { format!(" (up to {} hop{})", summary.max_depth, plural(summary.max_depth as usize)) } else { String::new() },
        tests_total,
        plural(tests_total),
        reasons.len(),
        plural(reasons.len()),
    );

    let total_us = started.elapsed().as_micros() as u64;
    Report {
        schema_version: SCHEMA_VERSION,
        risk,
        summary: summary_line,
        reasons,
        require,
        changed: changed_entries,
        impacted: summary,
        tests_to_run,
        areas_reached,
        owners,
        warnings,
        query: QueryInfo { max_depth: eff.max_depth, min_confidence: eff.min_confidence },
        timing: Timing { refresh_us: 0, query_us, evaluate_us: total_us.saturating_sub(query_us) },
        index: IndexInfo { generation: opts.generation, nodes: graph.node_count(), edges: graph.edge_count(), pending_changes: 0 },
    }
}

pub(crate) fn risk_word(r: Risk) -> &'static str {
    match r {
        Risk::Low => "LOW",
        Risk::Medium => "MEDIUM",
        Risk::High => "HIGH",
        Risk::Critical => "CRITICAL",
    }
}

pub(crate) fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn hop0(file: &str) -> Hop {
    Hop { file: file.to_string(), kind: None, confidence: None, line: None, detail: None }
}

fn tier_word(r: Risk) -> &'static str {
    match r {
        Risk::Low => "low",
        Risk::Medium => "medium",
        Risk::High => "high",
        Risk::Critical => "critical",
    }
}

fn list(items: &[String], max: usize) -> String {
    let mut s: Vec<String> = items.iter().take(max).map(|i| format!("`{i}`")).collect();
    if items.len() > max {
        s.push(format!("and {} more", items.len() - max));
    }
    s.join(", ")
}

impl Ctx<'_> {
    fn class(&self, node: NodeId) -> &FileClass {
        self.index.get(node).expect("policy index matches graph")
    }

    fn ensure_reach(&mut self, conf: Confidence, depth: u32) -> usize {
        if let Some(i) = self.reaches.iter().position(|r| r.conf == conf && covers(r.depth, depth)) {
            return i;
        }
        let impact = self.graph.impact(&self.seeds, QueryOpts { max_depth: depth, min_confidence: conf, file_level: false });
        let mut root = Vec::with_capacity(impact.hits.len());
        let mut minc = Vec::with_capacity(impact.hits.len());
        // Hits are in BFS order, so a hit's `via` always precedes it.
        let mut pos = vec![u32::MAX; self.graph.node_count()];
        for (i, h) in impact.hits.iter().enumerate() {
            pos[h.node as usize] = i as u32;
            if h.depth == 0 {
                root.push(h.node);
                minc.push(Confidence::Exact);
            } else {
                let p = pos[h.via as usize] as usize;
                root.push(root[p]);
                minc.push(minc[p].min(self.graph.edge(h.edge).2.confidence));
            }
        }
        self.reaches.push(Reach { conf, depth, impact, root, minc });
        self.reaches.len() - 1
    }

    fn rule_reach_needs(&self, r: &CompiledRule) -> (Confidence, u32) {
        let w = &r.rule.when;
        let conf = w.min_confidence.unwrap_or(self.eff.min_confidence);
        let d = self.eff.max_depth;
        // One BFS must serve both `within` (reaches*) and the query depth (the rest).
        let depth = match w.within {
            Some(within) if d != 0 => within.max(d),
            _ => d,
        };
        (conf, depth)
    }

    /// Index of a computed BFS at `conf` deep enough for `depth`.
    fn reach_idx(&self, conf: Confidence, depth: u32) -> usize {
        self.reaches.iter().position(|r| r.conf == conf && covers(r.depth, depth)).expect("reach computed up front")
    }

    fn impact_path(&self, reach: &Reach, node: NodeId) -> ImpactPath {
        let chain = reach.impact.path(node);
        let path = chain
            .iter()
            .map(|&(n, e)| {
                if e == u32::MAX {
                    hop0(self.graph.path(n))
                } else {
                    let (_, _, d) = self.graph.edge(e);
                    Hop {
                        file: self.graph.path(n).to_string(),
                        kind: Some(d.kind),
                        confidence: Some(d.confidence),
                        line: (d.line != 0).then_some(d.line),
                        detail: self.graph.edge_detail(e).map(str::to_string),
                    }
                }
            })
            .collect();
        ImpactPath { file: self.graph.path(node).to_string(), depth: reach.impact.get(node).map(|h| h.depth).unwrap_or(0), path }
    }

    /// Impacted non-test hits within `limit` hops, in BFS order: (hit index, node).
    fn impacted<'r>(&'r self, reach: &'r Reach, limit: u32) -> impl Iterator<Item = (usize, NodeId)> + 'r {
        reach
            .impact
            .hits
            .iter()
            .enumerate()
            .filter(move |(_, h)| within(h.depth, limit) && !self.class(h.node).test)
            .map(|(i, h)| (i, h.node))
    }

    /// "billing (critical), auth (high)".
    fn area_list(&self, areas: &BTreeSet<u16>) -> String {
        areas.iter().map(|&a| format!("{} ({})", self.policy.area(a).name, tier_word(self.policy.area(a).tier))).collect::<Vec<_>>().join(", ")
    }

    fn target_desc(&self, entries: &[String]) -> String {
        let names: Vec<String> = entries.iter().map(|e| if self.policy.config.areas.contains_key(e) { format!("area {e}") } else { format!("`{e}`") }).collect();
        names.join(", ")
    }

    fn untested(&self, conf: Confidence) -> Vec<String> {
        let mut out = Vec::new();
        for c in &self.changed {
            if c.class.test || !c.class.language.is_source() || c.file.status == FileStatus::Deleted {
                continue;
            }
            let tested = c.node.is_some_and(|n| self.has_test_dependent(n, conf, self.eff.max_depth));
            if !tested {
                out.push(c.file.path.clone());
            }
        }
        out
    }

    /// Reverse BFS from `node` that stops at the first test file.
    fn has_test_dependent(&self, node: NodeId, conf: Confidence, max_depth: u32) -> bool {
        let mut seen = HashSet::from([node]);
        let mut queue = VecDeque::from([(node, 0u32)]);
        while let Some((n, d)) = queue.pop_front() {
            if max_depth != 0 && d >= max_depth {
                continue;
            }
            for (next, e) in self.graph.dependents(n) {
                if self.graph.edge(e).2.confidence < conf || !seen.insert(next) {
                    continue;
                }
                if self.class(next).test {
                    return true;
                }
                queue.push_back((next, d + 1));
            }
        }
        false
    }

    fn eval_rule(&self, r: &CompiledRule, untested_cache: &mut BTreeMap<Confidence, Vec<String>>) -> Option<(Evidence, String)> {
        let w = &r.rule.when;
        let mut ev = Evidence::default();
        let mut parts: Vec<String> = Vec::new();
        let mut changed_ev: BTreeSet<String> = BTreeSet::new();
        let mut impacted_ev: Vec<(usize, NodeId)> = Vec::new();
        let conf = w.min_confidence.unwrap_or(self.eff.min_confidence);

        if !w.touches.is_empty() {
            let hits: Vec<String> = self
                .changed
                .iter()
                .filter(|c| {
                    r.touches.matches(&c.file.path, &c.touch_areas) || c.file.old_path.as_deref().is_some_and(|o| r.touches.globs_match(o))
                })
                .map(|c| c.file.path.clone())
                .collect();
            if hits.is_empty() {
                return None;
            }
            parts.push(format!("touches {}: {}", self.target_desc(&w.touches), list(&hits, 3)));
            changed_ev.extend(hits);
        }
        if let Some(tier) = w.touches_tier {
            let mut names = BTreeSet::new();
            let hits: Vec<String> = self
                .changed
                .iter()
                .filter(|c| {
                    let found: Vec<u16> = c.touch_areas.iter().copied().filter(|&a| self.policy.area(a).tier >= tier).collect();
                    names.extend(found.iter().copied());
                    !found.is_empty()
                })
                .map(|c| c.file.path.clone())
                .collect();
            if hits.is_empty() {
                return None;
            }
            parts.push(format!("changes {} in {}", list(&hits, 3), self.area_list(&names)));
            changed_ev.extend(hits);
        }
        if !w.reaches.is_empty() || w.reaches_tier.is_some() {
            let limit = w.within.unwrap_or(self.eff.max_depth);
            let ri = self.reach_idx(conf, limit);
            let reach = &self.reaches[ri];
            let mut names = BTreeSet::new();
            let hits: Vec<(usize, NodeId)> = self
                .impacted(reach, limit)
                .filter(|&(_, n)| {
                    let class = self.class(n);
                    let by_target = !r.reaches.is_empty() && r.reaches.matches(self.graph.path(n), &class.areas);
                    let by_tier: Vec<u16> = match w.reaches_tier {
                        Some(t) => class.areas.iter().copied().filter(|&a| self.policy.area(a).tier >= t).collect(),
                        None => Vec::new(),
                    };
                    if by_target {
                        names.extend(class.areas.iter().copied().filter(|a| r.reaches.areas.contains(a)));
                    }
                    names.extend(by_tier.iter().copied());
                    by_target || !by_tier.is_empty()
                })
                .collect();
            if hits.is_empty() {
                return None;
            }
            let what = if names.is_empty() { format!("matching {}", self.target_desc(&w.reaches)) } else { format!("in {}", self.area_list(&names)) };
            parts.push(format!(
                "reaches {} file{} {}{}",
                hits.len(),
                plural(hits.len()),
                what,
                match w.within {
                    Some(1) => " within 1 hop".to_string(),
                    Some(n) => format!(" within {n} hops"),
                    None => String::new(),
                }
            ));
            ev.impacted_matching = ev.impacted_matching.max(hits.len());
            for &(i, _) in &hits {
                changed_ev.insert(self.graph.path(reach.root[i]).to_string());
            }
            impacted_ev.extend(hits.iter().take(EVIDENCE_LIMIT).map(|&(_, n)| (ri, n)));
        }
        if let Some(n) = w.impacted_files_over {
            let reach = &self.reaches[self.reach_idx(conf, self.eff.max_depth)];
            let count = self.impacted(reach, self.eff.max_depth).count();
            if count <= n {
                return None;
            }
            parts.push(format!("{count} files impacted (more than {n})"));
            ev.counts.insert("impacted_files".into(), count);
        }
        if let Some(n) = w.changed_files_over {
            let count = self.changed.len();
            if count <= n {
                return None;
            }
            parts.push(format!("{count} files changed (more than {n})"));
            ev.counts.insert("changed_files".into(), count);
        }
        if let Some(n) = w.changed_lines_over {
            if !self.has_lines {
                return None;
            }
            let count: usize = self.changed.iter().map(|c| c.file.added.unwrap_or(0) as usize + c.file.deleted.unwrap_or(0) as usize).sum();
            if count <= n {
                return None;
            }
            parts.push(format!("{count} lines changed (more than {n})"));
            ev.counts.insert("changed_lines".into(), count);
        }
        if let Some(want) = w.crosses_boundary {
            let ri = self.reach_idx(conf, self.eff.max_depth);
            let reach = &self.reaches[ri];
            let crossing: Vec<(usize, NodeId)> = self
                .impacted(reach, self.eff.max_depth)
                .filter(|&(i, n)| {
                    let a = self.class(n).language.side();
                    let b = self.class(reach.root[i]).language.side();
                    a.is_some() && b.is_some() && a != b
                })
                .collect();
            if crossing.is_empty() == want {
                return None;
            }
            if want {
                let langs: BTreeSet<&str> = crossing.iter().map(|&(_, n)| self.class(n).language.as_str()).collect();
                parts.push(format!(
                    "crosses the Ruby/JS boundary: {} {} file{} on the other side depend on the change",
                    crossing.len(),
                    langs.into_iter().collect::<Vec<_>>().join("/"),
                    plural(crossing.len())
                ));
                ev.impacted_matching = ev.impacted_matching.max(crossing.len());
                for &(i, _) in &crossing {
                    changed_ev.insert(self.graph.path(reach.root[i]).to_string());
                }
                impacted_ev.extend(crossing.iter().take(EVIDENCE_LIMIT).map(|&(_, n)| (ri, n)));
            } else {
                parts.push("stays on one side of the Ruby/JS boundary".into());
            }
        }
        if let Some(want) = w.touches_global {
            let globals: Vec<String> = self.changed.iter().filter(|c| c.class.global).map(|c| c.file.path.clone()).collect();
            if globals.is_empty() == want {
                return None;
            }
            if want {
                parts.push(format!("changes global file{} {} (static impact is not reliable)", plural(globals.len()), list(&globals, 3)));
                changed_ev.extend(globals);
            } else {
                parts.push("changes no global files".into());
            }
        }
        if let Some(want) = w.untested {
            let untested = untested_cache.entry(conf).or_insert_with(|| self.untested(conf)).clone();
            if untested.is_empty() == want {
                return None;
            }
            if want {
                parts.push(format!("no test depends on {}", list(&untested, 3)));
                changed_ev.extend(untested);
            } else {
                parts.push("every changed source file has a dependent test".into());
            }
        }
        if parts.is_empty() {
            parts.push("always applies".into());
        }

        // Evidence: closest impacted files first, deduplicated.
        let mut seen = HashSet::new();
        let mut paths: Vec<ImpactPath> = impacted_ev
            .into_iter()
            .filter(|&(_, n)| seen.insert(n))
            .map(|(ri, n)| self.impact_path(&self.reaches[ri], n))
            .collect();
        paths.sort_by(|a, b| (a.depth, &a.file).cmp(&(b.depth, &b.file)));
        paths.truncate(EVIDENCE_LIMIT);
        ev.impacted = paths;
        ev.changed = changed_ev.into_iter().collect();
        let mut message = parts.join("; ");
        if let Some(first) = message.get(..1) {
            message = first.to_uppercase() + &message[1..];
        }
        Some((ev, message))
    }
}

#[cfg(test)]
mod tests;
