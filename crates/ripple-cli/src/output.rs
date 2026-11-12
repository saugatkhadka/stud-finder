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
