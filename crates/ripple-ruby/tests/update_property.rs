//! Property: after any sequence of edits, `update` equals a fresh `build`.
//!
//! Runs on a real repo (mastodon is the intended one), which is copied into
//! `CARGO_TARGET_TMPDIR` first, so the source repo is never modified:
//!
//! ```text
//! RIPPLE_RUBY_REPO=/path/to/mastodon [RIPPLE_RUBY_SEED=7] [RIPPLE_RUBY_OPS=40] \
//!   cargo test --release -p ripple-ruby --test update_property -- --ignored --nocapture
//! ```
//!
//! Operations (seeded): add a constant reference, remove a line that mentions
//! a constant, delete a file, add a new file defining a new constant that an
//! existing file references, rename a class, restore a deleted file, edit a
//! controller (a reference inside an action, a `private` before an action,
//! one action calling another: these move edges between action scopes), and
//! batch several of these into one `ChangeSet`. Method-level operations: add a
//! new method then later its first mention elsewhere (unmentioned -> scoped),
//! add more definers of an existing method name (crossing the by-name LIMIT),
//! and remove a whole `def`. Edges compare with their scopes and `inherit`
//! flag, and `Facts.scopes` must match too.
//!
//! Fast-path operations (the file keeps its fingerprint): a statement or a
//! comment inside a method body, a template edit, a new method. Every tenth
//! step is a batch of 20 operations (mostly body edits), and with
//! `RIPPLE_RUBY_BIG=N` step 5 is a batch of N. Updates alternate (seeded)
//! between `update` (all edges) and `update_rows` (only the changed files'
//! edges, spliced into the previous edge set here as the engine does).
//! Compared with a fresh build: edges, warnings, scopes, the mention table,
//! the metrics table, HTTP requests and every stat that is not a timing or
//! a per-run counter.

#[path = "../examples/support/mod.rs"]
mod support;

use ripple_core::{ChangeSet, Edge, ExtractOutput, Extractor};
use std::collections::BTreeSet;
use ripple_ruby::RubyExtractor;
use std::path::{Path, PathBuf};

const EXTS: &[&str] = &[
    ".rb",
    ".rake",
    ".ru",
    ".gemspec",
    ".erb",
    ".haml",
    ".slim",
    ".jbuilder",
    ".builder",
];

fn wanted(f: &str) -> bool {
    f.ends_with("Rakefile") || EXTS.iter().any(|e| f.ends_with(e))
}

fn copy_repo(src: &Path, dst: &Path) -> std::io::Result<()> {
    if dst.exists() {
        std::fs::remove_dir_all(dst)?;
    }
    for f in support::workspace(src)
        .files
        .into_iter()
        .filter(|f| wanted(f))
    {
        let to = dst.join(&f);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src.join(&f), to)?;
    }
    Ok(())
}

fn fmt_edge(e: &Edge) -> String {
    format!(
        "{} -> {} {:?} {:?} :{} @{:?}/{:?}{} {}",
        e.from,
        e.to,
        e.kind,
        e.confidence,
        e.line,
        e.from_scope,
        e.to_scope,
        if e.inherit { " inherit" } else { "" },
        e.detail.as_deref().unwrap_or("")
    )
}

/// Stats that legitimately differ between an update and a build.
fn per_run(k: &str) -> bool {
    k.contains("_time_") || k.ends_with("_this_run") || k == "ruby_shadow_docs_reindexed" || k == "ruby_update_fell_back_to_build"
}

#[derive(PartialEq)]
struct Snap {
    edges: Vec<String>,
    warnings: Vec<String>,
    scopes: Vec<String>,
    stats: Vec<String>,
    mentions: String,
    metrics: String,
    requests: String,
}

/// `edges`: the complete edge set (`out.edges`, or the spliced rows).
fn snap(out: &ExtractOutput, edges: &[Edge]) -> Snap {
    let mut e: Vec<String> = edges.iter().map(fmt_edge).collect();
    e.sort();
    let mut warnings: Vec<String> = out
        .warnings
        .iter()
        .map(|w| format!("{} {:?} {}", w.code, w.file, w.message))
        .collect();
    warnings.sort();
    let scopes: Vec<String> = out
        .facts
        .scopes
        .iter()
        .map(|(f, v)| {
            let r: Vec<String> = v.iter().map(|r| format!("{}:{}-{}", r.name, r.start, r.end)).collect();
            format!("{f} {}", r.join(" "))
        })
        .collect();
    let stats = out.stats.iter().filter(|(k, _)| !per_run(k)).map(|(k, v)| format!("{k}={v}")).collect();
    let mentions = match &out.facts.mentions {
        Some(m) => {
            let mut files: Vec<String> = m.files.iter().map(|(p, f)| format!("{p} {f:?}")).collect();
            files.sort();
            format!("{files:?} {:?}", m.unmentioned)
        }
        None => "none".into(),
    };
    Snap {
        edges: e,
        warnings,
        scopes,
        stats,
        mentions,
        metrics: format!("{:?}", out.facts.metrics),
        requests: format!("{:?}", out.facts.http_requests),
    }
}

