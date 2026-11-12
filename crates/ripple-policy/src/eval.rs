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
//!   no changed file carries line counts (no diff available). Next to a
//!   per-file condition (below) it is the lines of that same file instead.
//! * `crosses_boundary: B`: holds iff (some impacted non-test file is on the
//!   other side of the Ruby/JS boundary from the changed file it traces back
//!   to) == B. Ruby and templates are the server side, JS/TS the client side.
//! * `touches_global: B`: holds iff (a changed file matches `global_files`) == B.
//! * `untested: B`: holds iff (some changed, non-deleted, non-test source
//!   file has no test file among its transitive dependents) == B. With
//!   `true` next to a per-file condition, it must be that same file.
//! * Signal conditions look at the changed, non-deleted, non-test source
//!   files ("candidates") and need their signal ([`SignalInput`]) to be
//!   ready; a rule using a signal that is not ready does not fire and adds a
//!   `rule_signal_not_ready` warning instead. Thresholds are exclusive.
//!   **Per-file conditions** (`churn_over`, `complexity_over`,
//!   `max_method_complexity_over`, `coverage_under`,
//!   `changed_lines_coverage_under`, `uncovered: true`, `hotspot: true`,
//!   and with them `untested: true` and `changed_lines_over`) must all hold
//!   on one and the same candidate; the reason names the files that
//!   satisfy all of them ([`per_file_group`]). `uncovered: false` and
//!   `hotspot: false` are about every candidate.
//!   * `churn_over: N`: a candidate changed in more than N commits in the
//!     last 90 days of the history window.
//!   * `complexity_over: N` / `max_method_complexity_over: N`: a candidate's
//!     file complexity / most complex method is above N.
//!   * `coverage_under: P`: a candidate with known coverage is below P%.
//!     Unknown coverage (the file is in no report) never counts.
//!   * `changed_lines_coverage_under: P`: the coverable lines a candidate's
//!     diff added or changed are covered below P%. Needs diff hunks.
//!   * `uncovered: B`: holds iff (a candidate has known 0% coverage, or with
//!     `uncovered_includes_unknown` is in no report) == B.
//!   * `hotspot: B`: holds iff (a candidate is in the top
//!     `rank.hotspot_percent` of `ripple rank`) == B.
//!   * `missing_cochange: B`: holds iff (a file outside the diff changed
//!     together with a changed file in at least `cochange_min_confidence`
//!     (default 0.5) of that file's commits) == B.
//! * `min_confidence`: edges below it are ignored for this rule's `reaches*`,
//!   `impacted_files_over`, `crosses_boundary` and `untested`; default is the
//!   query's effective minimum (config `query.min_confidence` or CLI flag).
//!
//! **Method-level seeds.** A [`ChangedFile`] whose `scopes` is `Some(names)`
//! seeds every impact query above with only those methods / actions (see
//! [`ripple_core::Graph::impact_scoped`]); `None` seeds the whole file. With
//! [`EvalOptions::file_level`] every seed is whole-file and edge scopes are
//! ignored. `untested` follows the same seeds: a whole-file seed keeps the
//! plain "any test among the file's dependents" walk, a scoped seed asks
//! whether a test depends on one of the changed methods. The per-file
//! `dependents` count in the report is always the file-level fan-in.

use crate::changed::{ChangedFile, FileStatus};
use crate::policy::{CompiledRule, FileClass, Policy, PolicyIndex};
use crate::report::*;
use crate::signals::{Metrics, SignalInput, SignalState, SignalStatus};
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
    /// Cut every list to at most this many entries instead of its default
    /// budget ([`crate::report::budget`]). Counts and rules always cover the full set.
    pub limit: Option<usize>,
    /// No list budget: every dependent, further file and test is listed.
    pub all: bool,
    /// Always include the policy section (risk, rules, owners, areas), with
    /// full evidence. Otherwise it is included, compactly, only when the
    /// config defines rules or areas and a rule fired.
    pub policy: bool,
    /// Ignore method / action scopes: every changed file is wholly changed.
    pub file_level: bool,
    /// Repository root: lets the report give the line where a removed
    /// method is still mentioned. `None` = no file reads.
    pub root: Option<std::path::PathBuf>,
    /// Churn, coverage, co-change and hotspot data and the state of each
    /// signal. `None`: only complexity (from the graph) is known.
    pub signals: Option<SignalInput>,
}

