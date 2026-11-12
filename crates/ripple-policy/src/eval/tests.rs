use super::*;
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

fn run(p: &Policy, changed: &[ChangedFile]) -> Report {
    evaluate(p, &fixture_graph(), changed)
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
    assert!(r.reasons.len() <= 1);
    !r.reasons.is_empty()
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
    assert!(r.reasons.is_empty());
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
    assert_eq!(r.impacted.total, 1, "report itself still honours max_depth");
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
    assert_eq!(r.risk, Risk::Critical);
    assert_eq!(r.require, ["x", "y", "z"]);
    assert_eq!(r.reasons.iter().map(|x| x.rule.as_str()).collect::<Vec<_>>(), ["b", "a"]);
    let r = run(&p, &[m("README.md")]);
    assert_eq!(r.risk, Risk::Low);
    assert!(r.reasons.is_empty() && r.require.is_empty());
}

#[test]
fn default_rules() {
    let p = policy("");
    let ids = |r: &Report| r.reasons.iter().map(|x| x.rule.clone()).collect::<Vec<_>>();

    let r = run(&p, &[m("app/models/invoice.rb")]);
    assert_eq!(r.risk, Risk::Critical);
    assert!(r.require.contains(&"human_review".to_string()));
    assert!(ids(&r).contains(&"default.touches_critical".to_string()));
    assert!(ids(&r).contains(&"default.crosses_boundary".to_string()));
    assert!(r.reasons.iter().all(|x| x.builtin));

    let r = run(&p, &[m("lib/util.rb")]);
    assert_eq!((r.risk, ids(&r)), (Risk::High, vec!["default.touches_high".to_string()]));
    assert_eq!(r.require, ["human_review"]);

    let r = run(&p, &[m("config/initializers/stripe.rb")]);
    assert_eq!((r.risk, ids(&r)), (Risk::High, vec!["default.touches_global".to_string()]));
    assert_eq!(r.require, ["run_full_suite"]);

    // lib/token.rb is tier high; session.rb (critical) depends on it directly.
    let r = run(&p, &[m("lib/token.rb")]);
    assert!(ids(&r).contains(&"default.reaches_critical_direct".to_string()));
    assert!(!ids(&r).contains(&"default.reaches_critical".to_string()), "lower rung of the same ladder is dropped");

    let r = run(&p, &[m("README.md")]);
    assert_eq!(r.risk, Risk::Low);
    assert!(r.reasons.is_empty());
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
    assert_eq!(r.risk, Risk::Medium);
    assert_eq!(r.reasons[0].rule, "default.impacted_over_50");
    let r = evaluate(&p, &g, &[m("app/x.rb")]);
    assert_eq!(r.risk, Risk::Medium);
    assert_eq!(r.reasons.iter().map(|x| x.rule.as_str()).collect::<Vec<_>>(), ["default.reaches_critical"]);

    let edges: Vec<Edge> = (0..201).map(|i| Edge::new(format!("app/a{i}.rb"), "app/hub.rb", RubyConstant, Exact)).collect();
    let g = Graph::build(&edges, []);
    let r = evaluate(&p, &g, &[m("app/hub.rb")]);
    assert_eq!(r.risk, Risk::High);
    assert_eq!(r.reasons[0].rule, "default.impacted_over_200");
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

    assert_eq!(r.impacted.total, 4);
    assert_eq!(r.impacted.tests, 1);
    assert_eq!(r.tests_to_run.iter().map(|t| t.file.as_str()).collect::<Vec<_>>(), ["spec/models/invoice_spec.rb"]);
    assert_eq!(r.impacted.by_depth, BTreeMap::from([(1, 1), (2, 1), (3, 1), (4, 1)]));
    assert_eq!(r.impacted.by_language, BTreeMap::from([(Language::Ruby, 2), (Language::Js, 2)]));
    assert_eq!(r.impacted.by_min_confidence, BTreeMap::from([(Confidence::Heuristic, 2), (Confidence::Inferred, 1), (Confidence::Exact, 1)]));
    let checkout = r.impacted.files.iter().find(|f| f.file == "app/javascript/pages/checkout.tsx").unwrap();
    assert_eq!(checkout.depth, 4);
    assert_eq!(checkout.via.as_deref(), Some("app/javascript/billing/api.ts"));
    assert_eq!(checkout.root, "app/models/invoice.rb");
    assert_eq!((checkout.confidence, checkout.path_confidence), (Some(Confidence::Exact), Some(Confidence::Heuristic)));
    assert_eq!(checkout.areas, ["frontend"]);

    // Explanation path in the crosses_boundary evidence.
    let cross = r.reasons.iter().find(|x| x.rule == "default.crosses_boundary").unwrap();
    assert_eq!(cross.evidence.changed, ["app/models/invoice.rb"]);
    let api = &cross.evidence.impacted[0];
    assert_eq!(api.file, "app/javascript/billing/api.ts");
    let files: Vec<&str> = api.path.iter().map(|h| h.file.as_str()).collect();
    assert_eq!(files, ["app/javascript/billing/api.ts", "app/controllers/invoices_controller.rb", "app/services/charge.rb", "app/models/invoice.rb"]);
    assert_eq!(api.path[0].kind, Some(ApiCall));
    assert_eq!(api.path[0].line, Some(12));
    assert_eq!(api.path[0].detail.as_deref(), Some("POST /invoices"));
    assert_eq!(api.path[3].kind, None);

    let names: Vec<(&str, bool, u32)> = r.areas_reached.iter().map(|a| (a.area.as_str(), a.touched, a.min_depth)).collect();
    assert_eq!(names, [("billing", true, 0), ("frontend", false, 3)]);
    let billing = &r.areas_reached[0];
    assert_eq!(billing.files, 3, "invoice + charge + api.ts");
    assert_eq!(r.owners, ["@payments", "@web"]);
}

