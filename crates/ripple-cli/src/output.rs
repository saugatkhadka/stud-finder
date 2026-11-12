//! Plain-text rendering of the small (non-report) query results.

use serde_json::Value;

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn edge_desc(v: &Value) -> String {
    let mut parts = vec![s(&v["kind"]).to_string()];
    if v["inherit"] == Value::Bool(true) {
        parts.push("inherits".into());
    }
    match (v["detail"].as_str(), v["to_scope"].as_str()) {
        (Some(d), Some(t)) if !d.ends_with(&format!("#{t}")) => parts.push(format!("`{d}#{t}`")),
        (Some(d), _) => parts.push(format!("`{d}`")),
        (None, Some(t)) => parts.push(format!("`#{t}`")),
        (None, None) => {}
    }
    if let Some(l) = v["line"].as_u64() {
        parts.push(format!("line {l}"));
    }
    if let Some(c) = v["confidence"].as_str().filter(|c| *c != "exact") {
        parts.push(c.to_string());
    }
    parts.join(" ")
}

/// `file#scope` when the reference sits in a method / action.
fn file_in(v: &Value) -> String {
    match v["from_scope"].as_str() {
        Some(sc) => format!("{}#{sc}", s(&v["file"])),
        None => s(&v["file"]).to_string(),
    }
}

fn scopes_suffix(v: &Value) -> String {
    let names: Vec<&str> = v["scopes"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).collect();
    if names.is_empty() { String::new() } else { format!("#{}", names.join(",#")) }
}

fn unknown_note(v: &Value) -> String {
    let names: Vec<&str> = v["unknown_scopes"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).collect();
    if names.is_empty() { String::new() } else { format!("warning: no method or action named {} in {}\n", names.join(", "), v["to"].as_str().or(v["file"].as_str()).unwrap_or("")) }
}

pub fn neighbours(v: &Value) -> String {
    let dir = s(&v["direction"]);
    let depth = v["depth"].as_u64().unwrap_or(1);
    let mut o = unknown_note(v);
    o += &format!(
        "{} {} of {}{}{}\n",
        v["count"].as_u64().unwrap_or(0),
        dir,
        s(&v["file"]),
        scopes_suffix(v),
        if depth == 1 { " (direct)".to_string() } else if depth == 0 { " (all depths)".to_string() } else { format!(" (up to {depth} hops)") }
    );
    for f in v["files"].as_array().into_iter().flatten() {
        let via = if f["depth"].as_u64() == Some(1) { String::new() } else { format!("  via {}", s(&f["via"])) };
        o.push_str(&format!("  {} {}  ({}){via}\n", f["depth"], file_in(f), edge_desc(f)));
    }
    o
}

pub fn why(v: &Value) -> String {
    if v["found"] != Value::Bool(true) {
        let mut o = unknown_note(v);
        o += &format!("no dependency path between {} and {}{}\n", s(&v["from"]), s(&v["to"]), scopes_suffix(v));
        if v["file_level_path"] == Value::Bool(true) {
            o.push_str("(the files are linked, but not through the methods / controller actions involved; `ripple dependents --depth 0` lists file-level links)\n");
        }
        return o;
    }
    let hops = v["path"].as_array().cloned().unwrap_or_default();
    let mut o = String::new();
    for (i, h) in hops.iter().enumerate() {
        let file = if i + 1 < hops.len() { file_in(h) } else { s(&h["file"]).to_string() };
        o.push_str(&format!("{}{}\n", "  ".repeat(i), file));
        if i + 1 < hops.len() {
            o.push_str(&format!("{}  depends on ({})\n", "  ".repeat(i), edge_desc(h)));
        }
    }
    o
}

pub fn areas(v: &Value) -> String {
    let mut o = format!("{}\n", s(&v["file"]));
    let areas = v["areas"].as_array().cloned().unwrap_or_default();
    if areas.is_empty() {
        o.push_str("  areas: none\n");
    }
    for a in areas {
        let owners: Vec<&str> = a["owners"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).collect();
        o.push_str(&format!("  area {} ({}) {}\n", s(&a["area"]), s(&a["tier"]), owners.join(" ")));
    }
    o.push_str(&format!(
        "  language {}  test {}  global {}  ignored {}  indexed {}  dependents {}\n",
        s(&v["language"]),
        v["is_test"],
        v["is_global"],
        v["is_ignored"],
        v["in_graph"],
        v["dependents"]
    ));
    o
}

