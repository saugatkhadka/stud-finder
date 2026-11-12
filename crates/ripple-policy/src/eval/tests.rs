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
        "risk=critical\nrequire=human_review,run_full_suite\nimpacted=4\ntests=1\nchanged=2\n"
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