impl EvalOptions {
    /// The size of a list whose default budget is `default`.
    fn cap(&self, default: usize) -> usize {
        match (self.all, self.limit) {
            (true, None) => usize::MAX,
            (_, Some(l)) => l,
            (false, None) => default,
        }
    }
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

pub(crate) struct Changed {
    pub(crate) file: ChangedFile,
    pub(crate) class: FileClass,
    /// Areas of the new path and (for renames) the old path.
    touch_areas: Vec<u16>,
    pub(crate) node: Option<NodeId>,
    /// Effective seed scopes (`None` = whole file).
    scopes: Option<Vec<String>>,
}

pub(crate) struct Ctx<'a> {
    pub(crate) policy: &'a Policy,
    pub(crate) graph: &'a Graph,
    index: &'a PolicyIndex,
    pub(crate) changed: Vec<Changed>,
    seeds: Vec<(NodeId, Option<Vec<String>>)>,
    pub(crate) eff: QueryOpts,
    reaches: Vec<Reach>,
    has_lines: bool,
    sig: Option<&'a SignalInput>,
    /// The state of every signal (from `sig`, plus complexity from the graph).
    status: crate::signals::Signals,
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
        if crate::load::CONFIG_FILE_NAMES.contains(&f.path.as_str()) {
            warnings.push(Warning::new("ripple_config_changed", "ripple's own config: not analysed for impact").file(&f.path));
            continue;
        }
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
            let scopes = if opts.file_level || file.status != FileStatus::Modified { None } else { file.scopes.clone() };
            seeds.extend(node.map(|n| (n, scopes.clone())));
            if let Some(old) = &file.old_path {
                touch_areas.extend(class_of(old).areas);
                seeds.extend(graph.id(old).map(|n| (n, None)));
            }
            touch_areas.sort_unstable();
            touch_areas.dedup();
            Changed { file, class, touch_areas, node, scopes }
        })
        .collect();
    let has_lines = changed.iter().any(|c| c.file.added.is_some() || c.file.deleted.is_some());

    let cfg = &policy.config.query;
    let eff = QueryOpts {
        max_depth: opts.max_depth.unwrap_or(cfg.max_depth),
        min_confidence: opts.min_confidence.unwrap_or(cfg.min_confidence),
        file_level: opts.file_level,
    };
    let status = signal_status(graph, opts.signals.as_ref());
    let mut ctx = Ctx { policy, graph, index, changed, seeds, eff, reaches: Vec::new(), has_lines, sig: opts.signals.as_ref(), status };

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
        let blocked = ctx.blocked_signals(&r.rule.when);
        if !blocked.is_empty() {
            if !ctx.changed.is_empty() {
                let what: Vec<String> = blocked
                    .iter()
                    .map(|(name, st)| match &st.reason {
                        Some(why) => format!("{name} ({}: {why})", st.state.as_str()),
                        None => format!("{name} ({})", st.state.as_str()),
                    })
                    .collect();
                warnings.push(Warning::new("rule_signal_not_ready", format!("rule `{}` was not evaluated: it needs {}", r.rule.id, what.join(", "))));
            }
            continue;
        }
        if r.rule.when.changed_lines_coverage_under.is_some() && !has_lines && !ctx.changed.is_empty() {
            warnings.push(Warning::new(
                "no_line_counts",
                format!("rule `{}` uses changed_lines_coverage_under but no diff was provided (use --base or --staged)", r.rule.id),
            ));
        }
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
    let mut truncated = false;
    let mut counts = Counts { changed: ctx.changed.len(), ..Default::default() };
    let changed_entries: Vec<ChangedEntry> = ctx
        .changed
        .iter()
        .take(opts.cap(budget::CHANGED))
        .map(|c| ChangedEntry {
            path: c.file.path.clone(),
            status: c.file.status,
            old_path: c.file.old_path.clone(),
            added: c.file.added,
            deleted: c.file.deleted,
            first_line: c.file.first_line,
            scopes: c.scopes.clone(),
            change: c.file.change.clone().unwrap_or_else(|| crate::changed::classify_change(&c.file, None, None)),
            language: c.class.language,
            areas: policy.area_names(&c.class.areas),
            dependents: c.node.map(|n| graph.fan_in(n)).unwrap_or(0),
            in_graph: c.node.is_some(),
            is_test: c.class.test,
            is_global: c.class.global,
            metrics: ctx.metrics_of(c),
        })
        .collect();
    truncated |= changed_entries.len() < ctx.changed.len();

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

    // Method names per changed node that dependents may use by name
    // (changed, signature-changed and removed methods; else `--scope`).
    let mut used_names: BTreeMap<NodeId, Vec<String>> = BTreeMap::new();
    let mut fresh_names: Vec<crate::findings::NamesOf> = Vec::new();
    let mut singletons: BTreeMap<NodeId, Vec<String>> = BTreeMap::new();
    for c in &ctx.changed {
        let Some(n) = c.node else { continue };
        let (mut v, removed): (Vec<String>, &[String]) = match &c.file.change {
            Some(ch) => (ch.methods.iter().chain(&ch.signatures).chain(&ch.removed_methods).cloned().collect(), &ch.removed_methods),
            None => (Vec::new(), &[]),
        };
        if v.is_empty() {
            v.extend(c.scopes.iter().flatten().filter(|s| *s != ripple_core::model::SCOPE_BODY).cloned());
        }
        v.sort_unstable();
        v.dedup();
        if v.is_empty() {
            continue;
        }
        if let Some(text) = crate::findings::read(opts.root.as_deref(), &c.file.path) {
            let s = crate::findings::singleton_names(&text, &v);
            if !s.is_empty() {
                singletons.insert(n, s);
            }
        }
        let fresh: Vec<String> = v.iter().filter(|x| !removed.contains(x)).cloned().collect();
        if !fresh.is_empty() {
            let single = singletons.get(&n).cloned().unwrap_or_default();
            fresh_names.push(crate::findings::NamesOf { node: n, file: c.file.path.clone(), names: fresh, singleton: single });
        }
        used_names.insert(n, v);
    }

    // Walk the main BFS once: direct dependents, further files, tests.
    let mut deps: Vec<usize> = Vec::new();
    let mut further = Further::default();
    let mut further_listed: Vec<usize> = Vec::new();
    let mut tests: Vec<TestFile> = ctx
        .changed
        .iter()
        .filter(|c| c.class.test)
        .map(|c| TestFile { file: c.file.path.clone(), depth: 0, of: None, via: None, kind: None, line: None, detail: None, mention_line: None })
        .collect();
    counts.tests = tests.len();
    tests.truncate(opts.cap(budget::TESTS));
    counts.tests_direct = tests.len();
    let test_cap = opts.cap(budget::TESTS);
    let mut test_hits: Vec<usize> = Vec::new();
    let further_cap = opts.cap(budget::FURTHER);
    let crosses = |i: usize, node: NodeId| {
        let a = ctx.class(node).language.side();
        let b = ctx.class(main.root[i]).language.side();
        a.is_some() && b.is_some() && a != b
    };
    for (i, hit) in main.impact.hits.iter().enumerate() {
        if hit.depth == 0 {
            continue;
        }
        let class = ctx.class(hit.node);
        if class.test {
            counts.tests += 1;
            if hit.depth == 1 {
                counts.tests_direct += 1;
            }
            if opts.all || hit.depth == 1 {
                test_hits.push(i);
            }
            continue;
        }
        if hit.depth == 1 {
            deps.push(i);
            continue;
        }
        counts.further += 1;
        *further.by_depth.entry(hit.depth).or_default() += 1;
        *further.by_language.entry(class.language).or_default() += 1;
        for &a in &class.areas {
            *further.by_area.entry(policy.area(a).name.clone()).or_default() += 1;
        }
        if let Some(t) = policy.max_tier(&class.areas) {
            *further.by_tier.entry(t).or_default() += 1;
        }
        let notable = notable_reasons(crosses(i, hit.node), policy.max_tier(&class.areas), class.global);
        if !notable.is_empty() {
            counts.further_notable += 1;
        }
        if (opts.all || !notable.is_empty()) && further_listed.len() < further_cap {
            further_listed.push(i);
        }
    }
    counts.dependents = deps.len();
    counts.impacted = counts.dependents + counts.further;

    // Which changed methods a hop-1 file mentions, and the line where it
    // calls a changed singleton method on the class constant.
    let mention_info = |i: usize| -> (Vec<String>, Vec<u32>) {
        let hit = &main.impact.hits[i];
        let seed = main.root[i];
        let uses: Vec<String> = match (used_names.get(&seed), graph.mentions(hit.node)) {
            (Some(names), Some(m)) => names.iter().filter(|n| m.mentions_any(&[ripple_core::mentions::name_id(n)])).cloned().collect(),
            _ => Vec::new(),
        };
        let on_const = match singletons.get(&seed) {
            Some(single) if uses.iter().any(|u| single.contains(u)) => {
                let consts = graph.edge_detail(hit.edge).map(crate::findings::detail_constants).unwrap_or_default();
                let mut lines: Vec<u32> = crate::findings::read(opts.root.as_deref(), graph.path(hit.node))
                    .map(|t| single.iter().filter(|u| uses.contains(u)).flat_map(|u| crate::findings::constant_call_lines(&t, &consts, u, 10)).collect())
                    .unwrap_or_default();
                lines.sort_unstable();
                lines.dedup();
                lines.truncate(10);
                lines
            }
            _ => Vec::new(),
        };
        (uses, on_const)
    };
    let first_mention = |path: &str, uses: &[String]| -> Option<u32> {
        if uses.is_empty() {
            return None;
        }
        crate::findings::read(opts.root.as_deref(), path).and_then(|t| uses.iter().filter_map(|u| crate::findings::mention_line(&t, u)).min())
    };
    // Tests at hop 1: callers of the changed methods first.
    let mut ranked: Vec<(usize, Vec<String>, Vec<u32>)> = test_hits
        .iter()
        .map(|&i| if main.impact.hits[i].depth == 1 && !used_names.is_empty() { let (u, c) = mention_info(i); (i, u, c) } else { (i, Vec::new(), Vec::new()) })
        .collect();
    ranked.sort_by(|a, b| {
        let k = |x: &(usize, Vec<String>, Vec<u32>)| (main.impact.hits[x.0].depth, x.2.is_empty(), x.1.is_empty());
        (k(a), graph.path(main.impact.hits[a.0].node)).cmp(&(k(b), graph.path(main.impact.hits[b.0].node)))
    });
    for (i, uses, on_const) in ranked.into_iter().take(test_cap.saturating_sub(tests.len())) {
        let hit = &main.impact.hits[i];
        let (_, _, data) = graph.edge(hit.edge);
        let path = graph.path(hit.node);
        tests.push(TestFile {
            file: path.to_string(),
            depth: hit.depth,
            of: Some(graph.path(main.root[i]).to_string()),
            via: (hit.depth > 1).then(|| graph.path(hit.via).to_string()),
            kind: Some(data.kind),
            line: (data.line != 0).then_some(data.line),
            detail: graph.edge_detail(hit.edge).map(str::to_string),
            mention_line: on_const.first().copied().or_else(|| first_mention(path, &uses)),
        });
    }
    truncated |= counts.tests > tests.len();

    // Direct dependents: which changed methods each mentions, and whether a
    // singleton method is called on the class constant.
    struct Dep {
        i: usize,
        uses: Vec<String>,
        on_const: Vec<u32>,
        inherit: bool,
        cross: bool,
    }
    let mut dep_info: Vec<Dep> = deps
        .iter()
        .map(|&i| {
            let hit = &main.impact.hits[i];
            let (uses, on_const) = mention_info(i);
            Dep { i, uses, on_const, inherit: graph.edge_inherits(hit.edge), cross: crosses(i, hit.node) }
        })
        .collect();
    // Heirs / includers that never mention a changed method: grouped apart.
    let mut inherit_only: Vec<String> = Vec::new();
    dep_info.retain(|d| {
        let scoped = used_names.contains_key(&main.root[d.i]);
        let known = graph.mentions(main.impact.hits[d.i].node).is_some();
        if scoped && known && d.inherit && d.uses.is_empty() {
            inherit_only.push(graph.path(main.impact.hits[d.i].node).to_string());
            false
        } else {
            true
        }
    });
    inherit_only.sort();
    counts.inherit_only = inherit_only.len();
    let inherit_cap = opts.cap(budget::INHERIT_ONLY);
    truncated |= inherit_only.len() > inherit_cap;
    inherit_only.truncate(inherit_cap);
    let key = |d: &Dep| {
        let hit = &main.impact.hits[d.i];
        let class = ctx.class(hit.node);
        (
            d.on_const.is_empty(),
            d.uses.is_empty(),
            !d.cross,
            std::cmp::Reverse(policy.max_tier(&class.areas).filter(|t| *t >= Risk::High)),
            std::cmp::Reverse(graph.edge(hit.edge).2.confidence),
        )
    };
    dep_info.sort_by(|a, b| (key(a), graph.path(main.impact.hits[a.i].node)).cmp(&(key(b), graph.path(main.impact.hits[b.i].node))));
    let dep_cap = opts.cap(budget::DEPENDENTS);
    truncated |= dep_info.len() > dep_cap;
    let dependents: Vec<Dependent> = dep_info
        .iter()
        .take(dep_cap)
        .map(|d| {
            let i = d.i;
            let hit = &main.impact.hits[i];
            let (_, _, data) = graph.edge(hit.edge);
            let class = ctx.class(hit.node);
            let (from_scope, to_scope, inherit) = shown_scopes(graph, hit.edge);
            let path = graph.path(hit.node);
            let of = graph.path(main.root[i]);
            let mut detail = graph.edge_detail(hit.edge).map(str::to_string);
            let mut line = (data.line != 0).then_some(data.line);
            let mut mention_line = d.on_const.first().copied();
            if data.kind == ripple_core::EdgeKind::RailsRoute
                && let Some(ctrl) = crate::findings::controller_name(of)
                && let Some(text) = crate::findings::read(opts.root.as_deref(), path)
            {
                // The route of the changed action, not the first route of the controller.
                let routes = crate::findings::route_lines(&text, ctrl);
                let actions: Vec<String> = used_names.get(&main.root[i]).cloned().unwrap_or_default();
                match routes.iter().find(|(_, a)| actions.contains(a)) {
                    Some((l, _)) => {
                        mention_line = Some(*l);
                        detail = text.lines().nth(*l as usize - 1).map(|t| t.trim().chars().take(120).collect());
                    }
                    None if actions.is_empty() && routes.len() > 1 => {
                        let mut acts: Vec<&str> = routes.iter().map(|(_, a)| a.as_str()).collect();
                        acts.sort_unstable();
                        acts.dedup();
                        detail = Some(format!("routes ({} actions)", acts.len()));
                        line = None;
                    }
                    None => {}
                }
            } else if mention_line.is_none() {
                mention_line = first_mention(path, &d.uses);
            }
            Dependent {
                file: path.to_string(),
                line,
                kind: data.kind,
                detail,
                confidence: data.confidence,
                of: of.to_string(),
                from_scope,
                to_scope,
                inherit,
                uses_methods: d.uses.clone(),
                mention_line,
                calls_on_constant: d.on_const.clone(),
                language: class.language,
                areas: policy.area_names(&class.areas),
                crosses_boundary: d.cross,
            }
        })
        .collect();

    // Files mentioning a changed method name with no edge to it.
    let mut exclude: BTreeSet<NodeId> = ctx.seeds.iter().map(|(n, _)| *n).collect();
    exclude.extend(main.impact.hits.iter().filter(|h| h.depth <= 1).map(|h| h.node));
    let (also_mentioned, also_total) = crate::findings::also_mentioned(&ctx, opts, &fresh_names, &exclude);
    counts.also_mentioned = also_total;
    truncated |= also_mentioned.iter().any(|a| a.files.len() < a.total);
    let mut blind_spots = Vec::new();
    if !used_names.is_empty() {
        let mut all: Vec<&String> = used_names.values().flatten().collect();
        all.sort_unstable();
        all.dedup();
        let q = |n: &str| if n.chars().all(|c| c.is_alphanumeric() || c == '_') { n.to_string() } else { format!("'{n}'") };
        let rg = match all.as_slice() {
            [one] => q(one),
            many => many.iter().map(|n| format!("-e {}", q(n))).collect::<Vec<_>>().join(" "),
        };
        blind_spots.push("methods matched by name; receivers untyped".to_string());
        blind_spots.push("JS/TS and send/try not checked".to_string());
        blind_spots.push(format!("also run: rg -w {rg}"));
    }

    further.files = further_listed
        .iter()
        .map(|&i| {
            let hit = &main.impact.hits[i];
            let class = ctx.class(hit.node);
            FurtherFile {
                file: graph.path(hit.node).to_string(),
                depth: hit.depth,
                via: graph.path(hit.via).to_string(),
                of: graph.path(main.root[i]).to_string(),
                kind: graph.edge(hit.edge).2.kind,
                confidence: main.minc[i],
                language: class.language,
                areas: policy.area_names(&class.areas),
                notable: notable_reasons(crosses(i, hit.node), policy.max_tier(&class.areas), class.global),
            }
        })
        .collect();
    truncated |= further.files.len() < if opts.all { counts.further } else { counts.further_notable };

    // What the changed files rely on (hop 1, forward).
    let mut relies: Vec<Dependency> = Vec::new();
    let mut seen_dep: HashSet<NodeId> = HashSet::new();
    let changed_nodes: HashSet<NodeId> = ctx.changed.iter().filter_map(|c| c.node).collect();
    for c in &ctx.changed {
        let Some(n) = c.node else { continue };
        // A method-scoped change relies on what its changed methods reference.
        let within = c.scopes.as_ref();
        for (to, e) in graph.dependencies(n) {
            let (_, _, d) = graph.edge(e);
            if d.confidence < eff.min_confidence || changed_nodes.contains(&to) {
                continue;
            }
            if let Some(names) = within
                && !graph.edge_scopes(e).0.is_some_and(|f| names.iter().any(|x| x == f))
            {
                continue;
            }
            if !seen_dep.insert(to) {
                continue;
            }
            relies.push(Dependency {
                file: graph.path(to).to_string(),
                of: c.file.path.clone(),
                line: (d.line != 0).then_some(d.line),
                kind: d.kind,
                detail: graph.edge_detail(e).map(str::to_string),
                confidence: d.confidence,
            });
        }
    }
    relies.sort_by(|a, b| (std::cmp::Reverse(a.confidence), &a.file).cmp(&(std::cmp::Reverse(b.confidence), &b.file)));
    counts.relies_on = relies.len();
    let relies_cap = opts.cap(budget::RELIES_ON);
    truncated |= relies.len() > relies_cap;
    relies.truncate(relies_cap);

    // The same query with every file wholly changed, so readers can see how
    // much the method-level analysis narrowed the result.
    counts.impacted_file_level = if eff.file_level {
        counts.impacted
    } else {
        let whole: Vec<(NodeId, Option<Vec<String>>)> = ctx.seeds.iter().map(|(n, _)| (*n, None)).collect();
        let imp = graph.impact_scoped(&whole, QueryOpts { file_level: true, ..eff });
        imp.impacted().filter(|h| !ctx.class(h.node).test).count()
    };

    let (removed_methods, removed_total) = crate::findings::removed_methods(&ctx, opts);
    counts.removed_methods = removed_total;
    truncated |= removed_methods.len() < removed_total || removed_methods.iter().any(|r| r.mentions.len() < r.mentions_total);
    let mut incomplete = crate::findings::incomplete(&ctx);
    if !opts.all {
        incomplete = group_capped(incomplete, |i| i.reason.clone(), |reason, n| Incomplete {
            reason,
            file: None,
            message: format!("+{n} more files like this (--all)"),
        });
    }

    // Areas reached: changed files first (depth 0), then impacted non-test files in BFS order.
    let full = opts.policy || opts.all;
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
            example: full.then(example),
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
    if !opts.all {
        warnings = group_capped(warnings, |w| w.code.clone(), |code, n| Warning::new(&code, format!("+{n} more files like this (--all)")));
    }

    let configured = !policy.uses_default_rules() || !policy.areas().is_empty();
    let show_policy = opts.policy || (configured && !reasons.is_empty());
    if !full {
        for r in &mut reasons {
            r.evidence.impacted.truncate(budget::EVIDENCE);
        }
    }
    let policy_result = show_policy.then(|| PolicyResult { risk, reasons, require, owners, areas: areas_reached });

    let mut report = Report {
        schema_version: SCHEMA_VERSION,
        summary: String::new(),
        counts,
        truncated,
        changed: changed_entries,
        removed_methods,
        dependents,
        inherit_only,
        also_mentioned,
        relies_on: relies,
        further,
        tests,
        incomplete,
        blind_spots,
        usually_changes_with: ctx
            .sig
            .filter(|_| ctx.status.cochange.is_ready())
            .map(|s| s.usually_changes_with.iter().take(crate::signals::USUALLY_CHANGES_WITH).cloned().collect())
            .unwrap_or_default(),
        signals: opts.signals.as_ref().map(|_| ctx.status.clone()),
        policy: policy_result,
        warnings,
        query: QueryInfo { max_depth: eff.max_depth, min_confidence: eff.min_confidence, file_level: eff.file_level, all: opts.all },
        timing: Timing { refresh_us: 0, query_us, evaluate_us: 0 },
        index: IndexInfo { generation: opts.generation, nodes: graph.node_count(), edges: graph.edge_count(), pending_changes: 0, stale: false },
    };
    report.summary = summary_line(&report);
    report.timing.evaluate_us = (started.elapsed().as_micros() as u64).saturating_sub(query_us);
    report
}

