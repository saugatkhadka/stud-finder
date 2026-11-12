//! Inspect the boundary linker on a real repo.
//!
//! ```text
//! cargo run -p ripple-boundary --example dump -- <repo> [--routes|--mounts|--calls|--oracle|--timing|--warm-check]
//!     [--sample N] [--seed S] [--url-base B]... [--url-prefix P]...
//! ```
//!
//! With no mode it prints stats and a warning summary. Call sites come from
//! the DEV-ONLY regex scanner in `support/jsscan.rs`, not from the real JS
//! extractor. `--warm-check` EDITS FILES and refuses to run outside a
//! directory containing `work-boundary`.

#[path = "support/jsscan.rs"]
mod jsscan;
#[path = "support/oracle.rs"]
mod oracle;

use ripple_boundary::{BoundaryLinker, CallOutcome, Outcome, RouteKind};
use ripple_core::{ChangeSet, Config, ExtractOutput, Facts, Linker, Workspace};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

struct Opts {
    repo: PathBuf,
    mode: String,
    sample: usize,
    seed: u64,
    bases: Vec<String>,
    prefixes: Vec<String>,
}

fn parse_args() -> Result<Opts, String> {
    let mut args = std::env::args().skip(1);
    let mut o = Opts { repo: PathBuf::new(), mode: "stats".into(), sample: 0, seed: 1, bases: Vec::new(), prefixes: Vec::new() };
    while let Some(a) = args.next() {
        match a.as_str() {
            "--sample" => o.sample = args.next().and_then(|v| v.parse().ok()).ok_or("--sample N")?,
            "--seed" => o.seed = args.next().and_then(|v| v.parse().ok()).ok_or("--seed S")?,
            "--url-base" => o.bases.push(args.next().ok_or("--url-base B")?),
            "--url-prefix" => o.prefixes.push(args.next().ok_or("--url-prefix P")?),
            m if m.starts_with("--") => o.mode = m[2..].to_string(),
            r => o.repo = PathBuf::from(r),
        }
    }
    if o.repo.as_os_str().is_empty() {
        return Err("usage: dump <repo> [--routes|--mounts|--calls|--oracle|--timing|--warm-check]".into());
    }
    Ok(o)
}

fn workspace(o: &Opts) -> Result<Workspace, String> {
    let out = Command::new("git").arg("-C").arg(&o.repo).args(["ls-files", "-co", "--exclude-standard"]).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("git ls-files failed in {}", o.repo.display()));
    }
    let mut files: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect();
    files.sort();
    files.dedup();
    let mut config = Config::default();
    if !o.bases.is_empty() {
        config.boundary.url_bases = o.bases.clone();
    }
    config.boundary.url_prefixes = o.prefixes.clone();
    let root = std::fs::canonicalize(&o.repo).map_err(|e| e.to_string())?;
    Ok(Workspace { root, files, config })
}

fn facts(ws: &Workspace) -> Facts {
    let mut f = Facts::default();
    for p in ws.files.iter().filter(|p| jsscan::is_source(p)) {
        if let Ok(t) = std::fs::read_to_string(ws.abs(p)) {
            f.js_call_sites.extend(jsscan::scan(p, &t));
        }
    }
    f
}

/// Deterministic sample of `k` indices out of `n` (xorshift; same seed, same sample).
fn sample(n: usize, k: usize, seed: u64) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..n).collect();
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for i in (1..n).rev() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        idx.swap(i, (s % (i as u64 + 1)) as usize);
    }
    idx.truncate(k);
    idx.sort();
    idx
}

fn print_summary(out: &ExtractOutput) {
    for (k, v) in &out.stats {
        if !k.starts_with("routes.file.") {
            println!("{k:40} {v}");
        }
    }
    let mut by_code: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for w in &out.warnings {
        by_code.entry(&w.code).or_default().push(&w.message);
    }
    println!("\nwarnings: {}", out.warnings.len());
    for (code, msgs) in by_code {
        println!("  {code}: {}", msgs.len());
        for m in msgs.iter().take(6) {
            println!("      {m}");
        }
    }
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    for e in &out.edges {
        *kinds.entry(format!("{:?}/{:?}", e.kind, e.confidence)).or_default() += 1;
    }
    println!("\nedges: {}", out.edges.len());
    for (k, n) in kinds {
        println!("  {k:30} {n}");
    }
}