/// Splices `out` into the complete edge set `cur` (see `ExtractOutput::replaced`).
fn splice(cur: &mut Vec<Edge>, out: &ExtractOutput) {
    match &out.replaced {
        None => *cur = out.edges.clone(),
        Some(files) => {
            let files: BTreeSet<&str> = files.iter().map(String::as_str).collect();
            cur.retain(|e| !files.contains(e.from.as_str()));
            cur.extend(out.edges.iter().cloned());
        }
    }
}

fn report(out: &ExtractOutput, totals: &mut (u64, u64, u64)) {
    let get = |k: &str| out.stats.get(k).copied().unwrap_or(0);
    totals.0 += get("ruby_fast_files_this_run");
    totals.1 += get("ruby_slow_files_this_run");
    totals.2 += get("ruby_slow_batch_rebuilt_this_run");
    if out.stats.contains_key("ruby_update_fell_back_to_build") {
        println!("  (update fell back to a full build)");
    }
    if get("ruby_slow_batch_rebuilt_this_run") > 0 {
        println!("  (batch rebuilt from scratch)");
    }
    if let Some(n) = out.stats.get("ruby_shadow_docs_reindexed").filter(|n| **n > 0) {
        println!("  (shadowing repair re-indexed {n} documents)");
    }
}

fn diff(label: &str, a: &[String], b: &[String], n: usize) {
    let (a, b): (BTreeSet<&String>, BTreeSet<&String>) = (a.iter().collect(), b.iter().collect());
    for l in a.difference(&b).take(n) {
        eprintln!("  {label} missing in update: {l}");
    }
    for l in b.difference(&a).take(n) {
        eprintln!("  {label} extra in update:   {l}");
    }
}

/// `def` lines of `lines` as (index, name, indent, end index).
fn defs(lines: &[&str]) -> Vec<(usize, String, usize, usize)> {
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        if !t.starts_with("def ") || t.starts_with("def self.") {
            continue;
        }
        let indent = l.len() - t.len();
        let name: String = t[4..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '?' | '!'))
            .collect();
        if name.is_empty() {
            continue;
        }
        let end = if t.ends_with(" end") || t.ends_with(";end") {
            Some(i)
        } else {
            let close = format!("{}end", " ".repeat(indent));
            (i + 1..lines.len()).find(|j| lines[*j].trim_end() == close)
        };
        if let Some(end) = end {
            out.push((i, name, indent, end));
        }
    }
    out
}

struct Sim {
    root: PathBuf,
    rng: support::Rng,
    deleted: Vec<(String, String)>,
    added: usize,
    /// Methods added by op 9 that nothing mentions yet.
    unmentioned: Vec<String>,
}