/// Keeps at most [`budget::PER_KIND`] entries per key (in order) and
/// replaces the rest of each key with one summary entry.
fn group_capped<T>(items: Vec<T>, key: impl Fn(&T) -> String, summary: impl Fn(String, usize) -> T) -> Vec<T> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut out = Vec::new();
    for it in items {
        let n = seen.entry(key(&it)).or_default();
        *n += 1;
        if *n <= budget::PER_KIND {
            out.push(it);
        }
    }
    for (k, n) in seen {
        if n > budget::PER_KIND {
            out.push(summary(k, n - budget::PER_KIND));
        }
    }
    out
}

/// `crosses_boundary`, `high_tier`, `global`: why a far file is worth a look.
fn notable_reasons(crosses: bool, tier: Option<Risk>, global: bool) -> Vec<String> {
    let mut v = Vec::new();
    if crosses {
        v.push("crosses_boundary".to_string());
    }
    if tier.is_some_and(|t| t >= Risk::High) {
        v.push("high_tier".to_string());
    }
    if global {
        v.push("global".to_string());
    }
    v
}

/// The report's one-line summary. Recomputed by the query side after it
/// adds `incomplete` entries.
pub fn summary_line(r: &Report) -> String {
    let c = &r.counts;
    let mut parts = vec![format!("{} changed file{}", c.changed, plural(c.changed))];
    if c.inherit_only > 0 {
        let callers = c.dependents - c.inherit_only;
        parts.push(format!("{callers} direct dependent{} (+{} only inherit/include)", plural(callers), c.inherit_only));
    } else {
        parts.push(format!("{} direct dependent{}", c.dependents, plural(c.dependents)));
    }
    if c.also_mentioned > 0 {
        parts.push(format!("{} more mention the name with no edge", c.also_mentioned));
    }
    if r.query.max_depth != 1 {
        let hops = if r.query.max_depth == 0 { "further hops".to_string() } else { format!("hops 2-{}", r.query.max_depth) };
        parts.push(format!("{} more at {hops}", c.further));
    }
    parts.push(format!("{} test{} reach{} it", c.tests, plural(c.tests), if c.tests == 1 { "es" } else { "" }));
    if c.removed_methods > 0 {
        parts.push(format!("{} removed method{} still mentioned", c.removed_methods, plural(c.removed_methods)));
    }
    if c.impacted_file_level != c.impacted && !r.query.file_level {
        parts.push(format!("method-level ({} if whole files changed)", c.impacted_file_level));
    }
    if let Some(p) = &r.policy {
        parts.push(format!("{} risk", risk_word(p.risk)));
    }
    let mut s = parts.join("; ");
    if !r.incomplete.is_empty() {
        let mut reasons: Vec<&str> = r.incomplete.iter().map(|i| i.reason.as_str()).collect();
        reasons.sort_unstable();
        reasons.dedup();
        s.push_str(&format!("; may be incomplete ({})", reasons.join(", ")));
    }
    s
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
    Hop { file: file.to_string(), kind: None, confidence: None, line: None, detail: None, from_scope: None, to_scope: None, inherit: false }
}