pub fn stats(v: &Value) -> String {
    let mut o = format!(
        "root {}\ngeneration {}  files {} ({})  nodes {}  edges {}  tombstones {}\nscopes {} in {} files (method-level seeds; 0 = every change is whole-file)\n",
        s(&v["root"]),
        v["generation"],
        v["files"],
        s(&v["listing"]),
        v["nodes"],
        v["edges"],
        v["tombstones"],
        v["scopes"],
        v["files_with_scopes"]
    );
    if let Some(t) = v["timings_us"].as_object() {
        let parts: Vec<String> = t.iter().map(|(k, x)| format!("{}={:.1}ms", k.trim_end_matches("_us"), x.as_f64().unwrap_or(0.0) / 1000.0)).collect();
        o.push_str(&format!("last pass: {}\n", parts.join(" ")));
    }
    if let Some(ex) = v["extractors"].as_object() {
        for (name, st) in ex {
            let parts: Vec<String> = st.as_object().into_iter().flatten().map(|(k, x)| format!("{k}={x}")).collect();
            o.push_str(&format!("  {name}: {}\n", parts.join(" ")));
        }
    }
    o.push_str(&format!("warnings {}", v["warnings_total"]));
    if let Some(by) = v["warnings_by_code"].as_object().filter(|m| !m.is_empty()) {
        let parts: Vec<String> = by.iter().map(|(k, n)| format!("{k}={n}")).collect();
        o.push_str(&format!(" ({})", parts.join(" ")));
    }
    o.push('\n');
    let pol = &v["policy"];
    o.push_str(&format!(
        "policy {}  rules {}{}\n",
        pol["source"].as_str().unwrap_or("(defaults)"),
        pol["rules"].as_array().map_or(0, |r| r.len()),
        if pol["default_rules"] == Value::Bool(true) { " (built-in)" } else { "" }
    ));
    let d = &v["daemon"];
    if d.is_object() {
        o.push_str(&format!(
            "daemon pid {}  version {}  uptime {:.1}s  requests {}  pending {}  indexing {}\nsocket {}\n",
            d["pid"],
            s(&d["version"]),
            d["uptime_ms"].as_f64().unwrap_or(0.0) / 1000.0,
            d["requests"],
            d["pending_changes"],
            d["indexing"],
            s(&d["socket"])
        ));
    } else {
        o.push_str("daemon: not used (in-process index)\n");
    }
    o
}

/// "coverage unavailable (no coverage report found ...)" for every signal
/// that is neither ready nor turned off; empty when all are.
fn signals_line(v: &Value) -> String {
    let Ok(sig) = serde_json::from_value::<ripple_policy::Signals>(v.clone()) else { return String::new() };
    let parts: Vec<String> = sig
        .not_ready()
        .into_iter()
        .map(|(name, st)| match &st.reason {
            Some(r) => format!("{name} {} ({r})", st.state.as_str()),
            None => format!("{name} {}", st.state.as_str()),
        })
        .collect();
    if parts.is_empty() { String::new() } else { format!("not ready: {}\n", parts.join("; ")) }
}

