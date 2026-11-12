//! `ripple doctor` (alias `status`): is everything ready, and if not, why
//! and what to run. Never starts a daemon and never indexes: it reports
//! on what is there.

use crate::client::Client;
use crate::paths::DaemonPaths;
use anyhow::Result;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::path::Path;

/// Collects the state. `fixes` lists what to do about each problem found
/// (exit 1); `suggestions` what would make ripple more useful (exit 0).
pub fn collect(root: &Path) -> Value {
    let mut fixes: Vec<String> = Vec::new();
    let mut suggestions: Vec<String> = Vec::new();
    // Config.
    let config = match ripple_policy::Policy::load(root) {
        Ok(p) => {
            let builtin_areas = p.areas().iter().filter(|a| a.builtin).count();
            let builtin_rules = p.rules_with_origin().filter(|(_, b)| *b).count();
            json!({
                "ok": true,
                "source": p.source,
                "areas": p.areas().len(),
                "builtin_areas": builtin_areas,
                "rules": p.rules().count(),
                "builtin_rules": builtin_rules,
                "codeowners": p.codeowners().map(|c| c.path.clone()),
            })
        }
        Err(e) => {
            fixes.push("fix ripple.yml (`ripple config check` shows each problem with its line)".into());
            json!({"ok": false, "error": e.to_string()})
        }
    };
    // Daemon, index, signals.
    let paths = DaemonPaths::for_root(root).ok();
    let mut client = paths.as_ref().and_then(Client::connect);
    let (daemon, index, signals) = match client.as_mut() {
        None => {
            suggestions.push("the daemon is not running: `ripple daemon start` (any query also starts it)".into());
            (json!({"running": false, "socket": paths.as_ref().map(|p| p.socket.clone())}), Value::Null, Value::Null)
        }
        Some(c) => {
            let stats: Value = c.call("stats", &Value::Null).ok().and_then(|r| r.result.ok()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
            let d = &stats["daemon"];
            let version = d["version"].as_str().unwrap_or("");
            if !version.is_empty() && version != env!("CARGO_PKG_VERSION") {
                fixes.push(format!("the daemon runs ripple {version}, this is {}: `ripple daemon stop` (the next query starts a new one)", env!("CARGO_PKG_VERSION")));
            }
            let built = stats["built_at_unix_ms"].as_u64().unwrap_or(0);
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
            let pending = d["pending_changes"].as_u64().unwrap_or(0);
            let index = json!({
                "files": stats["files"],
                "nodes": stats["nodes"],
                "edges": stats["edges"],
                "generation": stats["generation"],
                "age_ms": now.saturating_sub(built),
                "pending_changes": pending,
                "indexing": d["indexing"],
                "fresh": pending == 0 && d["indexing"] != Value::Bool(true),
            });
            let rank: Value = c
                .call("rank", &json!({"limit": 0, "no_wait": true}))
                .ok()
                .and_then(|r| r.result.ok())
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or(Value::Null);
            let signals = rank["signals"].clone();
            if let Some(m) = signals.as_object() {
                for (name, s) in m {
                    match s["state"].as_str() {
                        Some("unavailable") if name == "coverage" => suggestions.push(format!(
                            "coverage: {} (run your test suite with SimpleCov or Jest/Vitest coverage, or set coverage.paths)",
                            s["reason"].as_str().unwrap_or("no report")
                        )),
                        Some("failed") => fixes.push(format!("{name}: {}", s["reason"].as_str().unwrap_or("failed"))),
                        Some("unavailable") => suggestions.push(format!("{name}: {}", s["reason"].as_str().unwrap_or("unavailable"))),
                        _ => {}
                    }
                }
            }
            (json!({"running": true, "pid": d["pid"], "version": d["version"], "uptime_ms": d["uptime_ms"], "socket": d["socket"]}), index, signals)
        }
    };
    // Skill.
    let skills: Vec<Value> = crate::skill::summary(root).into_iter().map(|(path, st)| json!({"path": path, "state": st.word()})).collect();
    let any_skill = crate::skill::summary(root).iter().any(|(_, s)| *s != crate::skill::State::Missing);
    if !any_skill {
        suggestions.push("agents: `ripple skill install` (project) or `ripple skill install --user`".into());
    } else if crate::skill::summary(root).iter().any(|(_, s)| matches!(s, crate::skill::State::Outdated(_))) {
        fixes.push("the installed skill is from another ripple version: `ripple skill update`".into());
    }
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "root": root,
        "config": config,
        "daemon": daemon,
        "index": index,
        "signals": signals,
        "skill": skills,
        "fixes": fixes,
        "suggestions": suggestions,
    })
}