/// `(from_scope, to_scope, inherit)` of an edge as shown to readers: the
/// class-level marker `@body` is left out, and inheritance edges carry no scopes.
pub(crate) fn shown_scopes(graph: &Graph, edge: u32) -> (Option<String>, Option<String>, bool) {
    if graph.edge_inherits(edge) {
        return (None, None, true);
    }
    let (f, t) = graph.edge_scopes(edge);
    let keep = |s: Option<&str>| s.filter(|s| *s != ripple_core::model::SCOPE_BODY).map(str::to_string);
    (keep(f), keep(t), false)
}

/// The hop list of `imp.path(node)` (the file, then each file closer to the change).
pub fn path_hops(graph: &Graph, imp: &Impact, node: NodeId) -> Vec<Hop> {
    imp.path(node)
        .into_iter()
        .map(|(n, e)| {
            if e == u32::MAX {
                hop0(graph.path(n))
            } else {
                let (_, _, d) = graph.edge(e);
                let (from_scope, to_scope, inherit) = shown_scopes(graph, e);
                Hop {
                    file: graph.path(n).to_string(),
                    kind: Some(d.kind),
                    confidence: Some(d.confidence),
                    line: (d.line != 0).then_some(d.line),
                    detail: graph.edge_detail(e).map(str::to_string),
                    from_scope,
                    to_scope,
                    inherit,
                }
            }
        })
        .collect()
}