fn run_oracle(ws: &Workspace, linker: &BoundaryLinker) {
    use oracle::{Check, Expect};
    let mut cases = Vec::new();
    for p in ws.files.iter().filter(|p| oracle::is_spec(p)) {
        if let Ok(t) = std::fs::read_to_string(ws.abs(p)) {
            cases.extend(oracle::scan(p, &t));
        }
    }
    cases.sort_by(|a, b| a.check.cmp(&b.check));
    cases.dedup_by(|a, b| a.check == b.check);
    let names: std::collections::HashSet<&str> = linker.routes().iter().filter_map(|r| r.name.as_deref()).collect();
    let mut tally: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for c in &cases {
        let (kind, ok, got) = match &c.check {
            Check::Path { verb, path, expect } => {
                let hits = linker.match_path(Some(verb), path);
                let first = hits.first().map(|r| r.endpoint());
                let ok = match expect {
                    Expect::Routable => !hits.is_empty(),
                    Expect::NotRoutable => hits.iter().all(|r| ripple_boundary::matching::is_catch_all(r)),
                    // With `:param`s the concrete value decides between the
                    // candidates (constraints), so any candidate counts.
                    Expect::Endpoint(e) if path.contains(":param") => hits.iter().any(|r| r.endpoint() == *e),
                    Expect::Endpoint(e) => first.as_deref() == Some(e.as_str()),
                };
                (if matches!(expect, Expect::Routable) { "path" } else { "routing" }, ok, format!("{first:?}"))
            }
            // ActionController::TestCase finds the route by generation,
            // which ignores the verb: only controller#action must exist.
            Check::Action { controller, action, .. } => {
                let ok = linker.routes().iter().any(|r| r.controller == *controller && r.action == *action);
                ("controller_action", ok, String::new())
            }
            Check::Helper { name } => ("helper", names.contains(name.as_str()), String::new()),
        };
        let e = tally.entry(kind).or_default();
        if ok {
            e.0 += 1;
        } else {
            e.1 += 1;
            println!("MISS {kind} {:?} got={got}  ({}:{})", c.check, c.file, c.line);
        }
    }
    let (mut pass, mut fail) = (0, 0);
    for (kind, (p, f)) in &tally {
        println!("oracle {kind:18} {p}/{} pass ({:.1}%)", p + f, 100.0 * *p as f64 / (*p + *f).max(1) as f64);
        pass += p;
        fail += f;
    }
    println!("oracle {:18} {pass}/{} pass ({:.1}%)", "TOTAL", pass + fail, 100.0 * pass as f64 / (pass + fail).max(1) as f64);
}

