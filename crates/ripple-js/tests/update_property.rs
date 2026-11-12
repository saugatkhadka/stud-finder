//! `update` must return exactly what a fresh `build` returns.
//!
//! Ignored by default (needs a real repo). Run with:
//!
//! ```text
//! RIPPLE_JS_REPO=/path/to/mastodon [RIPPLE_JS_SEED=1] [RIPPLE_JS_OPS=60] \
//!   cargo test -p ripple-js --release --test update_property -- --ignored --nocapture
//! ```
//!
//! The repo is copied (tracked files only) into a temp dir and mutated there;
//! the original is never touched. After every random operation the incremental
//! result is compared with a from-scratch build: sorted edges, sorted call
//! sites and sorted warnings must all be identical.

mod common;
use globset::{Glob, GlobSetBuilder};
use ripple_core::{ChangeSet, Config, ExtractOutput, Extractor, Workspace};
use ripple_js::JsExtractor;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

struct Rng(u64);
impl Rng {
    /// splitmix64: tiny, seedable, good enough for picking operations.
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len())]
    }
}

fn is_code(f: &str) -> bool {
    ["js", "jsx", "ts", "tsx", "mjs", "cjs", "mts", "cts", "vue"].contains(&f.rsplit('.').next().unwrap_or(""))
}

fn config() -> Config {
    let mut cfg = Config::default();
    cfg.boundary.http_callees = ["api().get", "api().post", "apiRequest", "apiRequestGet", "apiRequestPost"].map(String::from).to_vec();
    cfg
}

fn copy_repo(src: &Path, dst: &Path) -> Vec<String> {
    let out = Command::new("git").args(["ls-files", "-z"]).current_dir(src).output().expect("git");
    assert!(out.status.success());
    let mut ignore = GlobSetBuilder::new();
    for g in &Config::default().ignore {
        ignore.add(Glob::new(g).unwrap());
    }
    let ignore = ignore.build().unwrap();
    let mut files = Vec::new();
    for f in out.stdout.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        let f = String::from_utf8_lossy(f).into_owned();
        if ignore.is_match(&f) || !src.join(&f).is_file() {
            continue;
        }
        std::fs::create_dir_all(dst.join(&f).parent().unwrap()).unwrap();
        std::fs::copy(src.join(&f), dst.join(&f)).unwrap();
        files.push(f);
    }
    files.sort();
    files
}

fn normalise(out: &ExtractOutput) -> (Vec<String>, Vec<String>, Vec<String>) {
    (common::edges(out), common::calls(out), common::warnings(out))
}

fn counters(out: &ExtractOutput) -> Vec<(String, u64)> {
    // Everything except timings and the per-run parse count must match too.
    out.stats.iter().filter(|(k, _)| !k.starts_with("js_us_") && *k != "js_files_parsed").map(|(k, v)| (k.clone(), *v)).collect()
}