/// `ripple file` (at most ~40 lines).
pub fn file_profile(v: &Value) -> String {
    let mut o = String::new();
    let mut head = vec![s(&v["language"]).to_string()];
    let areas: Vec<String> = v["areas"].as_array().into_iter().flatten().map(|a| format!("{} ({})", s(&a["area"]), s(&a["tier"]))).collect();
    if !areas.is_empty() {
        head.push(format!("area {}", areas.join(", ")));
    }
    let owners: Vec<&str> = v["owners"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if !owners.is_empty() {
        head.push(format!("owners {}", owners.join(" ")));
    }
    if v["is_test"] == Value::Bool(true) {
        head.push("test".into());
    }
    if v["is_global"] == Value::Bool(true) {
        head.push("global (static impact is not reliable)".into());
    }
    if v["in_graph"] != Value::Bool(true) {
        head.push("not in the dependency graph".into());
    }
    o += &format!("{}\n  {}\n", s(&v["file"]), head.join(" · "));
    let m: ripple_policy::Metrics = serde_json::from_value(v["metrics"].clone()).unwrap_or_default();
    let mut size = Vec::new();
    if let Some(l) = m.lines {
        size.push(format!("{l} code lines"));
    }
    if let Some(n) = v["methods"].as_u64().filter(|n| *n > 0) {
        size.push(format!("{n} methods"));
    }
    let line = ripple_policy::metrics_line(&m);
    if !line.is_empty() || !size.is_empty() {
        o += &format!("  {}\n", [size.join(", "), line].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" · "));
    }
    if let Some(c) = m.churn {
        let last = c.last_changed.map(|t| format!(", last {}", days_ago(t))).unwrap_or_default();
        o += &format!("  churn: {} commits in the history window, {} in the last 90 days{last}\n", c.commits, c.recent_commits);
    }
    if let Some(c) = m.coverage {
        o += &format!("  coverage: {} of {} lines{}{}\n", c.covered, c.coverable, c.percent.map(|p| format!(" ({}%)", ripple_policy::fmt_pct(p))).unwrap_or_default(), if c.stale { ", stale (measured before HEAD or too long ago)" } else { "" });
    }
    if let Some(h) = m.hotspot {
        o += &format!("  hotspot: rank {} of {} (percentile {}){}\n", h.rank, v["ranked"], ripple_policy::fmt_pct(h.percentile), if h.hotspot { ", a hotspot" } else { "" });
    }
    let top: Vec<&Value> = v["top_methods"].as_array().into_iter().flatten().collect();
    if !top.is_empty() {
        o += "  most complex methods:\n";
        for t in top {
            let cov = t["coverage"].as_f64().map(|c| format!("  cov {}%", ripple_policy::fmt_pct(c))).unwrap_or_default();
            o += &format!("    {:>4}  {} (lines {}-{}){cov}\n", t["complexity"].as_u64().unwrap_or(0), t["name"].as_str().unwrap_or("?"), t["start"], t["end"]);
        }
    }
    let unc: Vec<&str> = v["uncovered_methods"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if !unc.is_empty() {
        let shown: Vec<&str> = unc.iter().take(8).copied().collect();
        let more = if unc.len() > 8 { format!(" and {} more", unc.len() - 8) } else { String::new() };
        o += &format!("  uncovered methods: {}{more}\n", shown.join(", "));
    }
    let d = &v["dependents"];
    let top: Vec<String> = d["top"].as_array().into_iter().flatten().map(|x| format!("{} ({})", s(&x["file"]), x["dependents"])).collect();
    o += &format!("  dependents: {}{}\n", d["count"], if top.is_empty() { String::new() } else { format!(", most depended-on: {}", top.join(", ")) });
    o += &format!("  dependencies: {}\n", v["dependencies"]["count"]);
    let t = &v["tests"];
    let files: Vec<&str> = t["files"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if files.is_empty() {
        o += "  tests depending on it directly: none\n";
    } else {
        o += &format!("  tests depending on it directly: {}\n", t["count"]);
        for f in files.iter().take(5) {
            o += &format!("    {f}\n");
        }
        let n = t["count"].as_u64().unwrap_or(0) as usize;
        if n > 5 {
            o += &format!("    +{} more\n", n - 5);
        }
    }
    let partners: Vec<&Value> = v["usually_changes_with"].as_array().into_iter().flatten().collect();
    if !partners.is_empty() {
        o += "  usually changes with:\n";
        for p in partners.iter().take(5) {
            o += &format!("    {}  {} of {} commits\n", s(&p["path"]), p["together"], p["of"]);
        }
    }
    o += &signals_line(&v["signals"]).lines().map(|l| format!("  {l}\n")).collect::<String>();
    o
}

/// "3 days ago" for a unix time.
fn days_ago(t: i64) -> String {
    let d = (ripple_coverage::now() - t).max(0) / 86_400;
    match d {
        0 => "today".to_string(),
        1 => "1 day ago".to_string(),
        n => format!("{n} days ago"),
    }
}

/// `ripple rank`.
pub fn rank(v: &Value) -> String {
    let mut o = String::new();
    let rows: Vec<&Value> = v["rows"].as_array().into_iter().flatten().collect();
    let by = s(&v["by"]);
    if v["ready"] == Value::Bool(false) {
        o += &format!("{}\n", s(&v["not_ready"]));
        o += &signals_line(&v["signals"]);
        return o;
    }
    o += &format!(
        "{} of {} ranked files{}; hotspots: top {}% (rank <= {})\n",
        rows.len(),
        v["files"],
        if by == "score" { String::new() } else { format!(", by {by}") },
        v["hotspot_percent"].as_f64().map(ripple_policy::fmt_pct).unwrap_or_default(),
        v["hotspot_cutoff"]
    );
    for r in rows {
        let hot = if r["hotspot"] == Value::Bool(true) { "*" } else { " " };
        o += &format!("{:>4}{hot} {:>5.1}  {}\n       {}\n", r["rank"], r["score"].as_f64().unwrap_or(0.0), s(&r["path"]), s(&r["reason"]));
    }
    o += &signals_line(&v["signals"]);
    o
}