pub fn text(v: &Value) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "ripple {}  {}", v["version"].as_str().unwrap_or(""), v["root"].as_str().unwrap_or(""));
    let c = &v["config"];
    if c["ok"] == Value::Bool(true) {
        let src = c["source"].as_str().map(str::to_string).unwrap_or_else(|| "no ripple.yml (built-in defaults)".into());
        let _ = writeln!(
            s,
            "config   ok: {src}; {} areas ({} built-in), {} rules ({} built-in){}",
            c["areas"],
            c["builtin_areas"],
            c["rules"],
            c["builtin_rules"],
            c["codeowners"].as_str().map(|p| format!("; CODEOWNERS {p}")).unwrap_or_default()
        );
    } else {
        let _ = writeln!(s, "config   INVALID: {}", c["error"].as_str().unwrap_or(""));
    }
    let d = &v["daemon"];
    if d["running"] == Value::Bool(true) {
        let _ = writeln!(s, "daemon   running (pid {}, ripple {}, up {} s)", d["pid"], d["version"].as_str().unwrap_or("?"), d["uptime_ms"].as_u64().unwrap_or(0) / 1000);
        let i = &v["index"];
        let fresh = if i["fresh"] == Value::Bool(true) { "fresh".to_string() } else { format!("{} changes pending", i["pending_changes"]) };
        let _ = writeln!(s, "index    {} files, {} edges, built {} s ago, {fresh}", i["files"], i["edges"], i["age_ms"].as_u64().unwrap_or(0) / 1000);
    } else {
        let _ = writeln!(s, "daemon   not running");
    }
    if let Some(m) = v["signals"].as_object() {
        for (name, st) in m {
            let state = st["state"].as_str().unwrap_or("?");
            let why = st["reason"].as_str().map(|r| format!(" ({r})")).unwrap_or_default();
            let _ = writeln!(s, "signal   {name:<10} {state}{why}");
        }
    }
    for k in v["skill"].as_array().into_iter().flatten() {
        let _ = writeln!(s, "skill    {}: {}", k["path"].as_str().unwrap_or(""), k["state"].as_str().unwrap_or(""));
    }
    let list = |k: &str| -> Vec<String> { v[k].as_array().map(|a| a.iter().filter_map(Value::as_str).map(|f| format!("  - {f}")).collect()).unwrap_or_default() };
    let (fixes, sugg) = (list("fixes"), list("suggestions"));
    if fixes.is_empty() {
        let _ = write!(s, "nothing to fix");
    } else {
        let _ = write!(s, "to fix:\n{}", fixes.join("\n"));
    }
    if !sugg.is_empty() {
        let _ = write!(s, "\noptional:\n{}", sugg.join("\n"));
    }
    s
}

/// Exit code: 0 nothing to fix, 1 something to fix, 2 invalid config.
pub fn exit_code(v: &Value) -> u8 {
    if v["config"]["ok"] == Value::Bool(false) {
        2
    } else if v["fixes"].as_array().is_some_and(|a| !a.is_empty()) {
        1
    } else {
        0
    }
}

pub fn run(root: &Path, json_out: bool) -> Result<u8> {
    let v = collect(root);
    if json_out {
        println!("{v}");
    } else {
        println!("{}", text(&v));
    }
    Ok(exit_code(&v))
}