#[test]
#[ignore = "needs RIPPLE_JS_REPO pointing at a real repo (e.g. mastodon)"]
fn update_equals_build() {
    let repo = std::env::var("RIPPLE_JS_REPO").expect("set RIPPLE_JS_REPO");
    let seed: u64 = std::env::var("RIPPLE_JS_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
    let ops: usize = std::env::var("RIPPLE_JS_OPS").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut files: BTreeSet<String> = copy_repo(Path::new(&repo), root).into_iter().collect();
    let mut rng = Rng(seed);
    let mut next_id = 0usize;

    let ws = |files: &BTreeSet<String>| Workspace { root: root.to_path_buf(), files: files.iter().cloned().collect(), config: config() };
    let mut ex = JsExtractor::new();
    let first = ex.build(&ws(&files)).unwrap();
    println!("initial: {} files, {} edges, {} call sites", files.len(), first.edges.len(), first.facts.js_call_sites.len());

    let mut tally: std::collections::BTreeMap<&str, usize> = Default::default();
    for step in 0..ops {
        let code: Vec<String> = files.iter().filter(|f| is_code(f)).cloned().collect();
        let mut change = ChangeSet::default();
        let op;
        match rng.below(11) {
            // Append an import of a random existing module (relative path computed from the importer).
            0 | 1 => {
                op = "add-import";
                let from = rng.pick(&code).clone();
                let to = rng.pick(&code).clone();
                let rel = relative(&from, &to);
                append(root, &from, &format!("\nimport _x{next_id} from \"{rel}\";\n"));
                change.modified.push(from);
            }
            // Drop the first line of a file (usually an import).
            2 => {
                op = "drop-first-line";
                let f = rng.pick(&code).clone();
                let text = std::fs::read_to_string(root.join(&f)).unwrap();
                let rest = text.split_once('\n').map_or("", |(_, r)| r);
                std::fs::write(root.join(&f), rest).unwrap();
                change.modified.push(f);
            }
            // Delete a file (importers' resolution changes).
            3 | 4 => {
                op = "delete-file";
                let f = rng.pick(&code).clone();
                std::fs::remove_file(root.join(&f)).unwrap();
                files.remove(&f);
                change.removed.push(f);
            }
            // Import something that does not exist yet, update, then create it
            // (previously-unresolved import starts resolving).
            5 => {
                op = "unresolved-then-created";
                let from = rng.pick(&code).clone();
                let name = format!("zz_new_{next_id}");
                let dir = from.rsplit_once('/').map_or("", |(d, _)| d).to_string();
                append(root, &from, &format!("\nimport _n{next_id} from \"./{name}\";\n"));
                ex.update(&ws(&files), &ChangeSet { modified: vec![from.clone()], removed: vec![] }).unwrap();
                let new = if dir.is_empty() { format!("{name}.ts") } else { format!("{dir}/{name}.ts") };
                std::fs::write(root.join(&new), "export const v = 1;\n").unwrap();
                files.insert(new.clone());
                change.modified.push(new);
            }
            // Add a sibling that shadows an existing resolution (`x.js` + new `x.ts`, or `x.ts` + `x/index.ts`).
            6 => {
                op = "shadowing-file";
                let target = rng.pick(&code).clone();
                let (stem, _) = target.rsplit_once('.').unwrap();
                let new = if target.ends_with(".ts") || target.ends_with(".tsx") { format!("{stem}/index.ts") } else { format!("{stem}.ts") };
                if files.contains(&new) {
                    continue;
                }
                std::fs::create_dir_all(root.join(&new).parent().unwrap()).unwrap();
                std::fs::write(root.join(&new), "export const shadow = 1;\n").unwrap();
                files.insert(new.clone());
                change.modified.push(new);
            }
            // Rename: old removed, new added; importers of the old name break.
            7 => {
                op = "rename-file";
                let f = rng.pick(&code).clone();
                let (stem, ext) = f.rsplit_once('.').unwrap();
                let new = format!("{stem}_renamed{next_id}.{ext}");
                std::fs::rename(root.join(&f), root.join(&new)).unwrap();
                files.remove(&f);
                files.insert(new.clone());
                change.removed.push(f);
                change.modified.push(new);
            }
            // Edit tsconfig paths / jsconfig baseUrl / package.json: invalidates resolution.
            8 => {
                op = "edit-config";
                let candidates: Vec<String> = files.iter().filter(|f| matches!(f.rsplit('/').next(), Some("tsconfig.json" | "jsconfig.json" | "package.json"))).cloned().collect();
                let f = rng.pick(&candidates).clone();
                let text = std::fs::read_to_string(root.join(&f)).unwrap();
                let edited = match f.rsplit('/').next().unwrap() {
                    "tsconfig.json" => {
                        // Re-point the `mastodon/*` alias (2k+ imports use it) somewhere else, or back.
                        const A: &str = "\"mastodon/*\": [\"app/javascript/mastodon/*\"]";
                        const B: &str = "\"mastodon/*\": [\"app/javascript/mastodon/components/*\"]";
                        if text.contains(A) { text.replace(A, B) } else { text.replace(B, A) }
                    }
                    "jsconfig.json" => {
                        if text.contains("\"baseUrl\": \"./app/javascript\"") {
                            text.replace("\"baseUrl\": \"./app/javascript\"", "\"baseUrl\": \"./app/javascript/mastodon\"")
                        } else {
                            text.replace("\"baseUrl\": \"./app/javascript/mastodon\"", "\"baseUrl\": \"./app/javascript\"")
                        }
                    }
                    _ => text.replacen('{', &format!("{{ \"main\": \"index{next_id}.js\","), 1),
                };
                std::fs::write(root.join(&f), edited).unwrap();
                change.modified.push(f);
            }
            // Touch without changing content.
            9 => {
                op = "touch";
                let f = rng.pick(&code).clone();
                change.modified.push(f);
            }
            // Add an HTTP call.
            _ => {
                op = "add-call";
                let f = rng.pick(&code).clone();
                append(root, &f, &format!("\nfetch(`/api/v1/probe/${{id}}/{next_id}`, {{ method: \"POST\" }});\napi().get(\"/api/v2/probe{next_id}\");\n"));
                change.modified.push(f);
            }
        }
        next_id += 1;
        *tally.entry(op).or_default() += 1;

        let incremental = ex.update(&ws(&files), &change).unwrap();
        let fresh = JsExtractor::new().build(&ws(&files)).unwrap();
        let (ie, ic, iw) = normalise(&incremental);
        let (fe, fc, fw) = normalise(&fresh);
        assert_eq!(ie, fe, "step {step} ({op}): edges differ");
        assert_eq!(ic, fc, "step {step} ({op}): call sites differ");
        assert_eq!(iw, fw, "step {step} ({op}): warnings differ");
        assert_eq!(counters(&incremental), counters(&fresh), "step {step} ({op}): stats differ");
        println!("step {step:3} {op:24} ok  edges={} calls={} warnings={}", fe.len(), fc.len(), fw.len());
    }
    println!("operations: {tally:?}");
}

fn append(root: &Path, file: &str, text: &str) {
    let p = root.join(file);
    let mut s = std::fs::read_to_string(&p).unwrap();
    s.push_str(text);
    std::fs::write(p, s).unwrap();
}

/// `./`-relative specifier from `from`'s directory to `to` (extension kept).
fn relative(from: &str, to: &str) -> String {
    let fdir: Vec<&str> = from.split('/').collect::<Vec<_>>().split_last().unwrap().1.to_vec();
    let tparts: Vec<&str> = to.split('/').collect();
    let common = fdir.iter().zip(&tparts).take_while(|(a, b)| a == b).count();
    let ups = fdir.len() - common;
    let mut out = if ups == 0 { "./".to_string() } else { "../".repeat(ups) };
    out.push_str(&tparts[common..].join("/"));
    out
}
