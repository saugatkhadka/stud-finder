use super::*;
use crate::changed::{ChangeKind, DefLine, Hunk, WholeFileReason, classify_change};
use ripple_core::model::ScopeRange;
use crate::lang::Language;
use crate::load::parse_config;
use ripple_core::{Edge, EdgeKind::*};

const AREAS: &str = "
areas:
  billing:
    paths: [app/models/invoice.rb, app/services/**, app/javascript/billing/**]
    tier: critical
    owners: ['@payments']
  auth:
    paths: [app/models/user.rb, app/models/session.rb]
    tier: critical
    owners: ['@identity']
  frontend:
    paths: [app/javascript/**]
    tier: medium
    owners: ['@web']
  tooling:
    paths: [lib/**]
    tier: high
";

pub(crate) fn fixture_graph() -> Graph {
    use Confidence::*;
    let edges = vec![
        Edge::new("app/services/charge.rb", "app/models/invoice.rb", RubyConstant, Exact).at(4).detail("Invoice"),
        Edge::new("app/controllers/invoices_controller.rb", "app/services/charge.rb", RailsInferred, Inferred).at(9).detail("Charge"),
        Edge::new("app/javascript/billing/api.ts", "app/controllers/invoices_controller.rb", ApiCall, Heuristic).at(12).detail("POST /invoices"),
        Edge::new("app/javascript/pages/checkout.tsx", "app/javascript/billing/api.ts", JsImport, Exact).at(1).detail("../billing/api"),
        Edge::new("spec/models/invoice_spec.rb", "app/models/invoice.rb", RubyConstant, Exact).at(3).detail("Invoice"),
        Edge::new("app/models/session.rb", "app/models/user.rb", RubyConstant, Exact).at(2).detail("User"),
        Edge::new("app/models/session.rb", "lib/token.rb", RubyConstant, Exact).at(5).detail("Token"),
    ];
    Graph::build(&edges, ["lib/util.rb", "config/initializers/stripe.rb", "README.md"])
}

fn policy(rules: &str) -> Policy {
    let mut config = parse_config(&format!("{AREAS}\n{rules}"), "test").unwrap();
    // The fixture chain is 4 hops long; these tests exercise rule semantics,
    // not the default depth cap, so walk the whole graph unless a test sets one.
    if !rules.contains("max_depth") {
        config.query.max_depth = 0;
    }
    Policy::compile(config).unwrap()
}

/// Full reports: the policy section always present, no list budget.
fn full() -> EvalOptions {
    EvalOptions { policy: true, all: true, ..Default::default() }
}

fn run(p: &Policy, changed: &[ChangedFile]) -> Report {
    let g = fixture_graph();
    evaluate_with(p, &g, &PolicyIndex::build(p, &g), changed, &full())
}

fn pol(r: &Report) -> &PolicyResult {
    r.policy.as_ref().expect("policy section")
}

fn m(path: &str) -> ChangedFile {
    ChangedFile::modified(path)
}

/// Does a single rule with this `when` (YAML flow mapping) fire for `changed`?
fn fires(when: &str, changed: &[ChangedFile]) -> bool {
    fires_with("", when, changed)
}

fn fires_with(extra: &str, when: &str, changed: &[ChangedFile]) -> bool {
    let p = policy(&format!("{extra}\nrules:\n  - id: r\n    risk: high\n    when: {when}\n"));
    let r = run(&p, changed);
    assert!(pol(&r).reasons.len() <= 1);
    !pol(&r).reasons.is_empty()
}

#[test]
fn touches_area_and_glob() {
    assert!(fires("{touches: [billing]}", &[m("app/models/invoice.rb")]));
    assert!(!fires("{touches: [billing]}", &[m("lib/util.rb")]));
    assert!(fires("{touches: ['app/models/**']}", &[m("app/models/user.rb")]));
    assert!(!fires("{touches: ['app/models/**']}", &[m("lib/util.rb")]));
    // Moving a file out of an area touches that area.
    assert!(fires("{touches: [billing]}", &[ChangedFile::modified("app/models/bill.rb").renamed_from("app/models/invoice.rb")]));
    // Files not in the graph still match.
    assert!(fires("{touches: [billing]}", &[ChangedFile::new("app/services/new_thing.rb", FileStatus::Added)]));
}

#[test]
fn touches_tier() {
    assert!(fires("{touches_tier: high}", &[m("app/models/invoice.rb")]));
    assert!(fires("{touches_tier: high}", &[m("lib/util.rb")]));
    assert!(!fires("{touches_tier: critical}", &[m("lib/util.rb")]));
    assert!(!fires("{touches_tier: low}", &[m("README.md")]));
}

#[test]
fn reaches_and_within() {
    // invoice -> charge (1) -> controller (2) -> api.ts (3) -> checkout.tsx (4)
    let inv = [m("app/models/invoice.rb")];
    assert!(fires("{reaches: [frontend]}", &inv));
    assert!(!fires("{reaches: [frontend], within: 2}", &inv));
    assert!(fires("{reaches: [frontend], within: 3}", &inv));
    assert!(fires("{reaches: ['app/controllers/**'], within: 2}", &inv));
    assert!(!fires("{reaches: [frontend]}", &[m("lib/util.rb")]));
}

#[test]
fn reaches_tier_excludes_changed_files_themselves() {
    assert!(fires("{reaches_tier: critical}", &[m("lib/token.rb")]));
    assert!(fires("{reaches_tier: critical, within: 1}", &[m("app/models/user.rb")]));
    // session.rb is critical but nothing depends on it.
    assert!(!fires("{reaches_tier: critical}", &[m("app/models/session.rb")]));
    assert!(!fires("{reaches_tier: critical}", &[m("lib/util.rb")]));
    // reaches and reaches_tier are OR-ed.
    assert!(fires("{reaches: ['nothing/**'], reaches_tier: critical}", &[m("lib/token.rb")]));
}

#[test]
fn impacted_files_over_excludes_tests() {
    // 4 non-test dependents of invoice; the spec does not count.
    let inv = [m("app/models/invoice.rb")];
    assert!(fires("{impacted_files_over: 3}", &inv));
    assert!(!fires("{impacted_files_over: 4}", &inv));
}

#[test]
fn changed_files_over() {
    assert!(fires("{changed_files_over: 1}", &[m("a.rb"), m("b.rb")]));
    assert!(!fires("{changed_files_over: 2}", &[m("a.rb"), m("b.rb")]));
    // Ignored files do not count.
    assert!(!fires("{changed_files_over: 1}", &[m("a.rb"), m("node_modules/x.js")]));
}

#[test]
fn changed_lines_over_needs_a_diff() {
    let c = [m("a.rb").lines(10, 5), m("b.rb")];
    assert!(fires("{changed_lines_over: 10}", &c));
    assert!(!fires("{changed_lines_over: 15}", &c));
    let p = policy("rules:\n  - {id: r, risk: high, when: {changed_lines_over: 0}}\n");
    let r = run(&p, &[m("a.rb")]);
    assert!(pol(&r).reasons.is_empty());
    assert!(r.warnings.iter().any(|w| w.code == "no_line_counts"));
}

#[test]
fn crosses_boundary_both_polarities() {
    assert!(fires("{crosses_boundary: true}", &[m("app/models/invoice.rb")]));
    assert!(!fires("{crosses_boundary: true}", &[m("lib/token.rb")]));
    assert!(fires("{crosses_boundary: false}", &[m("lib/token.rb")]));
    assert!(!fires("{crosses_boundary: false}", &[m("app/models/invoice.rb")]));
    // JS -> JS is not a crossing.
    assert!(!fires("{crosses_boundary: true}", &[m("app/javascript/billing/api.ts")]));
}

#[test]
fn touches_global_both_polarities() {
    assert!(fires("{touches_global: true}", &[m("config/initializers/stripe.rb")]));
    assert!(!fires("{touches_global: true}", &[m("lib/util.rb")]));
    assert!(fires("{touches_global: false}", &[m("lib/util.rb")]));
    assert!(!fires("{touches_global: false}", &[m("config/initializers/stripe.rb")]));
}

#[test]
fn untested_both_polarities() {
    // invoice has a spec depending on it; charge.rb has only non-test dependents.
    assert!(!fires("{untested: true}", &[m("app/models/invoice.rb")]));
    assert!(fires("{untested: true}", &[m("app/services/charge.rb")]));
    assert!(fires("{untested: false}", &[m("app/models/invoice.rb")]));
    assert!(!fires("{untested: false}", &[m("app/models/invoice.rb"), m("app/services/charge.rb")]));
    // New source files have no dependents at all; non-source files and tests never count.
    assert!(fires("{untested: true}", &[ChangedFile::new("app/models/brand_new.rb", FileStatus::Added)]));
    assert!(!fires("{untested: true}", &[m("README.md"), m("spec/models/invoice_spec.rb")]));
    // Transitive: token.rb <- session.rb, no test anywhere.
    assert!(fires("{untested: true}", &[m("lib/token.rb")]));
}

#[test]
fn min_confidence_per_rule() {
    let inv = [m("app/models/invoice.rb")];
    // charge -> controller is inferred, api.ts -> controller is heuristic.
    assert!(!fires("{reaches: [frontend], min_confidence: exact}", &inv));
    assert!(!fires("{reaches: [frontend], min_confidence: inferred}", &inv));
    assert!(fires("{reaches: [frontend], min_confidence: heuristic}", &inv));
    assert!(fires("{reaches: ['app/controllers/**'], min_confidence: inferred}", &inv));
    assert!(!fires("{impacted_files_over: 1, min_confidence: exact}", &inv));
    assert!(!fires("{crosses_boundary: true, min_confidence: inferred}", &inv));
    // A rule may look at weaker edges than the query does.
    let strict = "query: {min_confidence: inferred}";
    assert!(!fires_with(strict, "{reaches: [frontend]}", &inv));
    assert!(fires_with(strict, "{reaches: [frontend], min_confidence: heuristic}", &inv));
}

#[test]
fn within_may_exceed_query_depth() {
    let inv = [m("app/models/invoice.rb")];
    let shallow = "query: {max_depth: 1}";
    assert!(!fires_with(shallow, "{reaches: [frontend]}", &inv));
    assert!(fires_with(shallow, "{reaches: [frontend], within: 3}", &inv));
    let p = policy(&format!("{shallow}\nrules:\n  - {{id: r, risk: high, when: {{reaches: [frontend], within: 3}}}}\n"));
    let r = run(&p, &inv);
    assert_eq!(r.counts.impacted, 1, "report itself still honours max_depth");
}

#[test]
fn conditions_and_together_and_empty_when_always_fires() {
    let inv = [m("app/models/invoice.rb")];
    assert!(fires("{touches: [billing], crosses_boundary: true}", &inv));
    assert!(!fires("{touches: [billing], touches_global: true}", &inv));
    assert!(fires("{}", &[m("lib/util.rb")]));
}

#[test]
fn risk_is_max_and_require_is_union() {
    let p = policy(
        "rules:\n  - {id: a, risk: medium, when: {touches: [billing]}, require: [x, y]}\n  - {id: b, risk: critical, when: {crosses_boundary: true}, require: [y, z]}\n  - {id: c, risk: high, when: {touches_global: true}, require: [w]}\n",
    );
    let r = run(&p, &[m("app/models/invoice.rb")]);
    assert_eq!(pol(&r).risk, Risk::Critical);
    assert_eq!(pol(&r).require, ["x", "y", "z"]);
    assert_eq!(pol(&r).reasons.iter().map(|x| x.rule.as_str()).collect::<Vec<_>>(), ["b", "a"]);
    let r = run(&p, &[m("README.md")]);
    assert_eq!(pol(&r).risk, Risk::Low);
    assert!(pol(&r).reasons.is_empty() && pol(&r).require.is_empty());
}

#[test]
fn default_rules() {
    let p = policy("");
    let ids = |r: &Report| pol(&r).reasons.iter().map(|x| x.rule.clone()).collect::<Vec<_>>();

    let r = run(&p, &[m("app/models/invoice.rb")]);
    assert_eq!(pol(&r).risk, Risk::Critical);
    assert!(pol(&r).require.contains(&"human_review".to_string()));
    assert!(ids(&r).contains(&"default.touches_critical".to_string()));
    assert!(ids(&r).contains(&"default.crosses_boundary".to_string()));
    assert!(pol(&r).reasons.iter().all(|x| x.builtin));

    let r = run(&p, &[m("lib/util.rb")]);
    assert_eq!((pol(&r).risk, ids(&r)), (Risk::High, vec!["default.touches_high".to_string()]));
    assert_eq!(pol(&r).require, ["human_review"]);

    let r = run(&p, &[m("config/initializers/stripe.rb")]);
    assert_eq!((pol(&r).risk, ids(&r)), (Risk::Low, vec!["default.touches_global".to_string()]));
    assert_eq!(pol(&r).require, ["run_full_suite"]);

    // lib/token.rb is tier high; session.rb (critical) depends on it directly.
    let r = run(&p, &[m("lib/token.rb")]);
    assert!(ids(&r).contains(&"default.reaches_critical_direct".to_string()));
    assert!(!ids(&r).contains(&"default.reaches_critical".to_string()), "lower rung of the same ladder is dropped");

    let r = run(&p, &[m("README.md")]);
    assert_eq!(pol(&r).risk, Risk::Low);
    assert!(pol(&r).reasons.is_empty());
}

#[test]
fn default_rules_reach_any_depth_and_blast_radius() {
    use Confidence::*;
    // hub <- 60 files; deep: x -> y -> crit (critical, 2 hops)
    let mut edges: Vec<Edge> = (0..60).map(|i| Edge::new(format!("app/a{i}.rb"), "app/hub.rb", RubyConstant, Exact)).collect();
    edges.push(Edge::new("app/y.rb", "app/x.rb", RubyConstant, Exact));
    edges.push(Edge::new("app/crit.rb", "app/y.rb", RubyConstant, Exact));
    let g = Graph::build(&edges, []);
    let p = Policy::compile(parse_config("areas: {c: {paths: [app/crit.rb], tier: critical}}", "t").unwrap()).unwrap();
    let r = evaluate(&p, &g, &[m("app/hub.rb")]);
    assert!(r.policy.is_none(), "60 impacted files is ordinary: no rule fires");
    let r = evaluate(&p, &g, &[m("app/x.rb")]);
    assert_eq!(pol(&r).risk, Risk::Low);
    assert_eq!(pol(&r).reasons.iter().map(|x| x.rule.as_str()).collect::<Vec<_>>(), ["default.reaches_critical"]);

    let edges: Vec<Edge> = (0..201).map(|i| Edge::new(format!("app/a{i}.rb"), "app/hub.rb", RubyConstant, Exact)).collect();
    let g = Graph::build(&edges, []);
    let r = evaluate(&p, &g, &[m("app/hub.rb")]);
    assert_eq!(pol(&r).risk, Risk::Medium);
    assert_eq!(pol(&r).reasons[0].rule, "default.impacted_over_200");

    let edges: Vec<Edge> = (0..1501).map(|i| Edge::new(format!("app/a{i}.rb"), "app/hub.rb", RubyConstant, Exact)).collect();
    let g = Graph::build(&edges, []);
    let r = evaluate(&p, &g, &[m("app/hub.rb")]);
    assert_eq!(pol(&r).risk, Risk::High);
    assert_eq!(pol(&r).reasons.iter().map(|x| x.rule.as_str()).collect::<Vec<_>>(), ["default.impacted_over_1500"], "lower rung dropped");
    assert_eq!(pol(&r).require, ["run_full_suite"]);
    assert!(r.warnings.iter().all(|w| w.code != "rule_signal_not_ready"), "built-in metric rules skip quietly");
}

#[test]
fn report_contents() {
    let p = policy("");
    let r = run(&p, &[m("app/models/invoice.rb").lines(3, 1), m("config/initializers/stripe.rb"), ChangedFile::new("app/gone.rb", FileStatus::Deleted)]);
    assert_eq!(r.schema_version, SCHEMA_VERSION);
    let inv = r.changed.iter().find(|c| c.path == "app/models/invoice.rb").unwrap();
    assert_eq!((inv.dependents, inv.in_graph, inv.language, inv.areas.clone()), (2, true, Language::Ruby, vec!["billing".to_string()]));
    assert_eq!((inv.added, inv.deleted), (Some(3), Some(1)));
    let gone = r.changed.iter().find(|c| c.path == "app/gone.rb").unwrap();
    assert!(!gone.in_graph);
    assert!(r.warnings.iter().any(|w| w.code == "deleted_file_not_indexed" && w.file.as_deref() == Some("app/gone.rb")));
    let global = r.warnings.iter().find(|w| w.code == "global_file_changed").unwrap();
    assert!(global.message.starts_with("global: static impact is not reliable for this file"));
    assert!(r.changed.iter().any(|c| c.path == "config/initializers/stripe.rb" && c.is_global));

    assert_eq!(r.counts.impacted, 4);
    assert_eq!((r.counts.dependents, r.counts.further, r.counts.tests, r.counts.tests_direct), (1, 3, 1, 1));
    assert_eq!(r.tests.iter().map(|t| t.file.as_str()).collect::<Vec<_>>(), ["spec/models/invoice_spec.rb"]);
    assert_eq!(r.tests[0].line, Some(3));
    let charge = &r.dependents[0];
    assert_eq!((charge.file.as_str(), charge.line, charge.kind, charge.detail.as_deref()), ("app/services/charge.rb", Some(4), RubyConstant, Some("Invoice")));
    assert_eq!((charge.of.as_str(), charge.confidence, charge.areas.clone()), ("app/models/invoice.rb", Confidence::Exact, vec!["billing".to_string()]));
    assert_eq!(r.further.by_depth, BTreeMap::from([(2, 1), (3, 1), (4, 1)]));
    assert_eq!(r.further.by_language, BTreeMap::from([(Language::Ruby, 1), (Language::Js, 2)]));
    assert_eq!(r.further.by_area, BTreeMap::from([("billing".to_string(), 1), ("frontend".to_string(), 2)]));
    let checkout = r.further.files.iter().find(|f| f.file == "app/javascript/pages/checkout.tsx").unwrap();
    assert_eq!(checkout.depth, 4);
    assert_eq!(checkout.via, "app/javascript/billing/api.ts");
    assert_eq!(checkout.of, "app/models/invoice.rb");
    assert_eq!((checkout.kind, checkout.confidence), (JsImport, Confidence::Heuristic), "weakest link on the path");
    assert_eq!(checkout.areas, ["frontend"]);
    assert_eq!(checkout.notable, ["crosses_boundary"]);
    let api = r.further.files.iter().find(|f| f.file == "app/javascript/billing/api.ts").unwrap();
    assert_eq!(api.notable, ["crosses_boundary", "high_tier"]);
    assert_eq!(r.counts.further_notable, 2);
    // Whole-file changes and why.
    assert_eq!(inv.change.kind, ChangeKind::WholeFile);
    assert_eq!(inv.change.whole_file, Some(WholeFileReason::NoDiff));
    assert_eq!(gone.change.whole_file, Some(WholeFileReason::Deleted));
    // Never silent about what it cannot see.
    let inc: Vec<(&str, Option<&str>)> = r.incomplete.iter().map(|i| (i.reason.as_str(), i.file.as_deref())).collect();
    assert_eq!(inc, [("not_in_graph", Some("app/gone.rb")), ("global_file", Some("config/initializers/stripe.rb"))]);
    assert!(r.summary.ends_with("may be incomplete (global_file, not_in_graph)"), "{}", r.summary);

    // Explanation path in the crosses_boundary evidence.
    let cross = pol(&r).reasons.iter().find(|x| x.rule == "default.crosses_boundary").unwrap();
    assert_eq!(cross.evidence.changed, ["app/models/invoice.rb"]);
    let api = &cross.evidence.impacted[0];
    assert_eq!(api.file, "app/javascript/billing/api.ts");
    let files: Vec<&str> = api.path.iter().map(|h| h.file.as_str()).collect();
    assert_eq!(files, ["app/javascript/billing/api.ts", "app/controllers/invoices_controller.rb", "app/services/charge.rb", "app/models/invoice.rb"]);
    assert_eq!(api.path[0].kind, Some(ApiCall));
    assert_eq!(api.path[0].line, Some(12));
    assert_eq!(api.path[0].detail.as_deref(), Some("POST /invoices"));
    assert_eq!(api.path[3].kind, None);

    let names: Vec<(&str, bool, u32)> = pol(&r).areas.iter().map(|a| (a.area.as_str(), a.touched, a.min_depth)).collect();
    assert_eq!(names, [("billing", true, 0), ("frontend", false, 3)]);
    let billing = &pol(&r).areas[0];
    assert_eq!(billing.files, 3, "invoice + charge + api.ts");
    assert_eq!(pol(&r).owners, ["@payments", "@web"]);
}

#[test]
fn limit_truncates_lists_not_counts() {
    let p = policy("");
    let opts = EvalOptions { limit: Some(1), ..Default::default() };
    let g = fixture_graph();
    let r = evaluate_with(&p, &g, &PolicyIndex::build(&p, &g), &[m("app/models/invoice.rb")], &opts);
    assert_eq!(r.further.files.len(), 1);
    assert_eq!((r.counts.impacted, r.counts.further_notable), (4, 2));
    assert!(r.truncated);
}

/// hub.rb with `n` direct dependents (every 10th in a critical area), each
/// with one dependent of its own, and `n` tests.
fn hub_graph(n: usize) -> (Graph, Policy) {
    use Confidence::*;
    let mut edges = Vec::new();
    for i in 0..n {
        edges.push(Edge::new(format!("app/a{i:03}.rb"), "app/hub.rb", RubyConstant, Exact).at(i as u32 + 1).detail("Hub"));
        edges.push(Edge::new(format!("app/b{i:03}.rb"), format!("app/a{i:03}.rb"), RubyConstant, Exact).at(2).detail("A"));
        edges.push(Edge::new(format!("spec/a{i:03}_spec.rb"), "app/hub.rb", RubyConstant, Exact).at(1).detail("Hub"));
    }
    let g = Graph::build(&edges, []);
    let p = Policy::compile(parse_config("areas: {pay: {paths: ['app/a*0.rb', 'app/b*0.rb'], tier: critical}}", "t").unwrap()).unwrap();
    (g, p)
}

#[test]
fn budgets_cut_lists_and_say_so() {
    let (g, p) = hub_graph(60);
    let idx = PolicyIndex::build(&p, &g);
    let r = evaluate_with(&p, &g, &idx, &[m("app/hub.rb")], &EvalOptions::default());
    assert_eq!((r.counts.dependents, r.counts.further, r.counts.tests, r.counts.tests_direct), (60, 60, 60, 60));
    assert_eq!(r.dependents.len(), budget::DEPENDENTS);
    assert_eq!(r.tests.len(), budget::TESTS);
    assert_eq!(r.further.files.len(), 6, "only notable (high-tier) further files");
    assert_eq!(r.counts.further_notable, 6);
    assert!(r.further.files.iter().all(|f| f.notable == ["high_tier"]));
    assert!(r.truncated);
    // High-tier dependents first, then by path.
    let first: Vec<&str> = r.dependents.iter().take(7).map(|d| d.file.as_str()).collect();
    assert_eq!(first, ["app/a000.rb", "app/a010.rb", "app/a020.rb", "app/a030.rb", "app/a040.rb", "app/a050.rb", "app/a001.rb"]);
    let t = crate::render::text::render(&r, usize::MAX, false);
    assert!(t.contains("dependents 30/60:\n"), "{t}");
    assert!(t.contains("  +30 more (--all)\n"), "{t}");
    assert!(t.contains("tests (static reach) 60: 60 direct\n"), "{t}");
    assert!(t.contains("further 60 files  hops 2:60  ruby:60  areas pay:6  notable 6\n"), "{t}");
    assert!(t.lines().count() < 60, "{t}");

    let all = evaluate_with(&p, &g, &idx, &[m("app/hub.rb")], &EvalOptions { all: true, ..Default::default() });
    assert_eq!((all.dependents.len(), all.tests.len(), all.further.files.len()), (60, 60, 60));
    assert!(!all.truncated);
    assert!(!crate::render::text::render(&all, usize::MAX, false).contains("more (--all)"));
    // Counts never depend on the budget.
    assert_eq!(all.counts, r.counts);
}

#[test]
fn deterministic_regardless_of_input_order() {
    let p = policy("");
    let a = [m("app/models/invoice.rb"), m("lib/token.rb"), m("config/initializers/stripe.rb"), m("app/models/invoice.rb")];
    let mut b = a.to_vec();
    b.reverse();
    let norm = |mut r: Report| {
        r.timing = Timing::default();
        serde_json::to_string(&r).unwrap()
    };
    let x = norm(run(&p, &a));
    for _ in 0..5 {
        assert_eq!(x, norm(run(&p, &b)));
    }
    let back: Report = serde_json::from_str(&x).unwrap();
    assert_eq!(norm(back), x, "report round-trips through JSON");
}

// ---- renderers -------------------------------------------------------------

fn sample() -> Report {
    let p = policy("");
    let mut r = run(&p, &[m("app/models/invoice.rb").lines(3, 1), m("config/initializers/stripe.rb")]);
    r.changed[1].first_line = Some(7);
    r.timing = Timing::default();
    r
}

#[test]
fn markdown_snapshot() {
    let md = crate::render::markdown::render(&sample(), 2);
    let expect_lines = [
        "<!-- ripple-impact-report -->",
        "## Change impact: CRITICAL risk",
        "- **CRITICAL** `default.touches_critical`: Changes `app/models/invoice.rb` in billing (critical) — _Changes code in a critical area_",
        "- [ ] `human_review` (from `default.touches_critical`)",
        "- [ ] `run_full_suite` (from `default.touches_global`)",
        "Suggested reviewers (owners of the areas involved): @payments, @web",
        "| billing | critical | changed directly | 3 | @payments | `app/models/invoice.rb` |",
        "| `app/models/invoice.rb` | modified | +3 −1 | whole file (no diff) | ruby | billing | 2 |",
        "| `config/initializers/stripe.rb` (**global**) | modified |  | whole file (no diff) | ruby |  | 0 |",
        "### Direct dependents (1)",
        "- `app/services/charge.rb:4` [billing] → `app/models/invoice.rb` (ruby_constant, `Invoice`)",
        "### Further away (3)",
        "### The impact may be incomplete",
        "- `config/initializers/stripe.rb` `global_file`: matches `global_files`: the whole app may depend on it in ways no static graph sees",
        "spec/models/invoice_spec.rb",
    ];
    for l in expect_lines {
        assert!(md.lines().any(|x| x == l), "missing line:\n{l}\n--- in ---\n{md}");
    }
    assert!(md.contains("  - e.g. `app/javascript/billing/api.ts` → (api_call `POST /invoices` line 12 heuristic) `app/controllers/invoices_controller.rb`"), "{md}");
    assert!(!md.contains("checkout.tsx"), "capped at max_listed");
}

#[test]
fn text_snapshot_plain_and_colour() {
    let r = sample();
    let t = crate::render::text::render(&r, 10, false);
    assert!(!t.contains('\x1b'));
    let lines: Vec<&str> = t.lines().collect();
    assert_eq!(lines[0], "2 changed files; 1 direct dependent; 3 more at further hops; 1 test reaches it; CRITICAL risk; may be incomplete (global_file)");
    assert_eq!(lines[1], "INCOMPLETE config/initializers/stripe.rb: global_file: matches `global_files`: the whole app may depend on it in ways no static graph sees");
    assert!(lines[2].starts_with("changed "), "risk comes after the correctness signals: {t}");
    let risk = t.lines().find(|l| l.starts_with("risk ")).unwrap();
    assert!(risk.starts_with("risk CRITICAL  rules touches_critical") && risk.ends_with("require human_review, run_full_suite  owners @payments @web"), "{t}");
    assert!(t.find("
risk ").unwrap() > t.find("
tests ").unwrap(), "{t}");
    assert!(!t.contains("blind spots"), "whole-file changes have no name-based blind spots");
    assert!(t.contains("\nchanged M app/models/invoice.rb  ruby  +3 -1  whole file (no diff)  2 file dependents  [billing]\n"), "{t}");
    assert!(t.contains("\ndependents 1/1:\n  app/services/charge.rb:4  ruby_constant Invoice  -> app/models/invoice.rb  [billing]\n"), "{t}");
    assert!(t.contains("\nfurther 3 files  hops 2:1 3:1 4:1  ruby:1 js:2  areas billing:1 frontend:2  notable 2\n"), "{t}");
    assert!(t.contains("  app/javascript/billing/api.ts  hop 3 via app/controllers/invoices_controller.rb  heuristic  crosses_boundary,high_tier  [billing,frontend]\n"), "{t}");
    assert!(t.contains("\ntests (static reach) 1: 1 direct\n  spec/models/invoice_spec.rb:3\n"), "{t}");
    assert!(t.contains("warning: config/initializers/stripe.rb: global: static impact"), "{t}");
    assert!(!t.contains("relies on"), "invoice.rb relies on nothing");
    let c = crate::render::text::render(&r, 10, true);
    assert!(c.contains("\x1b[1;37;41mCRITICAL\x1b[0m"));
}

#[test]
fn github_snapshot() {
    let r = sample();
    let cmds = crate::render::github::workflow_commands(&r);
    let lines: Vec<&str> = cmds.lines().collect();
    assert!(lines.iter().any(|l| l.starts_with("::warning file=app/models/invoice.rb,line=1,title=ripple%3A critical risk (default.touches_critical)::Changes `app/models/invoice.rb` in billing (critical) (also: ")), "{cmds}");
    assert_eq!(lines.iter().filter(|l| l.starts_with("::warning file=app/models/invoice.rb")).count(), 1, "one annotation per changed file");
    assert!(lines.iter().any(|l| l.starts_with("::warning file=config/initializers/stripe.rb,line=7,title=ripple%3A low risk (default.touches_global)::")), "{cmds}");
    assert!(lines.iter().any(|l| l.starts_with("::notice file=config/initializers/stripe.rb,title=ripple: global file::global:")), "{cmds}");
    assert_eq!(*lines.last().unwrap(), format!("::notice title=ripple::{}", r.summary));
    assert_eq!(
        crate::render::github::outputs(&r),
        "risk=critical\nrequire=human_review,run_full_suite\nimpacted=4\nimpacted_file_level=4\ntests=1\nchanged=2\n"
    );
    let dir = std::env::temp_dir().join(format!("ripple-gh-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (sum, out) = (dir.join("summary.md"), dir.join("output"));
    let mut stdout = Vec::new();
    crate::render::github::emit(&r, 10, &mut stdout, Some(&sum), Some(&out)).unwrap();
    crate::render::github::emit(&r, 10, &mut stdout, Some(&sum), Some(&out)).unwrap();
    assert_eq!(std::fs::read_to_string(&out).unwrap().matches("risk=critical").count(), 2, "appends");
    assert!(std::fs::read_to_string(&sum).unwrap().contains("## Change impact: CRITICAL risk"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn json_is_complete_and_matches_the_schema() {
    let j: serde_json::Value = serde_json::from_str(&crate::render::json::render(&sample(), false)).unwrap();
    for key in ["schema_version", "summary", "counts", "truncated", "changed", "removed_methods", "dependents", "relies_on", "further", "tests", "incomplete", "policy", "warnings", "query", "timing", "index"] {
        assert!(j.get(key).is_some(), "missing {key}");
    }
    assert!(j.get("usually_changes_with").is_none(), "reserved, absent by default");
    assert_eq!(j["policy"]["risk"], "critical");
    assert_eq!(j["further"]["by_depth"]["2"], 1);
    assert_eq!(j["changed"][0]["change"]["kind"], "whole_file");
    assert_eq!(crate::schema::validate_report(&j), Vec::<String>::new());
    // Every report shape the fixtures produce validates.
    let (g, p) = hub_graph(40);
    for opts in [EvalOptions::default(), full(), EvalOptions { file_level: true, ..Default::default() }] {
        let r = evaluate_with(&p, &g, &PolicyIndex::build(&p, &g), &[m("app/hub.rb"), m("app/a001.rb").in_scopes(["x"])], &opts);
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(crate::schema::validate_report(&v), Vec::<String>::new());
    }
    for r in [scoped_sample(), removed_sample()] {
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(crate::schema::validate_report(&v), Vec::<String>::new());
    }
    // And the validator does reject a wrong shape.
    let mut bad = j.clone();
    bad["counts"]["dependents"] = serde_json::json!("1");
    bad["extra"] = serde_json::json!(1);
    assert_eq!(crate::schema::validate_report(&bad).len(), 2);
}

// ---- method-level seeds ----------------------------------------------------

/// purchase.rb: `refund!` and `charge!`. refund_service#run calls refund!;
/// session.rb (auth, critical) calls charge!; the frontend page calls the
/// purchases controller's #show, which calls charge!; a spec covers charge!;
/// sub.rb inherits from Purchase and sub_user.rb calls sub's refund!.
fn method_graph() -> Graph {
    use Confidence::*;
    use ripple_core::model::SCOPE_BODY;
    let m = "app/models/purchase.rb";
    let edges = vec![
        Edge::new("app/services/refund_service.rb", m, RubyConstant, Exact).at(12).detail("Purchase").from_scope("run").to_scope("refund!"),
        Edge::new("app/models/session.rb", m, RubyConstant, Exact).at(5).detail("Purchase").from_scope("login").to_scope("charge!"),
        Edge::new("app/controllers/purchases_controller.rb", m, RubyConstant, Exact).at(8).detail("Purchase").from_scope("show").to_scope("charge!"),
        Edge::new("app/javascript/billing/api.ts", "app/controllers/purchases_controller.rb", ApiCall, Inferred).at(3).detail("GET /purchases/:id").to_scope("show"),
        Edge::new("spec/models/purchase_spec.rb", m, RubyConstant, Exact).at(2).detail("Purchase").to_scope("charge!"),
        Edge::new("app/models/sub.rb", m, RubyConstant, Exact).at(1).detail("Purchase").inherit(),
        Edge::new("lib/sub_user.rb", "app/models/sub.rb", RubyConstant, Exact).at(4).detail("Sub").to_scope("refund!"),
        Edge::new("lib/report.rb", m, RubyConstant, Exact).at(9).detail("Purchase").to_scope(SCOPE_BODY),
    ];
    Graph::build(&edges, [])
}

fn eval_on(g: &Graph, p: &Policy, changed: &[ChangedFile], file_level: bool) -> Report {
    let opts = EvalOptions { file_level, ..full() };
    evaluate_with(p, g, &PolicyIndex::build(p, g), changed, &opts)
}

/// Every impacted non-test file (the report must have been made with `all`).
fn impacted_set(r: &Report) -> BTreeSet<String> {
    assert!(r.query.all);
    r.dependents.iter().map(|f| f.file.clone()).chain(r.inherit_only.iter().cloned()).chain(r.further.files.iter().map(|f| f.file.clone())).collect()
}

#[test]
fn scoped_seeds_narrow_every_query() {
    let g = method_graph();
    let p = policy("rules:\n  - id: auth\n    risk: critical\n    when: {reaches: [auth]}\n  - id: boundary\n    risk: high\n    when: {crosses_boundary: true}\n  - id: untested\n    risk: medium\n    when: {untested: true}\n");
    let refund = [m("app/models/purchase.rb").in_scopes(["refund!"])];
    let r = eval_on(&g, &p, &refund, false);
    assert_eq!(impacted_set(&r), ["app/models/sub.rb", "app/services/refund_service.rb", "lib/sub_user.rb"].map(String::from).into());
    assert_eq!((r.counts.impacted, r.counts.impacted_file_level), (3, 7));
    assert!(r.tests.is_empty(), "the spec only exercises charge!");
    let fired: Vec<&str> = pol(&r).reasons.iter().map(|x| x.rule.as_str()).collect();
    assert_eq!(fired, ["untested"], "refund! reaches neither auth nor the frontend, and no test covers it");
    assert_eq!(r.changed[0].scopes, Some(vec!["refund!".to_string()]));
    assert!(!r.query.file_level);
    assert_eq!(r.changed[0].dependents, 6, "direct dependents stay the file-level fan-in");
    assert!(r.summary.contains("; 2 direct dependents; 1 more at further hops; 0 tests reach it; method-level (7 if whole files changed)"), "{}", r.summary);

    // The same diff analysed file-level: the auth and boundary rules fire.
    let w = eval_on(&g, &p, &refund, true);
    assert_eq!((w.counts.impacted, w.counts.impacted_file_level), (7, 7));
    let fired: Vec<&str> = pol(&w).reasons.iter().map(|x| x.rule.as_str()).collect();
    assert_eq!(fired, ["auth", "boundary"]);
    assert_eq!(w.changed[0].scopes, None);
    assert!(w.query.file_level);
    assert!(impacted_set(&r).is_subset(&impacted_set(&w)));

    // charge! reaches auth, the controller's #show and through it the frontend; the spec covers it.
    let r = eval_on(&g, &p, &[m("app/models/purchase.rb").in_scopes(["charge!"])], false);
    let fired: Vec<&str> = pol(&r).reasons.iter().map(|x| x.rule.as_str()).collect();
    assert_eq!(fired, ["auth", "boundary"]);
    assert_eq!(r.tests[0].file, "spec/models/purchase_spec.rb");
    let auth = &pol(&r).reasons[0].evidence.impacted[0];
    assert_eq!(auth.path[0].file, "app/models/session.rb");
    assert_eq!((auth.path[0].from_scope.as_deref(), auth.path[0].to_scope.as_deref()), (Some("login"), Some("charge!")));
    let api = &pol(&r).reasons[1].evidence.impacted[0];
    let hops: Vec<(&str, Option<&str>, Option<&str>)> = api.path.iter().map(|h| (h.file.as_str(), h.from_scope.as_deref(), h.to_scope.as_deref())).collect();
    assert_eq!(
        hops,
        [("app/javascript/billing/api.ts", None, Some("show")), ("app/controllers/purchases_controller.rb", Some("show"), Some("charge!")), ("app/models/purchase.rb", None, None)]
    );

    // Inheritance passes the scope on; @body is never shown.
    let r = eval_on(&g, &p, &[m("app/models/purchase.rb")], false);
    let sub = r.dependents.iter().find(|f| f.file == "app/models/sub.rb").unwrap();
    assert_eq!((sub.from_scope.as_deref(), sub.to_scope.as_deref(), sub.inherit), (None, None, true));
    let report = r.dependents.iter().find(|f| f.file == "lib/report.rb").unwrap();
    assert_eq!(report.to_scope, None, "@body is not shown");
    assert_eq!(r.counts.impacted, r.counts.impacted_file_level);

    // Explicit empty scope list: nothing another file depends on changed.
    let r = eval_on(&g, &p, &[m("app/models/purchase.rb").in_scopes(Vec::<String>::new())], false);
    assert_eq!(r.counts.impacted, 0);
    assert!(pol(&r).reasons.is_empty(), "not untested either: {:?}", pol(&r).reasons);
}

#[test]
fn scopes_ignored_unless_modified() {
    let g = method_graph();
    let p = policy("");
    // A rename or an added file is always whole-file, whatever the caller sent.
    let mut c = m("app/models/purchase.rb").in_scopes(["refund!"]);
    c.status = FileStatus::Added;
    let r = eval_on(&g, &p, &[c], false);
    assert_eq!((r.changed[0].scopes.clone(), r.counts.impacted), (None, 7));
}

#[test]
fn untested_uses_each_files_own_scopes() {
    // Two changed files: the spec is reached first from charge! of purchase.rb;
    // refund_service.rb's run has no test. The shared BFS must not credit the
    // spec to the wrong seed.
    let g = method_graph();
    let p = policy("rules:\n  - id: u\n    risk: medium\n    when: {untested: true}\n");
    let r = eval_on(&g, &p, &[m("app/models/purchase.rb").in_scopes(["charge!"]), m("app/services/refund_service.rb").in_scopes(["run"])], false);
    assert_eq!(pol(&r).reasons.len(), 1);
    assert_eq!(pol(&r).reasons[0].evidence.changed, ["app/services/refund_service.rb"]);
    let r = eval_on(&g, &p, &[m("app/models/purchase.rb").in_scopes(["charge!"])], false);
    assert!(pol(&r).reasons.is_empty());
}

/// xorshift64*: deterministic randomness without a dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn method_level_is_never_wider_than_file_level() {
    // Random graphs with scoped, inherited and same-file edges; random scoped
    // seeds. Method-level impact must be a subset of file-level impact, and
    // every rule built only from monotone conditions that fires method-level
    // must fire file-level too.
    use ripple_core::model::SCOPE_BODY;
    let files: Vec<String> = (0..14)
        .map(|i| match i % 4 {
            0 => format!("app/models/m{i}.rb"),
            1 => format!("app/services/s{i}.rb"),
            2 => format!("app/javascript/c{i}.ts"),
            _ => format!("spec/x{i}_spec.rb"),
        })
        .collect();
    let scopes = ["a", "b", "c", SCOPE_BODY];
    let rules = "rules:\n  - id: reach_billing\n    risk: critical\n    when: {reaches: [billing]}\n  - id: near_auth\n    risk: high\n    when: {reaches_tier: critical, within: 1}\n  - id: big\n    risk: medium\n    when: {impacted_files_over: 3}\n  - id: cross\n    risk: low\n    when: {crosses_boundary: true}\n";
    let p = policy(rules);
    let mut rng = Rng(0x9E3779B97F4A7C15);
    let (mut checked, mut narrower, mut fewer_rules) = (0, 0, 0);
    for _ in 0..300 {
        let n_edges = 4 + rng.below(30) as usize;
        let mut edges = Vec::new();
        for _ in 0..n_edges {
            let from = &files[rng.below(files.len() as u64) as usize];
            let to = &files[rng.below(files.len() as u64) as usize];
            let mut e = Edge::new(from.as_str(), to.as_str(), RubyConstant, Confidence::Exact);
            match rng.below(5) {
                0 => e = e.inherit(),
                1 => {}
                _ => {
                    if rng.below(2) == 0 {
                        e = e.from_scope(scopes[rng.below(3) as usize]);
                    }
                    if rng.below(3) != 0 {
                        e = e.to_scope(scopes[rng.below(4) as usize]);
                    }
                }
            }
            edges.push(e);
        }
        let g = Graph::build(&edges, files.iter().map(String::as_str));
        let n_seeds = 1 + rng.below(3) as usize;
        let changed: Vec<ChangedFile> = (0..n_seeds)
            .map(|_| {
                let f = m(&files[rng.below(files.len() as u64) as usize]);
                match rng.below(4) {
                    0 => f,
                    1 => f.in_scopes(Vec::<String>::new()),
                    _ => f.in_scopes((0..1 + rng.below(2)).map(|_| scopes[rng.below(4) as usize].to_string()).collect::<Vec<_>>()),
                }
            })
            .collect();
        for depth in [0, 1, 2] {
            let mut p = p.clone();
            p.config.query.max_depth = depth;
            let narrow = eval_on(&g, &p, &changed, false);
            let wide = eval_on(&g, &p, &changed, true);
            assert!(impacted_set(&narrow).is_subset(&impacted_set(&wide)), "{edges:?}\n{changed:?}");
            let tests = |r: &Report| r.tests.iter().map(|t| t.file.clone()).collect::<BTreeSet<_>>();
            assert!(tests(&narrow).is_subset(&tests(&wide)));
            assert_eq!(narrow.counts.impacted_file_level, wide.counts.impacted);
            assert!(narrow.counts.impacted <= narrow.counts.impacted_file_level);
            // `crosses_boundary` compares each impacted file with the one changed
            // file it is traced back to; with several changed files that choice
            // depends on search order, so it is monotone only for a single seed.
            let fired = |r: &Report| pol(&r).reasons.iter().map(|x| x.rule.clone()).filter(|id| n_seeds == 1 || id != "cross").collect::<BTreeSet<_>>();
            assert!(fired(&narrow).is_subset(&fired(&wide)), "{:?} vs {:?}\n{edges:?}\n{changed:?}", fired(&narrow), fired(&wide));
            if n_seeds == 1 {
                assert!(pol(&narrow).risk <= pol(&wide).risk);
            }
            checked += 1;
            narrower += usize::from(narrow.counts.impacted < wide.counts.impacted);
            fewer_rules += usize::from(fired(&narrow).len() < fired(&wide).len());
        }
    }
    assert_eq!(checked, 900);
    // The generator must actually exercise narrowing, or the test proves nothing.
    assert!(narrower > 100 && fewer_rules > 20, "narrower {narrower}, fewer rules {fewer_rules}");
}

fn scoped_sample() -> Report {
    let g = method_graph();
    let p = policy("");
    let mut r = eval_on(&g, &p, &[m("app/models/purchase.rb").in_scopes(["charge!", "refund!"]).lines(2, 1)], false);
    r.timing = Timing::default();
    r
}

#[test]
fn renderers_show_scopes_and_narrowing() {
    let r = scoped_sample();
    assert_eq!((r.counts.impacted, r.counts.impacted_file_level), (6, 7), "only lib/report.rb (class body) drops out");
    let md = crate::render::markdown::render(&r, 20);
    assert!(md.contains("**6** impacted files (method-level; 7 if whole files are assumed changed)"), "{md}");
    assert!(md.contains("| `app/models/purchase.rb` | modified | +2 −1 | body charge!, refund! |"), "{md}");
    assert!(md.contains("- `app/models/session.rb:5` [auth] → `app/models/purchase.rb` (ruby_constant, `Purchase#charge!`, in `login`)"), "{md}");
    assert!(md.contains("method-level analysis (1 of 1 changed file narrowed"), "{md}");
    assert!(!md.contains("@body"));
    let t = crate::render::text::render(&r, 20, false);
    assert!(t.lines().next().unwrap().contains("; method-level (7 if whole files changed)"), "{t}");
    assert!(t.ends_with("blind spots: methods matched by name; receivers untyped; JS/TS and send/try not checked; also run: rg -w -e 'charge!' -e 'refund!'\n"), "{t}");
    assert!(t.contains("body charge!, refund!"), "{t}");
    assert!(t.contains("  app/models/session.rb:5 in login  ruby_constant Purchase#charge!  [auth]\n"), "{t}");
    let j: serde_json::Value = serde_json::from_str(&crate::render::json::render(&r, false)).unwrap();
    assert_eq!(j["changed"][0]["scopes"], serde_json::json!(["charge!", "refund!"]));
    assert_eq!(j["counts"]["impacted_file_level"], 7);
    assert_eq!(j["query"]["file_level"], false);
    let session = j["dependents"].as_array().unwrap().iter().find(|f| f["file"] == "app/models/session.rb").unwrap();
    assert_eq!((session["from_scope"].as_str(), session["to_scope"].as_str()), (Some("login"), Some("charge!")));
    assert_eq!(crate::render::github::outputs(&r).lines().nth(3), Some("impacted_file_level=7"));

    let g = method_graph();
    let w = eval_on(&g, &policy(""), &[m("app/models/purchase.rb").in_scopes(["charge!"])], true);
    let md = crate::render::markdown::render(&w, 20);
    assert!(md.contains("**7** impacted files ·"), "{md}");
    assert!(md.contains("file-level analysis (every changed file treated as wholly changed"), "{md}");
    let md = crate::render::markdown::render(&sample(), 2);
    assert!(md.contains("**4** impacted files ·") && md.contains("method-level analysis (0 of 2 changed files narrowed"), "{md}");
}

// ---- removed methods -------------------------------------------------------

const PURCHASE_NEW: &str = "class Purchase\n  def charge!\n    refund_all!\n  end\n\n  def refund_all!\n  end\n\n  def retry!\n    refund!\n  end\nend\n";

/// purchase.rb renamed `refund!` to `refund_all!` but `retry!` still calls
/// `refund!`; refund_service.rb (a dependent) calls it; session.rb (a
/// dependent) only calls `charge!`; lib/report.rb has no edge to purchase.rb
/// but calls `refund!` on something.
fn removed_world() -> (std::path::PathBuf, Graph) {
    use Confidence::*;
    use ripple_core::mentions::{FileMentions, MentionTable, NO_DEF, name_id};
    let root = std::env::temp_dir().join(format!("ripple-removed-{}-{:?}", std::process::id(), std::thread::current().id()));
    let files = [
        ("app/models/purchase.rb", PURCHASE_NEW),
        ("app/services/refund_service.rb", "class RefundService\n  def run(p)\n    p.refund!\n  end\nend\n"),
        ("app/models/session.rb", "class Session\n  def login\n    Purchase.find(1).charge!\n  end\nend\n"),
        ("lib/report.rb", "# refund! used to live here\nx.refund!(1)\n"),
    ];
    for (p, text) in files {
        let path = root.join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let edges = vec![
        Edge::new("app/services/refund_service.rb", "app/models/purchase.rb", RubyConstant, Exact).at(3).detail("Purchase"),
        Edge::new("app/models/session.rb", "app/models/purchase.rb", RubyConstant, Exact).at(3).detail("Purchase"),
    ];
    let mut g = Graph::build(&edges, ["lib/report.rb"]);
    let fm = |names: &[&str]| {
        let mut pairs: Vec<(u32, u32)> = names.iter().map(|n| (name_id(n), NO_DEF)).collect();
        pairs.sort_unstable();
        std::sync::Arc::new(FileMentions { pairs: pairs.into(), ..Default::default() })
    };
    g.set_mentions(&MentionTable {
        files: vec![
            ("app/models/purchase.rb".into(), fm(&["refund_all!", "refund!"])),
            ("app/models/session.rb".into(), fm(&["charge!", "find"])),
            ("app/services/refund_service.rb".into(), fm(&["refund!"])),
            ("lib/report.rb".into(), fm(&["refund!"])),
        ],
        unmentioned: std::sync::Arc::from([]),
    });
    (root, g)
}

fn renamed_purchase() -> ChangedFile {
    let mut f = m("app/models/purchase.rb").lines(1, 1);
    f.hunks = Some(vec![Hunk { start: 6, count: 1, removes_definition: true }]);
    f.def_lines = vec![DefLine { removed: true, line: 6, text: "def refund!".into() }, DefLine { removed: false, line: 6, text: "def refund_all!".into() }];
    let ranges = [ScopeRange { name: "charge!".into(), start: 2, end: 4 }, ScopeRange { name: "refund_all!".into(), start: 6, end: 7 }, ScopeRange { name: "retry!".into(), start: 9, end: 11 }];
    f.change = Some(classify_change(&f, Some(&ranges), Some(&crate::changed::def_names(PURCHASE_NEW))));
    f
}

fn removed_sample() -> Report {
    let (root, g) = removed_world();
    let p = policy("");
    let opts = EvalOptions { root: Some(root.clone()), ..Default::default() };
    let mut r = evaluate_with(&p, &g, &PolicyIndex::build(&p, &g), &[renamed_purchase()], &opts);
    r.timing = Timing::default();
    let _ = std::fs::remove_dir_all(root);
    r
}

#[test]
fn removed_method_still_mentioned() {
    let r = removed_sample();
    assert_eq!(r.changed[0].change.kind, ChangeKind::MethodRemoved);
    assert_eq!(r.changed[0].change.added_methods, ["refund_all!"]);
    assert_eq!(r.counts.removed_methods, 1);
    let rm = &r.removed_methods[0];
    assert_eq!((rm.file.as_str(), rm.method.as_str(), rm.mentions_total), ("app/models/purchase.rb", "refund!", 3));
    let got: Vec<(&str, Option<u32>, MentionHow)> = rm.mentions.iter().map(|x| (x.file.as_str(), x.line, x.how)).collect();
    assert_eq!(
        got,
        [
            ("app/models/purchase.rb", Some(10), MentionHow::SameFile),
            ("app/services/refund_service.rb", Some(3), MentionHow::Dependent),
            ("lib/report.rb", Some(2), MentionHow::ByName),
        ],
        "session.rb only calls charge!"
    );
    // The dependent says which changed method it uses.
    let svc = r.dependents.iter().find(|d| d.file == "app/services/refund_service.rb").unwrap();
    assert_eq!(svc.uses_methods, ["refund!"]);
    let session = r.dependents.iter().find(|d| d.file == "app/models/session.rb").unwrap();
    assert!(session.uses_methods.is_empty());
    // Prominent: right after the summary line.
    let t = crate::render::text::render(&r, 30, false);
    let lines: Vec<&str> = t.lines().collect();
    assert!(lines[0].contains("1 removed method still mentioned"), "{t}");
    assert_eq!(lines[1], "REMOVED app/models/purchase.rb#refund! still mentioned in 3 files:");
    assert_eq!(lines[2], "  app/models/purchase.rb:10  same file");
    assert_eq!(lines[3], "  app/services/refund_service.rb:3  dependent");
    assert_eq!(lines[4], "  lib/report.rb:2  by name only");
    assert!(t.contains("changed M app/models/purchase.rb  ruby  +1 -1  removed refund!; added refund_all!  2 file dependents"), "{t}");
    assert!(t.contains("  app/services/refund_service.rb:3  ruby_constant Purchase  calls refund!  [billing]\n"), "{t}");
}

#[test]
fn removed_methods_without_file_access_use_the_index() {
    let (root, g) = removed_world();
    let _ = std::fs::remove_dir_all(root);
    let p = policy("");
    let r = evaluate_with(&p, &g, &PolicyIndex::build(&p, &g), &[renamed_purchase()], &EvalOptions::default());
    let got: Vec<(&str, Option<u32>)> = r.removed_methods[0].mentions.iter().map(|x| (x.file.as_str(), x.line)).collect();
    assert_eq!(got, [("app/models/purchase.rb", None), ("app/services/refund_service.rb", None), ("lib/report.rb", None)]);
}

#[test]
fn deleted_file_removes_every_method() {
    let (root, g) = removed_world();
    let mut f = ChangedFile::new("app/models/purchase.rb", FileStatus::Deleted);
    f.def_lines = ["def charge!", "def refund_all!", "def retry!"].iter().map(|t| DefLine { removed: true, line: 0, text: t.to_string() }).collect();
    f.change = Some(classify_change(&f, None, None));
    let p = policy("");
    let opts = EvalOptions { root: Some(root.clone()), ..Default::default() };
    let r = evaluate_with(&p, &g, &PolicyIndex::build(&p, &g), &[f], &opts);
    let _ = std::fs::remove_dir_all(root);
    assert_eq!(r.changed[0].change.removed_methods, ["charge!", "refund_all!", "retry!"]);
    let found: Vec<(&str, Vec<&str>)> = r.removed_methods.iter().map(|m| (m.method.as_str(), m.mentions.iter().map(|x| x.file.as_str()).collect())).collect();
    // refund_all! and retry! were only used inside the deleted file itself.
    assert_eq!(found, [("charge!", vec!["app/models/session.rb"])]);
    assert_eq!(r.removed_methods[0].mentions[0].line, Some(3));
}

#[test]
fn incomplete_is_never_silent() {
    use ripple_core::mentions::{FileMentions, MentionTable};
    let mut g = fixture_graph();
    g.set_mentions(&MentionTable {
        files: vec![("app/models/invoice.rb".into(), std::sync::Arc::new(FileMentions { opaque: true, ..Default::default() }))],
        unmentioned: std::sync::Arc::from([]),
    });
    let p = policy("");
    let r = evaluate_with(&p, &g, &PolicyIndex::build(&p, &g), &[m("app/models/invoice.rb"), ChangedFile::new("app/models/brand_new.rb", FileStatus::Added)], &EvalOptions::default());
    let inc: Vec<(&str, &str)> = r.incomplete.iter().map(|i| (i.reason.as_str(), i.file.as_deref().unwrap())).collect();
    assert_eq!(inc, [("not_in_graph", "app/models/brand_new.rb"), ("dynamic_dispatch", "app/models/invoice.rb")]);
    let t = crate::render::text::render(&r, 30, false);
    assert!(t.lines().next().unwrap().ends_with("may be incomplete (dynamic_dispatch, not_in_graph)"), "{t}");
    assert!(t.contains("\nINCOMPLETE app/models/brand_new.rb: not_in_graph: "), "{t}");
    // Nothing to report: no marker.
    let r = run(&p, &[m("lib/token.rb")]);
    assert!(r.incomplete.is_empty() && !r.summary.contains("incomplete"));
}

#[test]
fn policy_section_only_when_configured_and_fired() {
    let g = fixture_graph();
    // Built-in rules and no areas: never shown by default, even when a rule fires.
    let bare = Policy::compile(parse_config("", "t").unwrap()).unwrap();
    let r = evaluate(&bare, &g, &[m("config/initializers/stripe.rb")]);
    assert!(r.policy.is_none(), "{:?}", r.policy);
    // Areas configured but nothing fired: not shown.
    let p = policy("");
    let r = evaluate(&p, &g, &[m("README.md")]);
    assert!(r.policy.is_none());
    // Fired: shown compactly (one evidence path per reason, no area examples).
    let r = evaluate(&p, &g, &[m("app/models/invoice.rb")]);
    let pr = r.policy.as_ref().unwrap();
    assert_eq!(pr.risk, Risk::Critical);
    assert!(pr.reasons.iter().all(|x| x.evidence.impacted.len() <= budget::EVIDENCE));
    assert!(pr.areas.iter().all(|a| a.example.is_none()));
    // Asked for (check): always shown, with full evidence; the meaning does not change.
    let full_r = run(&p, &[m("app/models/invoice.rb")]);
    assert_eq!(pol(&full_r).risk, pr.risk);
    assert_eq!(pol(&full_r).reasons.iter().map(|x| &x.rule).collect::<Vec<_>>(), pr.reasons.iter().map(|x| &x.rule).collect::<Vec<_>>());
    assert!(pol(&full_r).areas.iter().all(|a| a.example.is_some()));
    let r = run(&bare, &[m("README.md")]);
    assert_eq!(pol(&r).risk, Risk::Low);
}

// ---- name lookups: callers first, heirs apart, mentions with no edge -------

/// link.rb defines `Link.fetch` (singleton) and `price`. a_controller calls
/// `Link.fetch`; b.rb references Link but calls some other `fetch`; heir.rb
/// only inherits; the view calls `price` on an untyped `@link`; lib/cache.rb
/// calls an unrelated `fetch` with no edge; lib/fetcher.rb defines another
/// `fetch`; the spec calls `Link.fetch`.
fn names_world() -> (std::path::PathBuf, Graph) {
    use Confidence::*;
    use ripple_core::mentions::{FileMentions, MentionTable, NO_DEF, name_id};
    let root = std::env::temp_dir().join(format!("ripple-names-{}-{:?}", std::process::id(), std::thread::current().id()));
    let files = [
        ("app/models/link.rb", "class Link\n  def self.fetch(p, user: nil)\n    find(p)\n  end\n\n  def price\n  end\nend\n"),
        ("app/controllers/a_controller.rb", "class AController\n  def show\n    Link.fetch(1)\n    x = 1\n    Link.fetch(2, user: nil)\n  end\nend\n"),
        ("app/models/b.rb", "class B\n  def x\n    Link.where(1)\n    params.fetch(:a)\n  end\nend\n"),
        ("app/models/heir.rb", "class Heir < Link\nend\n"),
        ("app/views/links/show.html.erb", "<h1>\n<%= @link.price %>\n"),
        ("lib/cache.rb", "Rails.cache.fetch(:k)\n"),
        ("lib/fetcher.rb", "class Fetcher\n  def fetch\n  end\nend\n"),
        ("spec/models/link_spec.rb", "describe Link do\n  it { Link.fetch('a') }\nend\n"),
    ];
    for (p, text) in files {
        let path = root.join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let l = "app/models/link.rb";
    let edges = vec![
        Edge::new("app/controllers/a_controller.rb", l, RubyConstant, Exact).at(3).detail("Link"),
        Edge::new("app/models/b.rb", l, RubyConstant, Exact).at(3).detail("Link"),
        Edge::new("app/models/heir.rb", l, RubyConstant, Exact).at(1).detail("Link").inherit(),
        Edge::new("spec/models/link_spec.rb", l, RubyConstant, Exact).at(1).detail("Link"),
    ];
    let mut g = Graph::build(&edges, ["app/views/links/show.html.erb", "lib/cache.rb", "lib/fetcher.rb"]);
    let fm = |names: &[&str], defs: &[&str]| {
        let mut pairs: Vec<(u32, u32)> = names.iter().map(|n| (name_id(n), NO_DEF)).collect();
        pairs.sort_unstable();
        let defs: Vec<u32> = defs.iter().map(|n| name_id(n)).collect();
        std::sync::Arc::new(FileMentions { pairs: pairs.into(), defs: defs.into(), ..Default::default() })
    };
    g.set_mentions(&MentionTable {
        files: vec![
            ("app/controllers/a_controller.rb".into(), fm(&["fetch"], &["show"])),
            ("app/models/b.rb".into(), fm(&["fetch", "where"], &["x"])),
            ("app/models/heir.rb".into(), fm(&[], &[])),
            ("app/models/link.rb".into(), fm(&["find"], &["fetch", "price"])),
            ("app/views/links/show.html.erb".into(), fm(&["price"], &[])),
            ("lib/cache.rb".into(), fm(&["fetch", "cache"], &[])),
            ("lib/fetcher.rb".into(), fm(&[], &["fetch"])),
            ("spec/models/link_spec.rb".into(), fm(&["fetch"], &[])),
        ],
        unmentioned: std::sync::Arc::from([]),
    });
    (root, g)
}

fn names_report(scope: &str) -> Report {
    let (root, g) = names_world();
    let p = policy("");
    let opts = EvalOptions { root: Some(root.clone()), ..Default::default() };
    let r = evaluate_with(&p, &g, &PolicyIndex::build(&p, &g), &[m("app/models/link.rb").in_scopes([scope])], &opts);
    let _ = std::fs::remove_dir_all(root);
    r
}

#[test]
fn callers_first_heirs_apart_and_call_lines() {
    let r = names_report("fetch");
    let deps: Vec<(&str, &[String], &[u32], Option<u32>)> =
        r.dependents.iter().map(|d| (d.file.as_str(), d.uses_methods.as_slice(), d.calls_on_constant.as_slice(), d.mention_line)).collect();
    let fetch = vec!["fetch".to_string()];
    assert_eq!(
        deps,
        [("app/controllers/a_controller.rb", fetch.as_slice(), [3u32, 5].as_slice(), Some(3)), ("app/models/b.rb", fetch.as_slice(), [].as_slice(), Some(4))],
        "the call on the constant first; the evidence line is the call, not the constant"
    );
    assert_eq!(r.inherit_only, ["app/models/heir.rb"]);
    assert_eq!((r.counts.dependents, r.counts.inherit_only), (3, 1));
    assert_eq!(r.tests[0].file, "spec/models/link_spec.rb");
    assert_eq!(r.tests[0].mention_line, Some(2));
    // lib/cache.rb mentions some `fetch` with no edge; two files define one.
    let a = &r.also_mentioned[0];
    assert_eq!((a.name.as_str(), a.definitions, a.total, a.singleton), ("fetch", 2, 1, true));
    assert!(a.files.is_empty(), "a singleton's by-name mentions are counted, not listed");
    let t = crate::render::text::render(&r, 30, false);
    assert!(t.starts_with("1 changed file; 2 direct dependents (+1 only inherit/include); 1 more mention the name with no edge;"), "{t}");
    assert!(t.contains("\n  app/controllers/a_controller.rb:3  ruby_constant Link  calls fetch on the constant at 3,5\n  app/models/b.rb:4  ruby_constant Link  calls fetch\n"), "{t}");
    assert!(t.contains("\ninherits/includes, does not call it 1: app/models/heir.rb\n"), "{t}");
    assert!(t.contains("\nalso mentions `fetch` (no dependency edge; receiver type unknown) 1 file, name is common: 2 definitions; singleton method, so calls without the constant are likely other methods (--all lists them)\n"), "{t}");
    assert!(t.contains("\n  spec/models/link_spec.rb:2\n"), "{t}");
    assert!(t.ends_with("also run: rg -w fetch\n"), "{t}");
    assert!(crate::schema::validate_report(&serde_json::to_value(&r).unwrap()).is_empty());
    // With --all they are listed, confirmed in the text.
    let (root, g) = names_world();
    let p = policy("");
    let opts = EvalOptions { root: Some(root.clone()), all: true, ..Default::default() };
    let r = evaluate_with(&p, &g, &PolicyIndex::build(&p, &g), &[m("app/models/link.rb").in_scopes(["fetch"])], &opts);
    let _ = std::fs::remove_dir_all(root);
    let a = &r.also_mentioned[0];
    assert_eq!((a.files[0].file.as_str(), a.files[0].line), ("lib/cache.rb", Some(1)));
}

#[test]
fn mentions_with_no_edge_are_listed() {
    let r = names_report("price");
    // Nobody with an edge calls price; the view does, through an untyped @link.
    assert!(r.dependents.is_empty(), "{:?}", r.dependents);
    assert_eq!(r.inherit_only, ["app/models/heir.rb"]);
    let a = &r.also_mentioned[0];
    assert_eq!((a.name.as_str(), a.definitions, a.total, a.singleton), ("price", 1, 1, false));
    assert_eq!((a.files[0].file.as_str(), a.files[0].line), ("app/views/links/show.html.erb", Some(2)));
    let t = crate::render::text::render(&r, 30, false);
    assert!(t.contains("\nalso mentions `price` (no dependency edge; receiver type unknown) 1 file, defined only here:\n  app/views/links/show.html.erb:2\n"), "{t}");
    // A name nobody else mentions says so instead of staying silent.
    let (root, g) = names_world();
    let _ = std::fs::remove_dir_all(&root);
    let p = policy("");
    let r = evaluate_with(&p, &g, &PolicyIndex::build(&p, &g), &[m("app/models/link.rb").in_scopes(["nothing_calls_me"])], &EvalOptions::default());
    let t = crate::render::text::render(&r, 30, false);
    assert!(t.contains("also mentions `nothing_calls_me`: no file outside the dependents (defined only here)"), "{t}");
}

#[test]
fn ripple_config_is_not_a_change() {
    let (root, g) = names_world();
    let _ = std::fs::remove_dir_all(&root);
    let p = policy("");
    let r = evaluate_with(&p, &g, &PolicyIndex::build(&p, &g), &[m("ripple.yml"), m("app/models/heir.rb")], &EvalOptions::default());
    assert_eq!(r.counts.changed, 1);
    assert!(r.warnings.iter().any(|w| w.code == "ripple_config_changed"));
}

#[test]
fn many_deleted_fixtures_do_not_flood_the_report() {
    let g = Graph::build(&[], ["app/a.rb"]);
    let p = policy("");
    let changed: Vec<ChangedFile> = (0..40).map(|i| ChangedFile::new(format!("spec/fixtures/cassette_{i:02}.yml"), FileStatus::Deleted)).collect();
    let r = evaluate_with(&p, &g, &PolicyIndex::build(&p, &g), &changed, &EvalOptions::default());
    let not_indexed: Vec<&Warning> = r.warnings.iter().filter(|w| w.code == "deleted_file_not_indexed").collect();
    assert_eq!(not_indexed.len(), budget::PER_KIND + 1, "{:#?}", r.warnings);
    assert_eq!(not_indexed.last().unwrap().message, format!("+{} more files like this (--all)", 40 - budget::PER_KIND));
    assert!(r.incomplete.len() <= budget::PER_KIND + 1, "{:#?}", r.incomplete);
    assert!(r.tests.len() <= budget::TESTS && r.counts.tests == 40, "changed tests are cut to the budget too");
    let all = evaluate_with(&p, &g, &PolicyIndex::build(&p, &g), &changed, &EvalOptions { all: true, ..Default::default() });
    assert_eq!(all.warnings.iter().filter(|w| w.code == "deleted_file_not_indexed").count(), 40);
    assert_eq!(all.tests.len(), 40);
}

// ---- signal conditions (churn, complexity, coverage, hotspot, co-change) ----

use crate::signals::{ChurnMetric, CoChangeEntry, CoverageMetric, FileSignals, HotspotMetric, SignalInput, SignalState, SignalStatus};
use ripple_core::metrics::{FileMetrics, MethodMetric, MetricsTable};

/// The fixture graph with complexity metrics for two files.
fn metrics_graph() -> Graph {
    let mut g = fixture_graph();
    let inv_methods = [MethodMetric { start: 3, span: 20, complexity: 30 }, MethodMetric { start: 30, span: 5, complexity: 4 }];
    let table = MetricsTable::build(
        [
            ("app/models/invoice.rb", FileMetrics { lines: 200, complexity: 120, methods: 2, max_method: 30 }, &inv_methods[..]),
            ("lib/util.rb", FileMetrics { lines: 10, complexity: 2, methods: 0, max_method: 0 }, &[][..]),
        ],
        true,
    );
    g.set_metrics([std::sync::Arc::new(table)]);
    g
}

fn ready_signals() -> SignalInput {
    let mut s = SignalInput { hotspot_percent: 5.0, ..Default::default() };
    s.files.insert(
        "app/models/invoice.rb".into(),
        FileSignals {
            churn: Some(ChurnMetric { commits: 60, recent_commits: 41, last_changed: Some(1_700_000_000) }),
            coverage: Some(CoverageMetric { percent: Some(12.0), covered: 24, coverable: 200, changed_lines_percent: Some(20.0), stale: false }),
            hotspot: Some(HotspotMetric { rank: 3, of: 2114, percentile: 99.9, hotspot: true }),
        },
    );
    s.files.insert(
        "lib/util.rb".into(),
        FileSignals {
            churn: Some(ChurnMetric { commits: 2, recent_commits: 0, last_changed: None }),
            coverage: Some(CoverageMetric { percent: Some(0.0), covered: 0, coverable: 40, changed_lines_percent: None, stale: false }),
            hotspot: Some(HotspotMetric { rank: 900, of: 2114, percentile: 57.4, hotspot: false }),
        },
    );
    // app/services/charge.rb: in no coverage report (unknown), no history.
    let partner = |path: &str, via: &str, t: u32, of: u32| CoChangeEntry { path: path.into(), via: via.into(), together: t, of, why: crate::signals::cochange_why(via, t, of) };
    s.partners = vec![(partner("app/views/invoices/show.html.erb", "app/models/invoice.rb", 12, 20), 0.6), (partner("config/locales/en.yml", "app/models/invoice.rb", 6, 20), 0.3)];
    s.usually_changes_with = vec![partner("app/views/invoices/show.html.erb", "app/models/invoice.rb", 12, 20)];
    s
}

fn run_sig(rules: &str, changed: &[ChangedFile], sig: Option<SignalInput>) -> Report {
    let p = policy(rules);
    let g = metrics_graph();
    evaluate_with(&p, &g, &PolicyIndex::build(&p, &g), changed, &EvalOptions { signals: sig, ..full() })
}

/// The single rule `r` with `when`: its message if it fired.
fn sig_fires(when: &str, changed: &[ChangedFile], sig: Option<SignalInput>) -> (Option<String>, Report) {
    let r = run_sig(&format!("rules:\n  - id: r\n    risk: high\n    when: {when}\n"), changed, sig);
    // Messages are sentences; compare from the second word on.
    let msg = pol(&r).reasons.first().map(|x| {
        let mut c = x.message.chars();
        c.next().map(|f| f.to_lowercase().chain(c).collect()).unwrap_or_default()
    });
    (msg, r)
}

fn msg(when: &str, changed: &[ChangedFile]) -> Option<String> {
    sig_fires(when, changed, Some(ready_signals())).0
}

fn not_ready(mut s: SignalInput, f: impl FnOnce(&mut crate::signals::Signals)) -> SignalInput {
    f(&mut s.status);
    s
}

fn assert_blocked(when: &str, changed: &[ChangedFile], sig: SignalInput, signal: &str) {
    let (m, r) = sig_fires(when, changed, Some(sig));
    assert_eq!(m, None, "{when}: a rule whose signal is not ready must not fire");
    let w: Vec<&Warning> = r.warnings.iter().filter(|w| w.code == "rule_signal_not_ready").collect();
    assert_eq!(w.len(), 1, "{:#?}", r.warnings);
    assert!(w[0].message.contains(&format!("needs {signal}")), "{}", w[0].message);
}

#[test]
fn churn_over() {
    let inv = [m("app/models/invoice.rb")];
    let s = msg("{churn_over: 40}", &inv).expect("fires");
    assert!(s.contains("`app/models/invoice.rb` (41 commits)") && s.contains("more than 40 commits in the last 90 days"), "{s}");
    assert_eq!(msg("{churn_over: 41}", &inv), None);
    // No history for the file: unknown, never fires.
    assert_eq!(msg("{churn_over: 0}", &[m("app/services/charge.rb")]), None);
    // Tests do not count.
    assert_eq!(msg("{churn_over: 0}", &[m("spec/models/invoice_spec.rb")]), None);
    assert_blocked("{churn_over: 1}", &inv, not_ready(ready_signals(), |s| s.churn = SignalStatus::new(SignalState::Building, Some("reading git history".into()))), "churn");
}

#[test]
fn complexity_over_and_max_method() {
    let inv = [m("app/models/invoice.rb")];
    let s = msg("{complexity_over: 100}", &inv).expect("fires");
    assert!(s.contains("complexity above 100: `app/models/invoice.rb` (complexity 120)"), "{s}");
    assert_eq!(msg("{complexity_over: 120}", &inv), None);
    let s = msg("{max_method_complexity_over: 25}", &inv).expect("fires");
    assert!(s.contains("a method more complex than 25: `app/models/invoice.rb` (a method of complexity 30)"), "{s}");
    assert_eq!(msg("{max_method_complexity_over: 30}", &inv), None);
    // A file without methods has no max method; one without metrics no complexity.
    assert_eq!(msg("{max_method_complexity_over: 0}", &[m("lib/util.rb")]), None);
    assert_eq!(msg("{complexity_over: 0}", &[m("app/services/charge.rb")]), None);
    // An index without metrics tables: complexity unavailable, the rule warns.
    let p = policy("rules:\n  - {id: r, risk: high, when: {complexity_over: 1}}\n");
    let g = fixture_graph();
    let r = evaluate_with(&p, &g, &PolicyIndex::build(&p, &g), &inv, &EvalOptions { signals: Some(ready_signals()), ..full() });
    assert!(pol(&r).reasons.is_empty());
    assert!(r.warnings.iter().any(|w| w.code == "rule_signal_not_ready" && w.message.contains("needs complexity")), "{:#?}", r.warnings);
}

#[test]
fn coverage_under_ignores_unknown() {
    let inv = [m("app/models/invoice.rb")];
    let s = msg("{coverage_under: 50}", &inv).expect("fires");
    assert!(s.contains("line coverage under 50%: `app/models/invoice.rb` (12% of 200 lines)"), "{s}");
    assert_eq!(msg("{coverage_under: 12}", &inv), None);
    // charge.rb is in no report: unknown never fires.
    assert_eq!(msg("{coverage_under: 100}", &[m("app/services/charge.rb")]), None);
    assert_blocked(
        "{coverage_under: 50}",
        &inv,
        not_ready(ready_signals(), |s| s.coverage = SignalStatus::new(SignalState::Unavailable, Some("no coverage report found".into()))),
        "coverage",
    );
    assert_blocked("{coverage_under: 50}", &inv, not_ready(ready_signals(), |s| s.coverage = SignalStatus::new(SignalState::Loading, None)), "coverage");
}

#[test]
fn changed_lines_coverage_under() {
    let inv = [m("app/models/invoice.rb").lines(10, 2)];
    let s = msg("{changed_lines_coverage_under: 50}", &inv).expect("fires");
    assert!(s.contains("changed lines covered under 50%: `app/models/invoice.rb` (20% of the changed lines)"), "{s}");
    assert_eq!(msg("{changed_lines_coverage_under: 20}", &inv), None);
    // util.rb has coverage but no changed-lines figure; charge.rb none at all.
    assert_eq!(msg("{changed_lines_coverage_under: 100}", &[m("lib/util.rb").lines(1, 0), m("app/services/charge.rb").lines(1, 0)]), None);
    // Without a diff: a warning, and no fire.
    let (fired, r) = sig_fires("{changed_lines_coverage_under: 100}", &[m("lib/util.rb")], Some(ready_signals()));
    assert_eq!(fired, None);
    assert!(r.warnings.iter().any(|w| w.code == "no_line_counts"), "{:#?}", r.warnings);
    assert_blocked("{changed_lines_coverage_under: 50}", &inv, not_ready(ready_signals(), |s| s.coverage = SignalStatus::new(SignalState::Loading, None)), "coverage");
}

#[test]
fn uncovered_both_polarities_and_unknown() {
    let util = [m("lib/util.rb")];
    let s = msg("{uncovered: true}", &util).expect("fires");
    assert!(s.contains("not covered by any test: `lib/util.rb` (0 of 40 lines covered)"), "{s}");
    assert_eq!(msg("{uncovered: false}", &util), None);
    assert_eq!(msg("{uncovered: true}", &[m("app/models/invoice.rb")]), None);
    assert!(msg("{uncovered: false}", &[m("app/models/invoice.rb")]).is_some());
    // Unknown (in no report) counts only with uncovered_includes_unknown.
    let charge = [m("app/services/charge.rb")];
    assert_eq!(msg("{uncovered: true}", &charge), None);
    let s = msg("{uncovered: true, uncovered_includes_unknown: true}", &charge).expect("fires");
    assert!(s.contains("`app/services/charge.rb` (in no coverage report)"), "{s}");
    assert_blocked("{uncovered: true}", &util, not_ready(ready_signals(), |s| s.coverage = SignalStatus::new(SignalState::Unavailable, Some("no report".into()))), "coverage");
}

#[test]
fn hotspot_both_polarities() {
    let s = msg("{hotspot: true}", &[m("app/models/invoice.rb")]).expect("fires");
    assert!(s.contains("a hotspot (top 5% of `ripple rank`): `app/models/invoice.rb` (#3 of 2114)"), "{s}");
    assert_eq!(msg("{hotspot: true}", &[m("lib/util.rb")]), None);
    assert!(msg("{hotspot: false}", &[m("lib/util.rb")]).is_some());
    assert_eq!(msg("{hotspot: true}", &[m("app/services/charge.rb")]), None);
    assert_blocked("{hotspot: true}", &[m("app/models/invoice.rb")], not_ready(ready_signals(), |s| s.hotspot = SignalStatus::new(SignalState::Building, None)), "hotspot");
}

#[test]
fn missing_cochange_threshold() {
    let inv = [m("app/models/invoice.rb")];
    let s = msg("{missing_cochange: true}", &inv).expect("fires");
    assert!(s.contains("usually changes with files not in the diff (50%+ of commits): `app/views/invoices/show.html.erb` (with `app/models/invoice.rb` in 12 of 20 commits)"), "{s}");
    assert!(!s.contains("en.yml"), "{s}");
    let s = msg("{missing_cochange: true, cochange_min_confidence: 0.25}", &inv).expect("fires");
    assert!(s.contains("en.yml"), "{s}");
    assert_eq!(msg("{missing_cochange: true, cochange_min_confidence: 0.9}", &inv), None);
    assert!(msg("{missing_cochange: false, cochange_min_confidence: 0.9}", &inv).is_some());
    let mut none = ready_signals();
    none.partners.clear();
    assert_eq!(sig_fires("{missing_cochange: true}", &inv, Some(none)).0, None);
    assert_blocked(
        "{missing_cochange: true}",
        &inv,
        not_ready(ready_signals(), |s| s.cochange = SignalStatus::new(SignalState::Disabled, Some("history.cochange is off".into()))),
        "cochange",
    );
}

#[test]
fn signal_conditions_are_anded_with_each_other_and_the_rest() {
    let inv = [m("app/models/invoice.rb")];
    let s = msg("{touches: [billing], churn_over: 10, complexity_over: 100, coverage_under: 50}", &inv).expect("fires");
    assert!(s.contains("41 commits") && s.contains("complexity 120") && s.contains("12% of 200 lines"), "{s}");
    assert_eq!(msg("{touches: [auth], churn_over: 10}", &inv), None);
    assert_eq!(msg("{churn_over: 10, coverage_under: 5}", &inv), None);
    // Several files: every per-file condition must hold on the same file.
    let two = [m("app/models/invoice.rb"), m("lib/util.rb")];
    assert_eq!(msg("{churn_over: 10, uncovered: true}", &two), None, "churn is invoice's, 0% coverage util's");
    assert_eq!(msg("{complexity_over: 100, coverage_under: 5}", &two), None);
    let s = msg("{churn_over: 10, complexity_over: 100}", &two).expect("both on invoice");
    assert!(
        s.contains("changed in more than 10 commits in the last 90 days and complexity above 100 in the same file: `app/models/invoice.rb` (41 commits, complexity 120)"),
        "{s}"
    );
    assert!(!s.contains("util"), "{s}");
    // One blocked signal blocks the whole rule, with one warning naming it.
    let (fired, r) = sig_fires(
        "{churn_over: 10, coverage_under: 50}",
        &inv,
        Some(not_ready(ready_signals(), |s| s.coverage = SignalStatus::new(SignalState::Loading, None))),
    );
    assert_eq!(fired, None);
    assert_eq!(r.warnings.iter().filter(|w| w.code == "rule_signal_not_ready").count(), 1);
}

#[test]
fn without_signal_input_signal_rules_warn() {
    let (fired, r) = sig_fires("{churn_over: 1}", &[m("app/models/invoice.rb")], None);
    assert_eq!(fired, None);
    assert!(r.warnings.iter().any(|w| w.code == "rule_signal_not_ready" && w.message.contains("not provided by the caller")), "{:#?}", r.warnings);
    // ... and the report carries neither signals nor history-based metrics.
    assert!(r.signals.is_none());
    let mx = r.changed[0].metrics.as_ref().expect("complexity alone");
    assert_eq!((mx.complexity, mx.max_method, mx.churn), (Some(120), Some(30), None));
}

#[test]
fn report_metrics_signals_and_usually_changes_with() {
    let r = run_sig("", &[m("app/models/invoice.rb"), m("spec/models/invoice_spec.rb"), m("app/services/charge.rb")], Some(ready_signals()));
    let inv = r.changed.iter().find(|c| c.path == "app/models/invoice.rb").unwrap();
    let mx = inv.metrics.as_ref().unwrap();
    assert_eq!(mx.churn.unwrap().recent_commits, 41);
    assert_eq!(mx.coverage.unwrap().percent, Some(12.0));
    assert!(mx.hotspot.unwrap().hotspot);
    assert_eq!(crate::signals::metrics_line(mx), "cx 120 (max 30) · churn 41/90d · cov 12% (changed lines 20%) · hotspot #3");
    assert!(r.changed.iter().find(|c| c.path == "spec/models/invoice_spec.rb").unwrap().metrics.is_none(), "tests have no metrics");
    assert_eq!(r.usually_changes_with.len(), 1);
    assert_eq!(r.signals.as_ref().unwrap().coverage.state, SignalState::Ready);
    // Not-ready signals drop their metrics and partners, and the text says so.
    let sig = not_ready(ready_signals(), |s| {
        s.churn = SignalStatus::new(SignalState::Building, Some("reading git history".into()));
        s.cochange = SignalStatus::new(SignalState::Building, Some("reading git history".into()));
        s.coverage = SignalStatus::new(SignalState::Disabled, None);
    });
    let r = run_sig("", &[m("app/models/invoice.rb")], Some(sig));
    let mx = r.changed[0].metrics.as_ref().unwrap();
    assert_eq!((mx.churn, mx.coverage), (None, None));
    assert!(r.usually_changes_with.is_empty());
    let t = crate::render::text::render(&r, 30, false);
    assert!(t.contains("\n  cx 120 (max 30) · hotspot #3\n"), "{t}");
    assert!(t.contains("not ready: churn building (reading git history); cochange building (reading git history)\n"), "{t}");
    assert!(!t.contains("coverage disabled"), "disabled signals are not 'not ready': {t}");
    // The schema accepts it all.
    let v = serde_json::to_value(run_sig("", &[m("app/models/invoice.rb").lines(3, 1)], Some(ready_signals()))).unwrap();
    assert_eq!(crate::schema::validate_report(&v), Vec::<String>::new());
    let t = crate::render::text::render(&serde_json::from_value(v).unwrap(), 30, false);
    assert!(t.contains("usually changes with (not in the diff):\n  app/views/invoices/show.html.erb  with app/models/invoice.rb in 12 of its 20 commits (60%)\n"), "{t}");
}

#[test]
fn changed_lines_over_is_per_file_next_to_metric_conditions() {
    // invoice (complexity 120) barely touched, util (complexity 2) rewritten.
    let tiny_edit = [m("app/models/invoice.rb").lines(3, 2), m("lib/util.rb").lines(80, 40)];
    assert_eq!(msg("{complexity_over: 100, changed_lines_over: 50}", &tiny_edit), None, "125 lines in the diff, 5 in the complex file");
    // Alone it is still the whole diff.
    let s = msg("{changed_lines_over: 50}", &tiny_edit).expect("fires");
    assert!(s.contains("125 lines changed (more than 50)"), "{s}");
    let big_edit = [m("app/models/invoice.rb").lines(40, 20), m("lib/util.rb").lines(1, 0)];
    let (s, r) = sig_fires("{complexity_over: 100, changed_lines_over: 50}", &big_edit, Some(ready_signals()));
    let s = s.expect("fires");
    assert!(s.contains("complexity above 100 and more than 50 changed lines in the same file: `app/models/invoice.rb` (complexity 120, 60 lines changed)"), "{s}");
    let ev = &pol(&r).reasons[0].evidence;
    assert_eq!((ev.counts.get("complexity"), ev.counts.get("changed_lines")), (Some(&120), Some(&60)));
    assert_eq!(ev.changed, vec!["app/models/invoice.rb".to_string()]);
    // Without a diff: no fire and a warning.
    let (fired, r) = sig_fires("{complexity_over: 100, changed_lines_over: 1}", &[m("app/models/invoice.rb")], Some(ready_signals()));
    assert_eq!(fired, None);
    assert!(r.warnings.iter().any(|w| w.code == "no_line_counts"), "{:#?}", r.warnings);
}

#[test]
fn untested_joins_the_per_file_conditions() {
    // charge.rb has no test among its dependents; invoice.rb has a spec.
    let both = [m("app/models/invoice.rb").lines(200, 0), m("app/services/charge.rb").lines(2, 0)];
    assert_eq!(msg("{untested: true, changed_lines_over: 50}", &both), None, "the big edit is to the tested file");
    let both = [m("app/models/invoice.rb").lines(2, 0), m("app/services/charge.rb").lines(60, 0)];
    let s = msg("{untested: true, changed_lines_over: 50}", &both).expect("fires");
    assert!(s.contains("no test among its dependents and more than 50 changed lines in the same file: `app/services/charge.rb` (no test depends on it, 60 lines changed)"), "{s}");
    // Alone, `untested` keeps its own message.
    let s = msg("{untested: true}", &both).expect("fires");
    assert!(s.contains("no test depends on `app/services/charge.rb`"), "{s}");
    // With a metric condition: invoice is complex but tested, charge untested but has no metrics.
    assert_eq!(msg("{untested: true, complexity_over: 100}", &both), None);
}