fn tier_word(r: Risk) -> &'static str {
    match r {
        Risk::Low => "low",
        Risk::Medium => "medium",
        Risk::High => "high",
        Risk::Critical => "critical",
    }
}

/// Which conditions of `w` must hold on one and the same changed file:
/// (has a per-file metric condition, `untested: true` joins, `changed_lines_over` joins).
/// The metric conditions are `churn_over`, `complexity_over`,
/// `max_method_complexity_over`, `coverage_under`,
/// `changed_lines_coverage_under`, `uncovered: true` and `hotspot: true`.
/// `untested: true` and `changed_lines_over` join them (and each other);
/// alone, `untested` is about any file and `changed_lines_over` about the
/// whole diff.
pub(crate) fn per_file_group(w: &ripple_core::config::When) -> (bool, bool, bool) {
    let metric = w.churn_over.is_some()
        || w.complexity_over.is_some()
        || w.max_method_complexity_over.is_some()
        || w.coverage_under.is_some()
        || w.changed_lines_coverage_under.is_some()
        || w.uncovered == Some(true)
        || w.hotspot == Some(true);
    let untested = w.untested == Some(true) && (metric || w.changed_lines_over.is_some());
    let lines = w.changed_lines_over.is_some() && (metric || w.untested == Some(true));
    (metric, untested, lines)
}

