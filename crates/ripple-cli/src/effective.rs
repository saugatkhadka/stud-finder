//! `ripple config show` and `ripple init`: the configuration in effect,
//! with what was built in or auto-detected and what it matches here.

use crate::engine::Snapshot;
use ripple_policy::{Distribution, Policy};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::path::Path;

const PERCENTS: [f64; 4] = [1.0, 2.0, 5.0, 10.0];

fn dist(d: &Distribution) -> Value {
    let top: serde_json::Map<String, Value> = PERCENTS.iter().map(|&p| (format!("{p}"), json!(d.threshold(p)))).collect();
    json!({"files": d.len(), "top_percent": top})
}

fn tier(r: ripple_core::Risk) -> String {
    format!("{r:?}").to_lowercase()
}

/// The effective configuration (`config show --format json`). Stable keys:
/// `source`, `areas[]`, `rules[]`, `tests`, `global_files`, `ignore`,
/// `codeowners`, `coverage`, `distributions`, `config`.
pub fn show(snap: &Snapshot) -> Value {
    let p = &snap.policy;
    let mut counts = vec![0usize; p.areas().len()];
    let (mut tests, mut globals) = (0usize, 0usize);
    for f in snap.files.iter() {
        for a in p.areas_of(f) {
            counts[a as usize] += 1;
        }
        tests += p.is_test(f) as usize;
        globals += p.is_global(f) as usize;
    }
    show_with(p, &snap.root, &counts, tests, globals, Some(&snap.index))
}

pub fn show_with(p: &Policy, root: &Path, counts: &[usize], tests: usize, globals: usize, index: Option<&ripple_policy::PolicyIndex>) -> Value {
    let areas: Vec<Value> = p
        .areas()
        .iter()
        .zip(counts)
        .map(|(a, n)| {
            json!({"name": a.name, "tier": tier(a.tier), "owners": a.owners, "description": a.description, "paths": a.paths, "builtin": a.builtin, "files": n})
        })
        .collect();
    let rules: Vec<Value> = p
        .rules_with_origin()
        .map(|(r, builtin)| json!({"id": r.id, "risk": tier(r.risk), "require": r.require, "description": r.description, "when": r.when_json(), "builtin": builtin}))
        .collect();
    let reports: Vec<String> = ripple_coverage::discover_reports(root, &p.config.coverage)
        .iter()
        .map(|r| r.strip_prefix(root).unwrap_or(r).display().to_string())
        .collect();
    json!({
        "source": p.source,
        "defaults": p.config.defaults,
        "areas": areas,
        "rules": rules,
        "tests": {"globs": p.config.tests, "files": tests},
        "global_files": {"globs": p.config.global_files, "files": globals},
        "ignore": p.config.ignore,
        "codeowners": p.codeowners().map(|c| json!({"path": c.path, "rules": c.rules()})),
        "coverage": {
            "enabled": p.config.coverage.enabled,
            "configured": p.config.coverage.paths,
            "auto_detect": p.config.coverage.paths.is_empty(),
            "reports": reports,
        },
        "distributions": index.map(|i| json!({
            "complexity": dist(&i.complexity),
            "max_method_complexity": dist(&i.max_method),
            "dependents": dist(&i.dependents),
            "non_test_files": i.non_test_files,
        })),
        "config": p.config,
    })
}

trait WhenJson {
    fn when_json(&self) -> Value;
}
impl WhenJson for ripple_core::config::Rule {
    /// `when` without its unset keys.
    fn when_json(&self) -> Value {
        let mut v = serde_json::to_value(&self.when).unwrap_or(Value::Null);
        if let Some(o) = v.as_object_mut() {
            o.retain(|k, x| !(x.is_null() || x == &json!([]) || (k == "uncovered_includes_unknown" && x == &json!(false))));
            // f32 fields (cochange_min_confidence) would show as 0.699999988.
            for x in o.values_mut() {
                if let Some(f) = x.as_f64().filter(|_| x.is_f64()) {
                    *x = json!((f * 1e6).round() / 1e6);
                }
            }
        }
        v
    }
}