#[test]
fn limit_truncates_lists_not_counts() {
    let p = policy("");
    let opts = EvalOptions { limit: Some(1), ..Default::default() };
    let g = fixture_graph();
    let r = evaluate_with(&p, &g, &PolicyIndex::build(&p, &g), &[m("app/models/invoice.rb")], &opts);
    assert_eq!(r.impacted.files.len(), 1);
    assert_eq!(r.impacted.total, 4);
    assert!(r.impacted.truncated);
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
        "| `app/models/invoice.rb` | modified | +3 −1 | ruby | billing | 2 |",
        "| `config/initializers/stripe.rb` (**global**) | modified |  | ruby |  | 0 |",
        "### Impacted files (4)",
        "- `app/services/charge.rb` [billing] ← `app/models/invoice.rb` (ruby_constant, `Invoice`)",
        "…and 2 more (see the JSON report for the full list).",
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
    let first = t.lines().next().unwrap();
    assert_eq!(first, "CRITICAL risk  2 changed, 4 impacted (depth ≤4), 1 test to run  gen 0");
    assert!(t.contains("  M app/models/invoice.rb  ruby  +3 -1  [billing]  2 dependents"), "{t}");
    assert!(t.contains("impacted by depth: 1:1  2:1  3:1  4:1"), "{t}");
    assert!(t.contains("warning: config/initializers/stripe.rb: global: static impact"), "{t}");
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
    assert!(lines.iter().any(|l| l.starts_with("::warning file=config/initializers/stripe.rb,line=7,title=ripple%3A high risk (default.touches_global)::")), "{cmds}");
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
fn json_is_complete() {
    let j: serde_json::Value = serde_json::from_str(&crate::render::json::render(&sample(), false)).unwrap();
    for key in ["schema_version", "risk", "summary", "reasons", "require", "changed", "impacted", "tests_to_run", "areas_reached", "owners", "warnings", "query", "timing", "index"] {
        assert!(j.get(key).is_some(), "missing {key}");
    }
    assert_eq!(j["risk"], "critical");
    assert_eq!(j["impacted"]["by_depth"]["1"], 1);
    assert_eq!(j["impacted"]["by_edge_kind"]["api_call"], 1);
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
    let opts = EvalOptions { file_level, ..Default::default() };
    evaluate_with(p, g, &PolicyIndex::build(p, g), changed, &opts)
}

fn impacted_set(r: &Report) -> BTreeSet<String> {
    r.impacted.files.iter().map(|f| f.file.clone()).collect()
}

#[test]
fn scoped_seeds_narrow_every_query() {
    let g = method_graph();
    let p = policy("rules:\n  - id: auth\n    risk: critical\n    when: {reaches: [auth]}\n  - id: boundary\n    risk: high\n    when: {crosses_boundary: true}\n  - id: untested\n    risk: medium\n    when: {untested: true}\n");
    let refund = [m("app/models/purchase.rb").in_scopes(["refund!"])];
    let r = eval_on(&g, &p, &refund, false);
    assert_eq!(impacted_set(&r), ["app/models/sub.rb", "app/services/refund_service.rb", "lib/sub_user.rb"].map(String::from).into());
    assert_eq!((r.impacted.total, r.impacted.file_level_total), (3, 7));
    assert!(r.tests_to_run.is_empty(), "the spec only exercises charge!");
    let fired: Vec<&str> = r.reasons.iter().map(|x| x.rule.as_str()).collect();
    assert_eq!(fired, ["untested"], "refund! reaches neither auth nor the frontend, and no test covers it");
    assert_eq!(r.changed[0].scopes, Some(vec!["refund!".to_string()]));
    assert!(!r.query.file_level);
    assert_eq!(r.changed[0].dependents, 6, "direct dependents stay the file-level fan-in");
    assert!(r.summary.contains("3 impacted files (up to 2 hops) [method-level; 7 if whole files are assumed changed]"), "{}", r.summary);

    // The same diff analysed file-level: the auth and boundary rules fire.
    let w = eval_on(&g, &p, &refund, true);
    assert_eq!((w.impacted.total, w.impacted.file_level_total), (7, 7));
    let fired: Vec<&str> = w.reasons.iter().map(|x| x.rule.as_str()).collect();
    assert_eq!(fired, ["auth", "boundary"]);
    assert_eq!(w.changed[0].scopes, None);
    assert!(w.query.file_level);
    assert!(impacted_set(&r).is_subset(&impacted_set(&w)));

    // charge! reaches auth, the controller's #show and through it the frontend; the spec covers it.
    let r = eval_on(&g, &p, &[m("app/models/purchase.rb").in_scopes(["charge!"])], false);
    let fired: Vec<&str> = r.reasons.iter().map(|x| x.rule.as_str()).collect();
    assert_eq!(fired, ["auth", "boundary"]);
    assert_eq!(r.tests_to_run[0].file, "spec/models/purchase_spec.rb");
    assert_eq!(r.tests_to_run[0].to_scope.as_deref(), Some("charge!"));
    let auth = &r.reasons[0].evidence.impacted[0];
    assert_eq!(auth.path[0].file, "app/models/session.rb");
    assert_eq!((auth.path[0].from_scope.as_deref(), auth.path[0].to_scope.as_deref()), (Some("login"), Some("charge!")));
    let api = &r.reasons[1].evidence.impacted[0];
    let hops: Vec<(&str, Option<&str>, Option<&str>)> = api.path.iter().map(|h| (h.file.as_str(), h.from_scope.as_deref(), h.to_scope.as_deref())).collect();
    assert_eq!(
        hops,
        [("app/javascript/billing/api.ts", None, Some("show")), ("app/controllers/purchases_controller.rb", Some("show"), Some("charge!")), ("app/models/purchase.rb", None, None)]
    );

    // Inheritance passes the scope on; @body is never shown.
    let r = eval_on(&g, &p, &[m("app/models/purchase.rb")], false);
    let sub = r.impacted.files.iter().find(|f| f.file == "app/models/sub.rb").unwrap();
    assert_eq!((sub.from_scope.as_deref(), sub.to_scope.as_deref()), (None, None));
    let report = r.impacted.files.iter().find(|f| f.file == "lib/report.rb").unwrap();
    assert_eq!(report.to_scope, None, "@body is not shown");
    assert_eq!(r.impacted.total, r.impacted.file_level_total);

    // Explicit empty scope list: nothing another file depends on changed.
    let r = eval_on(&g, &p, &[m("app/models/purchase.rb").in_scopes(Vec::<String>::new())], false);
    assert_eq!(r.impacted.total, 0);
    assert!(r.reasons.is_empty(), "not untested either: {:?}", r.reasons);
}

#[test]
fn scopes_ignored_unless_modified() {
    let g = method_graph();
    let p = policy("");
    // A rename or an added file is always whole-file, whatever the caller sent.
    let mut c = m("app/models/purchase.rb").in_scopes(["refund!"]);
    c.status = FileStatus::Added;
    let r = eval_on(&g, &p, &[c], false);
    assert_eq!((r.changed[0].scopes.clone(), r.impacted.total), (None, 7));
}

#[test]
fn untested_uses_each_files_own_scopes() {
    // Two changed files: the spec is reached first from charge! of purchase.rb;
    // refund_service.rb's run has no test. The shared BFS must not credit the
    // spec to the wrong seed.
    let g = method_graph();
    let p = policy("rules:\n  - id: u\n    risk: medium\n    when: {untested: true}\n");
    let r = eval_on(&g, &p, &[m("app/models/purchase.rb").in_scopes(["charge!"]), m("app/services/refund_service.rb").in_scopes(["run"])], false);
    assert_eq!(r.reasons.len(), 1);
    assert_eq!(r.reasons[0].evidence.changed, ["app/services/refund_service.rb"]);
    let r = eval_on(&g, &p, &[m("app/models/purchase.rb").in_scopes(["charge!"])], false);
    assert!(r.reasons.is_empty());
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
            let tests = |r: &Report| r.tests_to_run.iter().map(|t| t.file.clone()).collect::<BTreeSet<_>>();
            assert!(tests(&narrow).is_subset(&tests(&wide)));
            assert_eq!(narrow.impacted.file_level_total, wide.impacted.total);
            assert!(narrow.impacted.total <= narrow.impacted.file_level_total);
            // `crosses_boundary` compares each impacted file with the one changed
            // file it is traced back to; with several changed files that choice
            // depends on search order, so it is monotone only for a single seed.
            let fired = |r: &Report| r.reasons.iter().map(|x| x.rule.clone()).filter(|id| n_seeds == 1 || id != "cross").collect::<BTreeSet<_>>();
            assert!(fired(&narrow).is_subset(&fired(&wide)), "{:?} vs {:?}\n{edges:?}\n{changed:?}", fired(&narrow), fired(&wide));
            if n_seeds == 1 {
                assert!(narrow.risk <= wide.risk);
            }
            checked += 1;
            narrower += usize::from(narrow.impacted.total < wide.impacted.total);
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
    assert_eq!((r.impacted.total, r.impacted.file_level_total), (6, 7), "only lib/report.rb (class body) drops out");
    let md = crate::render::markdown::render(&r, 20);
    assert!(md.contains("**6** impacted files (method-level; 7 if whole files are assumed changed)"), "{md}");
    assert!(md.contains("| `app/models/purchase.rb` · methods: `charge!`, `refund!` | modified | +2 −1 |"), "{md}");
    assert!(md.contains("`app/models/session.rb#login` → (ruby_constant `Purchase#charge!` line 5) `app/models/purchase.rb`"), "{md}");
    assert!(md.contains("method-level analysis (1 of 1 changed file narrowed"), "{md}");
    assert!(!md.contains("@body"));
    let t = crate::render::text::render(&r, 20, false);
    assert!(t.lines().next().unwrap().contains("6 impacted (depth ≤2; 7 if whole files changed)"), "{t}");
    assert!(t.contains("methods: charge!, refund!"), "{t}");
    assert!(t.contains("app/models/session.rb#login  <- app/models/purchase.rb#charge! (ruby_constant)"), "{t}");
    assert!(t.trim_end().ends_with("dependencies followed per method / controller action)"), "{t}");
    let j: serde_json::Value = serde_json::from_str(&crate::render::json::render(&r, false)).unwrap();
    assert_eq!(j["changed"][0]["scopes"], serde_json::json!(["charge!", "refund!"]));
    assert_eq!(j["impacted"]["file_level_total"], 7);
    assert_eq!(j["query"]["file_level"], false);
    let session = j["impacted"]["files"].as_array().unwrap().iter().find(|f| f["file"] == "app/models/session.rb").unwrap();
    assert_eq!((session["from_scope"].as_str(), session["to_scope"].as_str()), (Some("login"), Some("charge!")));
    assert_eq!(crate::render::github::outputs(&r).lines().nth(3), Some("impacted_file_level=7"));
    assert!(crate::render::github::workflow_commands(&r).contains("[method-level; 7 if whole files are assumed changed]"));

    // Many methods are capped; file-level reports say so and never claim narrowing.
    let mut many = r.clone();
    many.changed[0].scopes = Some((1..=7).map(|i| format!("m{i}")).collect());
    let md = crate::render::markdown::render(&many, 20);
    assert!(md.contains("methods: `m1`, `m2`, `m3`, `m4` +3 |"), "{md}");
    let g = method_graph();
    let w = eval_on(&g, &policy(""), &[m("app/models/purchase.rb").in_scopes(["charge!"])], true);
    let md = crate::render::markdown::render(&w, 20);
    assert!(md.contains("**7** impacted files ·"), "{md}");
    assert!(md.contains("file-level analysis (every changed file treated as wholly changed"), "{md}");
    assert!(!md.contains("methods:"));
    // Unchanged reports (no scopes anywhere) keep the old headline.
    let md = crate::render::markdown::render(&sample(), 2);
    assert!(md.contains("**4** impacted files ·") && md.contains("method-level analysis (0 of 2 changed files narrowed"), "{md}");
}