fn list(items: &[String], max: usize) -> String {
    let mut s: Vec<String> = items.iter().take(max).map(|i| format!("`{i}`")).collect();
    if items.len() > max {
        s.push(format!("and {} more", items.len() - max));
    }
    s.join(", ")
}

/// Complexity is ready when the index carries metrics; the rest comes from
/// the caller (absent = unavailable).
fn signal_status(graph: &Graph, sig: Option<&SignalInput>) -> crate::signals::Signals {
    let mut s = match sig {
        Some(s) => s.status.clone(),
        None => {
            let na = || SignalStatus::new(SignalState::Unavailable, Some("not provided by the caller".to_string()));
            crate::signals::Signals { complexity: SignalStatus::ready(), churn: na(), cochange: na(), coverage: na(), hotspot: na() }
        }
    };
    if graph.metrics_tables().is_empty() && s.complexity.is_ready() {
        s.complexity = SignalStatus::new(SignalState::Unavailable, Some("the index has no metrics".to_string()));
    }
    s
}

impl Ctx<'_> {
    /// Changed files the signal conditions look at.
    fn candidates(&self) -> impl Iterator<Item = &Changed> {
        self.changed.iter().filter(|c| !c.class.test && c.class.language.is_source() && c.file.status != FileStatus::Deleted)
    }

    fn file_signals(&self, path: &str) -> Option<&crate::signals::FileSignals> {
        self.sig.and_then(|s| s.files.get(path))
    }

    /// The signals `w` uses that are not ready.
    fn blocked_signals(&self, w: &ripple_core::config::When) -> Vec<(&'static str, SignalStatus)> {
        let st = &self.status;
        let mut out = Vec::new();
        let mut need = |used: bool, name: &'static str, s: &SignalStatus| {
            if used && !s.is_ready() && !out.iter().any(|(n, _)| *n == name) {
                out.push((name, s.clone()));
            }
        };
        need(w.churn_over.is_some(), "churn", &st.churn);
        need(w.complexity_over.is_some() || w.max_method_complexity_over.is_some(), "complexity", &st.complexity);
        need(w.coverage_under.is_some() || w.changed_lines_coverage_under.is_some() || w.uncovered.is_some(), "coverage", &st.coverage);
        need(w.hotspot.is_some(), "hotspot", &st.hotspot);
        need(w.missing_cochange.is_some(), "cochange", &st.cochange);
        out
    }

    /// `changed[].metrics`: source files (not tests) that still exist.
    fn metrics_of(&self, c: &Changed) -> Option<Metrics> {
        if c.class.test || !c.class.language.is_source() || c.file.status == FileStatus::Deleted {
            return None;
        }
        let mut m = Metrics::default();
        if self.status.complexity.is_ready()
            && let Some(fm) = c.node.and_then(|n| self.graph.metrics(n))
        {
            m.complexity = Some(fm.complexity);
            m.max_method = (fm.methods > 0).then_some(u32::from(fm.max_method));
            m.lines = Some(fm.lines);
        }
        if let Some(f) = self.file_signals(&c.file.path) {
            m.churn = f.churn.filter(|_| self.status.churn.is_ready());
            m.coverage = f.coverage.filter(|_| self.status.coverage.is_ready());
            // Only hotspots: a rank in the middle of the field is noise in
            // a report (`ripple file` shows it for any file).
            m.hotspot = f.hotspot.filter(|h| h.hotspot && self.status.hotspot.is_ready());
        }
        (!m.is_empty()).then_some(m)
    }

    /// The per-file conditions of a rule (see [`per_file_group`]), all on
    /// the same changed file, then the rule-wide signal conditions
    /// (`uncovered: false`, `hotspot: false`, `missing_cochange`).
    /// `untested`: the changed source files no test depends on, when the
    /// group uses `untested: true`. `Err(())` when one does not hold.
    fn eval_signals(&self, w: &ripple_core::config::When, untested: Option<&[String]>, parts: &mut Vec<String>, changed_ev: &mut BTreeSet<String>, ev: &mut Evidence) -> Result<(), ()> {
        let cands: Vec<&Changed> = self.candidates().collect();
        let (metric, with_untested, with_lines) = per_file_group(w);
        // One condition of the group: what it says, the evidence count it
        // sets, and per file the value (larger = worse; sorts and counts)
        // and the number shown, or `None` when it does not hold.
        type Test<'s> = Box<dyn Fn(&Changed) -> Option<(f64, String)> + 's>;
        let mut conds: Vec<(String, Option<&'static str>, Test<'_>)> = Vec::new();
        let fm = |c: &Changed| c.node.and_then(|n| self.graph.metrics(n));
        let cov = |c: &Changed| self.file_signals(&c.file.path).and_then(|f| f.coverage);
        if let Some(n) = w.churn_over {
            let t: Test = Box::new(move |c| self.file_signals(&c.file.path)?.churn.filter(|ch| ch.recent_commits > n).map(|ch| (f64::from(ch.recent_commits), format!("{} commits", ch.recent_commits))));
            conds.push((format!("changed in more than {n} commits in the last 90 days"), Some("recent_commits"), t));
        }
        if let Some(n) = w.complexity_over {
            let t: Test = Box::new(move |c| fm(c).filter(|m| m.complexity > n).map(|m| (f64::from(m.complexity), format!("complexity {}", m.complexity))));
            conds.push((format!("complexity above {n}"), Some("complexity"), t));
        }
        if let Some(n) = w.max_method_complexity_over {
            let t: Test = Box::new(move |c| {
                fm(c).filter(|m| m.methods > 0 && u32::from(m.max_method) > n).map(|m| (f64::from(m.max_method), format!("a method of complexity {}", m.max_method)))
            });
            conds.push((format!("a method more complex than {n}"), Some("max_method_complexity"), t));
        }
        if let Some(pct) = w.coverage_under {
            let t: Test = Box::new(move |c| {
                let cv = cov(c)?;
                let p = cv.percent?;
                (p < pct).then(|| (-p, format!("{}% of {} lines", crate::signals::fmt_pct(p), cv.coverable)))
            });
            conds.push((format!("line coverage under {}%", crate::signals::fmt_pct(pct)), None, t));
        }
        if let Some(pct) = w.changed_lines_coverage_under {
            let t: Test = Box::new(move |c| {
                let p = cov(c)?.changed_lines_percent?;
                (p < pct).then(|| (-p, format!("{}% of the changed lines", crate::signals::fmt_pct(p))))
            });
            conds.push((format!("changed lines covered under {}%", crate::signals::fmt_pct(pct)), None, t));
        }
        if w.uncovered == Some(true) {
            let inc = w.uncovered_includes_unknown;
            let t: Test = Box::new(move |c| match cov(c) {
                Some(cv) if cv.coverable > 0 && cv.covered == 0 => Some((f64::from(cv.coverable), format!("0 of {} lines covered", cv.coverable))),
                Some(_) => None,
                None if inc => Some((0.0, "in no coverage report".to_string())),
                None => None,
            });
            conds.push(("not covered by any test".into(), None, t));
        }
        if w.hotspot == Some(true) {
            let top = self.sig.map_or(0.0, |s| s.hotspot_percent);
            let t: Test = Box::new(move |c| {
                let h = self.file_signals(&c.file.path)?.hotspot.filter(|h| h.hotspot)?;
                Some((-f64::from(h.rank), format!("#{} of {}", h.rank, h.of)))
            });
            conds.push((format!("a hotspot (top {}% of `ripple rank`)", crate::signals::fmt_pct(top)), None, t));
        }
        if with_untested {
            let untested = untested.unwrap_or_default();
            let t: Test = Box::new(move |c| untested.contains(&c.file.path).then(|| (0.0, "no test depends on it".to_string())));
            conds.push(("no test among its dependents".into(), None, t));
        }
        if with_lines && let Some(n) = w.changed_lines_over {
            if !self.has_lines {
                return Err(());
            }
            let t: Test = Box::new(move |c| {
                let lines = c.file.added.unwrap_or(0) as usize + c.file.deleted.unwrap_or(0) as usize;
                (lines > n).then(|| (lines as f64, format!("{lines} lines changed")))
            });
            conds.push((format!("more than {n} changed lines"), Some("changed_lines"), t));
        }
        debug_assert!(conds.is_empty() || metric || with_untested);
        if !conds.is_empty() {
            let mut hits: Vec<(&Changed, Vec<(f64, String)>)> = cands
                .iter()
                .filter_map(|&c| conds.iter().map(|(_, _, t)| t(c)).collect::<Option<Vec<_>>>().map(|v| (c, v)))
                .collect();
            if hits.is_empty() {
                return Err(());
            }
            hits.sort_by(|a, b| b.1[0].0.total_cmp(&a.1[0].0).then_with(|| a.0.file.path.cmp(&b.0.file.path)));
            for (i, (_, key, _)) in conds.iter().enumerate() {
                if let Some(key) = key {
                    let max = hits.iter().map(|h| h.1[i].0).fold(0.0, f64::max);
                    ev.counts.insert((*key).into(), max as usize);
                }
            }
            let whats: Vec<&str> = conds.iter().map(|(w, _, _)| w.as_str()).collect();
            let what = match whats.split_last() {
                Some((last, [])) => (*last).to_string(),
                Some((last, rest)) => format!("{} and {last} in the same file", rest.join(", ")),
                None => unreachable!(),
            };
            let shown: Vec<String> = hits.iter().take(3).map(|(c, v)| format!("`{}` ({})", c.file.path, v.iter().map(|x| x.1.as_str()).collect::<Vec<_>>().join(", "))).collect();
            let more = if hits.len() > 3 { format!(" and {} more", hits.len() - 3) } else { String::new() };
            parts.push(format!("{what}: {}{more}", shown.join(", ")));
            changed_ev.extend(hits.iter().map(|(c, _)| c.file.path.clone()));
        }
        drop(conds);
        // Rule-wide: "no changed file is ...".
        if w.uncovered == Some(false) {
            let any = cands.iter().any(|c| match cov(c) {
                Some(cv) => cv.coverable > 0 && cv.covered == 0,
                None => w.uncovered_includes_unknown,
            });
            if any {
                return Err(());
            }
            parts.push("every changed source file has some line coverage".into());
        }
        if w.hotspot == Some(false) {
            if cands.iter().any(|c| self.file_signals(&c.file.path).and_then(|f| f.hotspot).is_some_and(|h| h.hotspot)) {
                return Err(());
            }
            parts.push("changes no hotspot".into());
        }
        if let Some(want) = w.missing_cochange {
            let min = w.cochange_min_confidence.unwrap_or(ripple_core::config::DEFAULT_COCHANGE_MIN_CONFIDENCE);
            let partners: Vec<&(crate::signals::CoChangeEntry, f32)> = self.sig.map(|s| s.partners.iter().filter(|(_, sc)| *sc >= min).collect()).unwrap_or_default();
            if partners.is_empty() == want {
                return Err(());
            }
            if want {
                let shown: Vec<String> = partners.iter().take(3).map(|(p, _)| format!("`{}` (with `{}` in {} of {} commits)", p.path, p.via, p.together, p.of)).collect();
                let more = if partners.len() > 3 { format!(" and {} more", partners.len() - 3) } else { String::new() };
                parts.push(format!("usually changes with files not in the diff ({}%+ of commits): {}{more}", crate::signals::fmt_pct(f64::from(min) * 100.0), shown.join(", ")));
                changed_ev.extend(partners.iter().map(|(p, _)| p.via.clone()));
                ev.counts.insert("missing_cochange".into(), partners.len());
            } else {
                parts.push("no usual co-change partner is missing from the diff".into());
            }
        }
        Ok(())
    }

    pub(crate) fn class(&self, node: NodeId) -> &FileClass {
        self.index.get(node).expect("policy index matches graph")
    }

    fn ensure_reach(&mut self, conf: Confidence, depth: u32) -> usize {
        if let Some(i) = self.reaches.iter().position(|r| r.conf == conf && covers(r.depth, depth)) {
            return i;
        }
        let impact = self.graph.impact_scoped(&self.seeds, QueryOpts { max_depth: depth, min_confidence: conf, file_level: self.eff.file_level });
        let mut root = Vec::with_capacity(impact.hits.len());
        let mut minc = Vec::with_capacity(impact.hits.len());
        // Follow the explanation path, not the `via` chain: with scoped seeds
        // the file a hit's `via` was first reached from need not be the seed
        // whose change actually reaches the hit.
        for h in &impact.hits {
            let path = impact.path(h.node);
            root.push(path.last().map_or(h.node, |&(n, _)| n));
            minc.push(path.iter().filter(|&&(_, e)| e != u32::MAX).map(|&(_, e)| self.graph.edge(e).2.confidence).min().unwrap_or(Confidence::Exact));
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
        let path = path_hops(self.graph, &reach.impact, node);
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
        let reach = &self.reaches[self.reach_idx(conf, self.eff.max_depth)];
        for c in &self.changed {
            if c.class.test || !c.class.language.is_source() || c.file.status == FileStatus::Deleted {
                continue;
            }
            let tested = c.node.is_some_and(|n| match &c.scopes {
                None => self.has_test_dependent(n, conf, self.eff.max_depth),
                // Nothing another file can depend on changed.
                Some(s) if s.is_empty() => true,
                Some(s) => self.has_scoped_test_dependent(reach, n, s, conf),
            });
            if !tested {
                out.push(c.file.path.clone());
            }
        }
        out
    }

    /// Does a test depend on one of `scopes` of `node`? The shared BFS answers
    /// "yes" cheaply when it traced a test back to `node`; otherwise (another
    /// changed file may have reached that test first) ask the graph directly.
    fn has_scoped_test_dependent(&self, reach: &Reach, node: NodeId, scopes: &[String], conf: Confidence) -> bool {
        let limit = self.eff.max_depth;
        let quick = reach.impact.hits.iter().enumerate().any(|(i, h)| reach.root[i] == node && within(h.depth, limit) && self.class(h.node).test);
        if quick {
            return true;
        }
        let imp = self.graph.impact_scoped(&[(node, Some(scopes.to_vec()))], QueryOpts { max_depth: limit, min_confidence: conf, file_level: false });
        imp.impacted().any(|h| self.class(h.node).test)
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
        let (_, group_untested, group_lines) = per_file_group(w);
        if let Some(n) = w.changed_lines_over.filter(|_| !group_lines) {
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
        let mut grouped_untested = None;
        if group_untested {
            grouped_untested = Some(untested_cache.entry(conf).or_insert_with(|| self.untested(conf)).clone());
        } else if let Some(want) = w.untested {
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
        if self.eval_signals(w, grouped_untested.as_deref(), &mut parts, &mut changed_ev, &mut ev).is_err() {
            return None;
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