fn when_flow(w: &Value) -> String {
    let parts: Vec<String> = w
        .as_object()
        .map(|o| {
            o.iter()
                .map(|(k, v)| {
                    let v = match v {
                        Value::Array(a) => format!("[{}]", a.iter().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect::<Vec<_>>().join(", ")),
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    format!("{k}: {v}")
                })
                .collect()
        })
        .unwrap_or_default();
    format!("{{{}}}", parts.join(", "))
}

/// `config show` as text.
pub fn text(v: &Value) -> String {
    let mut s = String::new();
    let src = v["source"].as_str().map(str::to_string).unwrap_or_else(|| "none (built-in defaults only)".into());
    let _ = writeln!(s, "config: {src}");
    let _ = writeln!(s, "tests: {} files   global_files: {} files", v["tests"]["files"], v["global_files"]["files"]);
    match v["codeowners"].as_object() {
        Some(c) => {
            let _ = writeln!(s, "codeowners: {} ({} rules; owners of changed files join the report)", c["path"].as_str().unwrap_or(""), c["rules"]);
        }
        None => {
            let _ = writeln!(s, "codeowners: none found");
        }
    }
    let reports: Vec<&str> = v["coverage"]["reports"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let how = if v["coverage"]["auto_detect"] == Value::Bool(true) { "auto-detected" } else { "configured" };
    let _ = writeln!(s, "coverage: {}", if reports.is_empty() { format!("no report found ({how})") } else { format!("{} ({how})", reports.join(", ")) });
    let _ = writeln!(s, "\nareas (built-in ones only where they match files):");
    for a in v["areas"].as_array().into_iter().flatten().filter(|a| a["builtin"] != Value::Bool(true) || a["files"].as_u64() != Some(0)) {
        let origin = if a["builtin"] == Value::Bool(true) { "built-in" } else { "ripple.yml" };
        let files = a["files"].as_u64().unwrap_or(0);
        let owners: Vec<&str> = a["owners"].as_array().map(|o| o.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        let _ = writeln!(s, "  {:<18} {:<8} {:>6} files  {:<10} {}", a["name"].as_str().unwrap_or(""), a["tier"].as_str().unwrap_or(""), files, origin, owners.join(" "));
    }
    let _ = writeln!(s, "\nrules:");
    for r in v["rules"].as_array().into_iter().flatten() {
        let origin = if r["builtin"] == Value::Bool(true) { "built-in" } else { "ripple.yml" };
        let req: Vec<&str> = r["require"].as_array().map(|o| o.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        let _ = writeln!(s, "  {:<30} {:<8} {:<10} {}", r["id"].as_str().unwrap_or(""), r["risk"].as_str().unwrap_or(""), origin, when_flow(&r["when"]));
        if !req.is_empty() {
            let _ = writeln!(s, "  {:<30} requires {}", "", req.join(", "));
        }
    }
    if let Some(d) = v["distributions"].as_object() {
        let _ = writeln!(s, "\nrelative thresholds here (top 1% / 2% / 5% / 10% of {} non-test files):", d["non_test_files"]);
        for key in ["complexity", "max_method_complexity", "dependents"] {
            let t = &d[key]["top_percent"];
            let cut = |p: &str| t[p].as_u64().map(|x| x.to_string()).unwrap_or_else(|| "-".into());
            let _ = writeln!(s, "  {:<22} >= {} / {} / {} / {}   ({} files)", key, cut("1"), cut("2"), cut("5"), cut("10"), d[key]["files"]);
        }
    }
    let d = &v["defaults"];
    let _ = write!(
        s,
        "\nbuilt-ins: areas {}, rules {}, codeowners {}{}\nturn one off in ripple.yml with `defaults: {{disable: [NAME]}}`; redefine an area or rule id to replace it.",
        on(&d["areas"]),
        on(&d["rules"]),
        on(&d["codeowners"]),
        d["disable"].as_array().filter(|a| !a.is_empty()).map(|a| format!("; disabled: {}", a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))).unwrap_or_default()
    );
    s
}

fn on(v: &Value) -> &'static str {
    if v == &Value::Bool(false) { "off" } else { "on" }
}

/// `ripple init`: a commented ripple.yml spelling out this repository's
/// effective defaults (which built-in areas match here, the rules), all
/// commented out except the version, so the file changes nothing until edited.
pub fn init_yaml(v: &Value) -> String {
    let mut s = String::new();
    s.push_str(
        "# ripple.yml: optional. Without it ripple uses the built-in areas and rules\n\
         # below; this file only overrides or extends them.\n\
         #\n\
         # Merge rules: an area or rule here with the same name / id as a built-in\n\
         # replaces it; anything else is added. `defaults.disable` turns built-ins off\n\
         # by name; `defaults: {areas: false}` / `{rules: false}` drops them all.\n\
         # See the effective result any time with `ripple config show`.\n\
         # Every `when` key is listed in examples/ripple.yml and the README.\n\
         version: 1\n\n",
    );
    s.push_str("# defaults:\n#   areas: true        # built-in areas (only where their paths exist)\n#   rules: true        # built-in default.* rules\n#   codeowners: true   # owners from CODEOWNERS join the report\n#   disable: []        # e.g. [complex_file, jobs]\n\n");
    s.push_str("# Built-in areas matching files in this repository (redefine one to change it):\n# areas:\n");
    for a in v["areas"].as_array().into_iter().flatten().filter(|a| a["builtin"] == Value::Bool(true)) {
        let n = a["files"].as_u64().unwrap_or(0);
        if n == 0 {
            continue;
        }
        let _ = writeln!(s, "#   {}:              # {} files here", a["name"].as_str().unwrap_or(""), n);
        let _ = writeln!(s, "#     tier: {}", a["tier"].as_str().unwrap_or(""));
        let _ = writeln!(s, "#     owners: []");
        let _ = writeln!(s, "#     paths:");
        for p in a["paths"].as_array().into_iter().flatten() {
            let _ = writeln!(s, "#       - \"{}\"", p.as_str().unwrap_or(""));
        }
    }
    let unmatched: Vec<&str> =
        v["areas"].as_array().into_iter().flatten().filter(|a| a["builtin"] == Value::Bool(true) && a["files"].as_u64() == Some(0)).filter_map(|a| a["name"].as_str()).collect();
    if !unmatched.is_empty() {
        let _ = writeln!(s, "# (built-in areas with no files here: {})", unmatched.join(", "));
    }
    s.push_str("#\n#   Add your own, e.g. a critical area:\n#   billing:\n#     tier: critical\n#     owners: [\"@payments-team\"]\n#     paths: [\"app/models/billing/**\"]\n\n");
    s.push_str("# Built-in rules (redefine an id to change it, or disable it under defaults):\n# rules:\n");
    for r in v["rules"].as_array().into_iter().flatten().filter(|r| r["builtin"] == Value::Bool(true)) {
        let _ = writeln!(s, "#   - id: {}", r["id"].as_str().unwrap_or(""));
        let _ = writeln!(s, "#     risk: {}", r["risk"].as_str().unwrap_or(""));
        let _ = writeln!(s, "#     when: {}", when_flow(&r["when"]));
        let req: Vec<&str> = r["require"].as_array().map(|o| o.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        if !req.is_empty() {
            let _ = writeln!(s, "#     require: [{}]", req.join(", "));
        }
    }
    s.push_str("\n# ci:\n#   fail_on: high       # `ripple check` / `ripple impact --fail-on` default\n");
    s
}