impl Sim {
    fn files(&self) -> Vec<String> {
        support::workspace(&self.root).files
    }
    fn rb_files(&self) -> Vec<String> {
        self.files()
            .into_iter()
            .filter(|f| f.ends_with(".rb"))
            .collect()
    }
    fn pick(&mut self, v: &[String]) -> String {
        v[self.rng.below(v.len())].clone()
    }
    fn read(&self, f: &str) -> String {
        std::fs::read_to_string(self.root.join(f)).unwrap_or_default()
    }
    fn write(&self, f: &str, s: &str) {
        let p = self.root.join(f);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(p, s).expect("write");
    }
    /// A constant defined somewhere in the repo, from a Zeitwerk-style path.
    fn some_constant(&mut self) -> String {
        let models: Vec<String> = self
            .rb_files()
            .into_iter()
            .filter(|f| f.starts_with("app/models/") && !f.contains("/concerns/"))
            .collect();
        let f = self.pick(&models);
        f.trim_start_matches("app/models/")
            .trim_end_matches(".rb")
            .split('/')
            .map(|seg| {
                seg.split('_')
                    .map(|w| w[..1].to_uppercase() + &w[1..])
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("::")
    }

    /// A fast-path edit: a statement or comment inside a method body, or a
    /// comment at the end of the file.
    fn body_edit(&mut self, step: usize, changes: &mut ChangeSet) -> String {
        for _ in 0..20 {
            let f = self.pick(&self.rb_files());
            let s = self.read(&f);
            let lines: Vec<&str> = s.lines().collect();
            let d = defs(&lines);
            let (text, what) = match (d.is_empty(), self.rng.below(3)) {
                (false, 0 | 1) => {
                    let (at, name, indent, end) = d[self.rng.below(d.len())].clone();
                    if end == at {
                        continue;
                    }
                    let pad = " ".repeat(indent + 2);
                    let stmt = if self.rng.below(2) == 0 { format!("{pad}ripple_local_{step} = [1, 2].map {{ |x| x * 2 }}") } else { format!("{pad}# ripple comment {step}") };
                    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
                    out.insert(at + 1, stmt);
                    (out.join("\n") + "\n", format!("body edit in {name}"))
                }
                _ => (format!("{s}\n# ripple trailing comment {step}\n"), "trailing comment".to_string()),
            };
            self.write(&f, &text);
            changes.modified.push(f.clone());
            return format!("{what} of {f}");
        }
        "noop".into()
    }

    /// Applies one random operation and returns what changed.
    fn op(&mut self, step: usize, changes: &mut ChangeSet) -> String {
        match self.rng.below(17) {
            0 | 1 => {
                let f = self.pick(&self.rb_files());
                let c = self.some_constant();
                let s = self.read(&f);
                self.write(&f, &format!("{s}\n::{c}\n"));
                changes.modified.push(f.clone());
                format!("add ref {c} in {f}")
            }
            2 => {
                let f = self.pick(&self.rb_files());
                let s = self.read(&f);
                let lines: Vec<&str> = s.lines().collect();
                let idx: Vec<usize> = (0..lines.len())
                    .filter(|i| lines[*i].bytes().any(|b| b.is_ascii_uppercase()))
                    .collect();
                if idx.is_empty() {
                    return "noop".into();
                }
                let drop = idx[self.rng.below(idx.len())];
                let kept: Vec<&str> = lines
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != drop)
                    .map(|(_, l)| *l)
                    .collect();
                self.write(&f, &(kept.join("\n") + "\n"));
                changes.modified.push(f.clone());
                format!("remove line {} of {f}", drop + 1)
            }
            3 => {
                let all = self.files();
                let f = self.pick(&all);
                let s = self.read(&f);
                std::fs::remove_file(self.root.join(&f)).expect("rm");
                self.deleted.push((f.clone(), s));
                changes.removed.push(f.clone());
                format!("delete {f}")
            }
            4 => {
                self.added += 1;
                let name = format!("RippleProp{step}");
                let f = format!("app/models/ripple_prop{step}.rb");
                self.write(&f, &format!("class {name}\n  KIND = :prop\nend\n"));
                let user = self.pick(&self.rb_files());
                let s = self.read(&user);
                self.write(&user, &format!("{s}\n{name}::KIND\n"));
                changes.modified.extend([f.clone(), user.clone()]);
                format!("add {f}, referenced from {user}")
            }
            5 => {
                let candidates: Vec<String> = self
                    .rb_files()
                    .into_iter()
                    .filter(|f| f.starts_with("app/"))
                    .collect();
                for _ in 0..20 {
                    let f = self.pick(&candidates);
                    let s = self.read(&f);
                    if let Some(i) = s.find("\nclass ") {
                        let at = i + "\nclass ".len();
                        let end = s[at..]
                            .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
                            .map_or(s.len(), |e| at + e);
                        let renamed = format!("{}{}Renamed{}", &s[..at], &s[at..end], &s[end..]);
                        self.write(&f, &renamed);
                        changes.modified.push(f.clone());
                        return format!("rename class {} in {f}", &s[at..end]);
                    }
                }
                "noop".into()
            }
            7 | 8 => {
                let ctrls: Vec<String> = self
                    .rb_files()
                    .into_iter()
                    .filter(|f| f.starts_with("app/controllers/") && !f.contains("/concerns/"))
                    .collect();
                if ctrls.is_empty() {
                    return "noop".into();
                }
                let f = self.pick(&ctrls);
                let s = self.read(&f);
                let lines: Vec<&str> = s.lines().collect();
                let defs: Vec<usize> = (0..lines.len())
                    .filter(|i| lines[*i].trim_start().starts_with("def ") && !lines[*i].contains("self."))
                    .collect();
                if defs.is_empty() {
                    return "noop".into();
                }
                let at = defs[self.rng.below(defs.len())];
                let name: String = lines[at]
                    .trim_start()
                    .trim_start_matches("def ")
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                let other = lines[defs[self.rng.below(defs.len())]]
                    .trim_start()
                    .trim_start_matches("def ")
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect::<String>();
                let (insert_after, text, what) = match self.rng.below(3) {
                    0 => {
                        let c = self.some_constant();
                        (at, format!("    ::{c}"), format!("ref {c} inside {name}"))
                    }
                    1 => (at.wrapping_sub(1), "  private".to_string(), format!("private before {name}")),
                    _ => (at, format!("    {other}"), format!("{name} calls {other}")),
                };
                let mut out: Vec<&str> = Vec::with_capacity(lines.len() + 1);
                if insert_after == usize::MAX {
                    out.push(&text);
                }
                for (i, l) in lines.iter().enumerate() {
                    out.push(l);
                    if i == insert_after {
                        out.push(&text);
                    }
                }
                self.write(&f, &(out.join("\n") + "\n"));
                changes.modified.push(f.clone());
                format!("controller {f}: {what}")
            }
            9 => {
                let f = self.pick(&self.rb_files());
                let s = self.read(&f);
                if self.unmentioned.is_empty() || self.rng.below(2) == 0 {
                    // A new method nothing calls (denied as unmentioned) ...
                    let name = format!("ripple_prop_method_{step}");
                    let lines: Vec<&str> = s.lines().collect();
                    let d = defs(&lines);
                    let text = match d.first() {
                        Some(&(at, _, indent, _)) => {
                            let pad = " ".repeat(indent);
                            let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
                            out.insert(at, format!("{pad}def {name}\n{pad}  Time.now\n{pad}end\n"));
                            out.join("\n") + "\n"
                        }
                        None => format!("{s}\ndef {name}\n  1\nend\n"),
                    };
                    self.write(&f, &text);
                    self.unmentioned.push(name.clone());
                    changes.modified.push(f.clone());
                    format!("add unmentioned def {name} in {f}")
                } else {
                    // ... and later its first mention.
                    let i = self.rng.below(self.unmentioned.len());
                    let name = self.unmentioned.swap_remove(i);
                    self.write(&f, &format!("{s}\nripple_receiver.{name}\n"));
                    changes.modified.push(f.clone());
                    format!("first mention of {name} in {f}")
                }
            }
            10 => {
                // More definers of an existing method name (1-3 new ones), which
                // moves the name across the by-name LIMIT.
                let models: Vec<String> = self.rb_files().into_iter().filter(|f| f.starts_with("app/")).collect();
                let src = self.pick(&models);
                let s = self.read(&src);
                let names: Vec<String> = defs(&s.lines().collect::<Vec<_>>()).into_iter().map(|d| d.1).filter(|n| n.len() >= 4).collect();
                if names.is_empty() {
                    return "noop".into();
                }
                let name = names[self.rng.below(names.len())].clone();
                let n = 1 + self.rng.below(3);
                let mut to = Vec::new();
                for _ in 0..n {
                    let f = self.pick(&self.rb_files());
                    let s = self.read(&f);
                    self.write(&f, &format!("{s}\nclass RipplePropDefiner{step}\n  def {name}\n    nil\n  end\nend\n"));
                    changes.modified.push(f.clone());
                    to.push(f);
                }
                format!("{n} more definers of {name} (from {src}) in {to:?}")
            }
            11 => {
                for _ in 0..20 {
                    let f = self.pick(&self.rb_files());
                    let s = self.read(&f);
                    let lines: Vec<&str> = s.lines().collect();
                    let d = defs(&lines);
                    if d.is_empty() {
                        continue;
                    }
                    let (at, name, _, end) = d[self.rng.below(d.len())].clone();
                    let kept: Vec<&str> = lines.iter().enumerate().filter(|(i, _)| *i < at || *i > end).map(|(_, l)| *l).collect();
                    self.write(&f, &(kept.join("\n") + "\n"));
                    changes.modified.push(f.clone());
                    return format!("remove def {name} (lines {}-{}) of {f}", at + 1, end + 1);
                }
                "noop".into()
            }
            13..=15 => self.body_edit(step, changes),
            16 => {
                let tpl: Vec<String> = self.files().into_iter().filter(|f| f.ends_with(".erb") || f.ends_with(".haml")).collect();
                if tpl.is_empty() {
                    return "noop".into();
                }
                let f = self.pick(&tpl);
                let s = self.read(&f);
                self.write(&f, &format!("{s}\n<%= ripple_prop_helper_{step} %>\n"));
                changes.modified.push(f.clone());
                format!("template edit {f}")
            }
            _ => match self.deleted.pop() {
                Some((f, s)) => {
                    self.write(&f, &s);
                    changes.modified.push(f.clone());
                    format!("restore {f}")
                }
                None => "noop".into(),
            },
        }
    }
}

#[test]
#[ignore = "needs RIPPLE_RUBY_REPO (a mastodon checkout); slow"]
fn update_equals_build_on_real_repo() {
    let Some(src) = std::env::var_os("RIPPLE_RUBY_REPO").map(PathBuf::from) else {
        panic!("set RIPPLE_RUBY_REPO to a checkout (e.g. mastodon); it is copied, not modified");
    };
    let seed: u64 = std::env::var("RIPPLE_RUBY_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7);
    let ops: usize = std::env::var("RIPPLE_RUBY_OPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("ripple-ruby-prop-{seed}"));
    copy_repo(&src, &root).expect("copy repo");

    let mut sim = Sim {
        root: root.clone(),
        rng: support::Rng::new(seed),
        deleted: Vec::new(),
        added: 0,
        unmentioned: Vec::new(),
    };
    let big: usize = std::env::var("RIPPLE_RUBY_BIG").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut ex = RubyExtractor::new();
    let first = ex.build(&support::workspace(&root)).expect("build");
    let mut cur = first.edges.clone();
    let mut totals = (0u64, 0u64, 0u64);

    for step in 0..ops {
        let mut changes = ChangeSet::default();
        // Every fourth step batches three operations into one update, every
        // tenth twenty (mostly body edits).
        let n = if big > 0 && step == 5 { big } else if step % 10 == 9 { 20 } else if step % 4 == 3 { 3 } else { 1 };
        let what: Vec<String> = (0..n)
            .map(|k| if n >= 20 && k % 4 != 0 { sim.body_edit(step * 1000 + k, &mut changes) } else { sim.op(step * 1000 + k, &mut changes) })
            .collect();
        changes.modified.sort();
        changes.modified.dedup();
        changes.removed.sort();
        changes.removed.dedup();
        changes.modified.retain(|f| !changes.removed.contains(f) || sim.root.join(f).exists());
        changes.removed.retain(|f| !sim.root.join(f).exists());
        let ws = support::workspace(&root);
        let rows = sim.rng.below(2) == 0;
        let out = if rows { ex.update_rows(&ws, &changes) } else { ex.update(&ws, &changes) }.expect("update");
        report(&out, &mut totals);
        splice(&mut cur, &out);
        let updated = snap(&out, &cur);
        let fresh_out = RubyExtractor::new().build(&ws).expect("build");
        let fresh = snap(&fresh_out, &fresh_out.edges);
        if updated != fresh {
            diff("edge", &fresh.edges, &updated.edges, 20);
            diff("scope", &fresh.scopes, &updated.scopes, 10);
            diff("warning", &fresh.warnings, &updated.warnings, 10);
            diff("stat", &fresh.stats, &updated.stats, 20);
            if fresh.mentions != updated.mentions {
                eprintln!("  mention tables differ");
            }
            if fresh.metrics != updated.metrics {
                eprintln!("  metrics tables differ");
            }
            if fresh.requests != updated.requests {
                eprintln!("  http requests differ");
            }
            let shown: Vec<&String> = what.iter().take(30).collect();
            panic!("step {step} (rows={rows}, {shown:?}): update != build");
        }
        let shown = if what.len() > 3 { format!("{} ops: {} ...", what.len(), what[..3].join("; ")) } else { what.join("; ") };
        println!("step {step:02} ok ({} edges, rows={rows}): {shown}", fresh.edges.len());
    }
    println!(
        "{ops} steps, {} new files added; files on the fast path {}, slow path {}, batches rebuilt {}",
        sim.added, totals.0, totals.1, totals.2
    );
}