fn median_ms(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn run_timing(ws: &Workspace, f: &Facts) -> anyhow::Result<()> {
    let mut full = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        BoundaryLinker::new().link(ws, f, None)?;
        full.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let mut linker = BoundaryLinker::new();
    linker.link(ws, f, None)?;
    let routes_file = ws.files.iter().find(|p| p.as_str() == "config/routes.rb").cloned().unwrap_or_default();
    let mut warm_routes = Vec::new();
    let mut warm_view = Vec::new();
    let view = linker.mounts().first().map(|m| m.file.clone()).unwrap_or_default();
    for _ in 0..5 {
        let c = ChangeSet { modified: vec![routes_file.clone()], removed: vec![] };
        let t = Instant::now();
        linker.link(ws, f, Some(&c))?;
        warm_routes.push(t.elapsed().as_secs_f64() * 1e3);
        let c = ChangeSet { modified: vec![view.clone()], removed: vec![] };
        let t = Instant::now();
        linker.link(ws, f, Some(&c))?;
        warm_view.push(t.elapsed().as_secs_f64() * 1e3);
    }
    println!("files={} call_sites={}", ws.files.len(), f.js_call_sites.len());
    println!("full link (median of 5):          {:.1} ms", median_ms(full));
    println!("warm link, routes edit (median):  {:.1} ms", median_ms(warm_routes));
    println!("warm link, view edit {view} (median): {:.1} ms", median_ms(warm_view));
    Ok(())
}

fn fingerprint(out: &ExtractOutput) -> String {
    format!("{:?}\n{:?}\n{:?}", out.edges, out.warnings, out.stats)
}

fn edit(ws: &Workspace, rel: &str, f: impl FnOnce(String) -> String) -> anyhow::Result<()> {
    let p = ws.abs(rel);
    let text = std::fs::read_to_string(&p)?;
    std::fs::write(&p, f(text))?;
    Ok(())
}

/// Edits a throwaway copy (routes file, a view, a controller, then deletes a
/// controller) and checks each warm link against a fresh full link.
fn run_warm_check(o: &Opts) -> anyhow::Result<()> {
    if !o.repo.to_string_lossy().contains("work-boundary") {
        anyhow::bail!("--warm-check edits files; run it on a copy under work-boundary/");
    }
    let mut ws = workspace(o).map_err(anyhow::Error::msg)?;
    let f = facts(&ws);
    let mut linker = BoundaryLinker::new();
    let mut prev = fingerprint(&linker.link(&ws, &f, None)?);
    let view = linker.mounts().iter().find(|m| !m.file.ends_with(".rb")).map(|m| m.file.clone()).unwrap_or_default();
    let controller = ws.files.iter().find(|p| p.starts_with("app/controllers/") && p.ends_with("_controller.rb") && !p.ends_with("application_controller.rb")).cloned().unwrap_or_default();
    let routed = linker.routes().iter().find(|r| r.kind == RouteKind::Action && ws.files.iter().any(|p| p.ends_with(&format!("app/controllers/{}_controller.rb", r.controller)))).map(|r| format!("app/controllers/{}_controller.rb", r.controller)).unwrap_or_default();
    let steps: Vec<(&str, String)> = vec![("routes", "config/routes.rb".into()), ("view", view), ("controller", controller), ("delete-controller", routed)];
    let mut all_ok = true;
    for (step, rel) in steps {
        if rel.is_empty() {
            println!("{step}: skipped (no file)");
            continue;
        }
        let mut changes = ChangeSet::default();
        match step {
            "routes" => edit(&ws, &rel, |t| t.replacen(".routes.draw do\n", ".routes.draw do\n  get '/ripple_probe', to: 'ripple_probe#show'\n  resources :ripple_probes, only: [:index]\n", 1))?,
            "view" => edit(&ws, &rel, |t| {
                let line = if rel.ends_with(".haml") { "\n= react_component 'RippleProbe'\n" } else { "\n<%= react_component 'RippleProbe' %>\n" };
                t + line
            })?,
            "controller" => edit(&ws, &rel, |t| t + "\nclass RippleProbeController < ApplicationController\n  def show\n    render inertia: \"RippleProbe\"\n  end\nend\n")?,
            _ => {
                std::fs::remove_file(ws.abs(&rel))?;
                ws.files.retain(|p| *p != rel);
            }
        }
        if step == "delete-controller" { changes.removed.push(rel.clone()) } else { changes.modified.push(rel.clone()) }
        let t = Instant::now();
        let warm = fingerprint(&linker.link(&ws, &f, Some(&changes))?);
        let warm_ms = t.elapsed().as_secs_f64() * 1e3;
        let full = fingerprint(&BoundaryLinker::new().link(&ws, &f, None)?);
        let ok = warm == full;
        all_ok &= ok;
        println!("{step:18} {rel}: warm==full {ok}  changed_vs_before {}  warm {warm_ms:.1} ms", warm != prev);
        prev = warm;
    }
    println!("warm-check: {}", if all_ok { "PASS" } else { "FAIL" });
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let o = parse_args().map_err(anyhow::Error::msg)?;
    if o.mode == "warm-check" {
        return run_warm_check(&o);
    }
    let ws = workspace(&o).map_err(anyhow::Error::msg)?;
    let f = facts(&ws);
    if o.mode == "timing" {
        return run_timing(&ws, &f);
    }
    let mut linker = BoundaryLinker::new();
    let t = Instant::now();
    let out = linker.link(&ws, &f, None)?;
    let ms = t.elapsed().as_secs_f64() * 1e3;
    match o.mode.as_str() {
        "routes" => {
            for r in linker.routes() {
                println!("{:7} {:60} {:45} {:35} {}:{}", r.verb, r.path, r.endpoint(), r.name.as_deref().unwrap_or(""), r.file, r.line);
            }
            let t = linker.route_table();
            println!("\nroutes: {}  uninterpreted: {:?}  unreached: {:?}", t.routes.len(), t.uninterpreted, t.unreached);
        }
        "mounts" => {
            let recs = linker.mounts();
            let picked: Vec<usize> = if o.sample > 0 {
                let resolved: Vec<usize> = (0..recs.len()).filter(|&i| matches!(recs[i].outcome, Outcome::Resolved(..))).collect();
                sample(resolved.len(), o.sample, o.seed).into_iter().map(|i| resolved[i]).collect()
            } else {
                (0..recs.len()).collect()
            };
            for i in picked {
                let m = &recs[i];
                println!("{}:{} {:?} {} {:?} -> {:?}", m.file, m.mount.line, m.mount.kind, m.mount.helper, m.mount.name, m.outcome);
            }
        }
        "calls" => {
            let calls = linker.call_matches();
            let picked: Vec<usize> = if o.sample > 0 {
                let matched: Vec<usize> = (0..calls.len()).filter(|&i| matches!(calls[i].outcome, CallOutcome::Matched(_))).collect();
                sample(matched.len(), o.sample, o.seed).into_iter().map(|i| matched[i]).collect()
            } else {
                (0..calls.len()).collect()
            };
            for i in picked {
                let c = &calls[i];
                let s = &f.js_call_sites[c.site];
                let targets: Vec<String> = c.routes.iter().map(|&r| {
                    let r = &linker.routes()[r];
                    format!("{} {} → {}", r.verb, r.path, r.endpoint())
                }).collect();
                println!("{}:{} {} {} [{}] {:?} {:?}", s.file, s.line, s.method.as_deref().unwrap_or("ANY"), s.url, c.url, c.outcome, targets);
            }
        }
        "oracle" => run_oracle(&ws, &linker),
        _ => {
            println!("repo {} files={} call_sites={} full link {ms:.1} ms\n", Path::new(&o.repo).display(), ws.files.len(), f.js_call_sites.len());
            print_summary(&out);
        }
    }
    Ok(())
}
