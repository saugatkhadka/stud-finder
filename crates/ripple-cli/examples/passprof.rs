//! Per-phase timings of incremental passes for typical edits (profiling aid).
//!
//! ```text
//! cargo run --release -p ripple-cli --example passprof -- REPO MODEL CONST JSFILE [REPS] [SCENARIOS]
//! ```
//!
//! Scenarios (comma-separated, default all): `body` (a statement inside the
//! first method of MODEL), `method` (a new method in MODEL), `constref` (a
//! reference to CONST inside the first method of MODEL), `js` (a statement
//! appended to JSFILE), `batch20`, `batch200` (a comment appended to N
//! seeded-random Ruby files under app/ and lib/), `mixed20`, `mixed200` (the
//! same, every fourth file also gets a reference to CONST). Each edit is
//! applied, timed, then restored (the restore pass is not counted). The
//! repository is modified while this runs and restored afterwards.

use ripple_cli::engine::{Engine, Pipeline};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Instant;

fn insert_after_first(src: &str, pred: impl Fn(&str) -> bool, line: impl Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(src.len() + 64);
    let mut done = false;
    for l in src.split_inclusive('\n') {
        out.push_str(l);
        if !done && pred(l) {
            let indent: String = l.chars().take_while(|c| c.is_whitespace() && *c != '\n').collect();
            out.push_str(&line(&indent));
            out.push('\n');
            done = true;
        }
    }
    assert!(done, "no matching line");
    out
}

fn is_def(l: &str) -> bool {
    let t = l.trim_start();
    t.starts_with("def ") && !l.trim_end().ends_with(" end") && l.len() != t.len()
}

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let root = Path::new(&a[1]).canonicalize().unwrap();
    let (model, konst, js) = (a[2].clone(), a[3].clone(), a[4].clone());
    let reps: usize = a.get(5).and_then(|s| s.parse().ok()).unwrap_or(5);
    let all = "body,method,constref,js,batch20,batch200,mixed20,mixed200";
    let scenarios: Vec<String> = a.get(6).map_or(all, |s| s.as_str()).split(',').map(String::from).collect();

    let t = Instant::now();
    let (mut eng, snap) = Engine::open(&root, Pipeline::standard()).unwrap();
    println!("cold open: {} ms ({} nodes, {} edges)", t.elapsed().as_millis(), snap.graph.node_count(), snap.graph.edge_count());
    let mut rb: Vec<String> = snap.files.iter().filter(|f| f.ends_with(".rb") && (f.starts_with("app/") || f.starts_with("lib/")) && !f.contains("routes")).cloned().collect();
    rb.sort();
    drop(snap);

    let mut rows: BTreeMap<String, Vec<BTreeMap<String, u64>>> = BTreeMap::new();
    for rep in 0..reps {
        for sc in &scenarios {
            let mut edits: Vec<(String, String)> = Vec::new();
            let read = |f: &str| std::fs::read_to_string(root.join(f)).unwrap();
            match sc.as_str() {
                "body" => edits.push((model.clone(), insert_after_first(&read(&model), is_def, |i| format!("{i}  _ripple_prof = {rep}")))),
                "method" => edits.push((model.clone(), insert_after_first(&read(&model), |l| l.starts_with("class ") || l.starts_with("module "), |_| format!("  def ripple_prof_m{rep}\n    1\n  end")))),
                "constref" => edits.push((model.clone(), insert_after_first(&read(&model), is_def, |i| format!("{i}  ::{konst}")))),
                "js" => edits.push((js.clone(), format!("{}\nconst __rippleProf{rep} = {rep};\n", read(&js)))),
                s if s.starts_with("batch") || s.starts_with("mixed") => {
                    let n: usize = s[5..].parse().unwrap();
                    let mut rng = Rng(0x9e3779b97f4a7c15 ^ (rep as u64 + 1) * 7919);
                    let mut picked = BTreeSet::new();
                    while picked.len() < n.min(rb.len()) {
                        picked.insert(rb[rng.below(rb.len())].clone());
                    }
                    for (k, f) in picked.into_iter().enumerate() {
                        let extra = if s.starts_with("mixed") && k % 4 == 0 { format!("\n::{konst}\n") } else { String::new() };
                        let src = read(&f);
                        edits.push((f, format!("{src}\n# ripple prof {rep}\n{extra}")));
                    }
                }
                other => panic!("unknown scenario {other}"),
            }
            let originals: Vec<(String, String)> = edits.iter().map(|(f, _)| (f.clone(), read(f))).collect();
            let paths: BTreeSet<String> = edits.iter().map(|(f, _)| f.clone()).collect();
            for (f, s) in &edits {
                std::fs::write(root.join(f), s).unwrap();
            }
            let t = Instant::now();
            let snap = eng.apply_paths(&paths).unwrap();
            let wall = t.elapsed().as_micros() as u64;
            let mut row: BTreeMap<String, u64> = snap.timings.clone();
            row.insert("wall_us".into(), wall);
            for ex in ["ruby", "js", "boundary"] {
                for (k, v) in snap.stats.get(ex).into_iter().flatten() {
                    if k.contains("_time_") || k.starts_with("js_us_") || k.starts_with("ruby_fast") || k.starts_with("ruby_slow") || k == "ruby_update_fell_back_to_build" || k == "ruby_shadow_docs_reindexed" {
                        row.insert(k.clone(), *v);
                    }
                }
            }
            drop(snap);
            rows.entry(sc.clone()).or_default().push(row);
            for (f, s) in &originals {
                std::fs::write(root.join(f), s).unwrap();
            }
            eng.apply_paths(&paths).unwrap();
        }
    }
    for sc in &scenarios {
        let rs = &rows[sc];
        let keys: BTreeSet<&String> = rs.iter().flat_map(|r| r.keys()).collect();
        println!("== {sc} (median of {} reps, ms)", rs.len());
        for k in keys {
            let mut v: Vec<u64> = rs.iter().filter_map(|r| r.get(k).copied()).collect();
            v.sort_unstable();
            let med = v[v.len() / 2];
            let max = v[v.len() - 1];
            if k.contains("_us") || k == "wall_us" {
                println!("  {k:<40} {:>9.1} (max {:.1})", med as f64 / 1000.0, max as f64 / 1000.0);
            } else {
                println!("  {k:<40} {med:>9} (max {max})");
            }
        }
    }
}
