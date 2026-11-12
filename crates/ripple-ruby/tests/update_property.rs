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

#[path = "../examples/support/mod.rs"]
mod support;

use ripple_core::{ChangeSet, Extractor, Workspace};
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

fn fmt_edges(
    ws: &Workspace,
    ex: &mut RubyExtractor,
    changes: Option<&ChangeSet>,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let out = match changes {
        Some(c) => ex.update(ws, c),
        None => ex.build(ws),
    }
    .expect("extract");
    if out.stats.contains_key("ruby_update_fell_back_to_build") {
        println!("  (update fell back to a full build)");
    }
    if let Some(n) = out
        .stats
        .get("ruby_shadow_docs_reindexed")
        .filter(|n| **n > 0)
    {
        println!("  (shadowing repair re-indexed {n} documents)");
    }
    let mut edges: Vec<String> = out
        .edges
        .iter()
        .map(|e| {
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
        })
        .collect();
    edges.sort();
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
    (edges, warnings, scopes)
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

    /// Applies one random operation and returns what changed.
    fn op(&mut self, step: usize, changes: &mut ChangeSet) -> String {
        match self.rng.below(12) {
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
    let mut ex = RubyExtractor::new();
    fmt_edges(&support::workspace(&root), &mut ex, None);

    for step in 0..ops {
        let mut changes = ChangeSet::default();
        // Every fourth step batches three operations into one update.
        let n = if step % 4 == 3 { 3 } else { 1 };
        let what: Vec<String> = (0..n)
            .map(|k| sim.op(step * 10 + k, &mut changes))
            .collect();
        let ws = support::workspace(&root);
        let updated = fmt_edges(&ws, &mut ex, Some(&changes));
        let fresh = fmt_edges(&ws, &mut RubyExtractor::new(), None);
        if updated != fresh {
            let to_edge = |v: &[String]| {
                v.iter()
                    .map(|s| s.to_string())
                    .collect::<std::collections::BTreeSet<_>>()
            };
            let (a, b) = (to_edge(&fresh.0), to_edge(&updated.0));
            for l in a.difference(&b).take(20) {
                eprintln!("  missing in update: {l}");
            }
            for l in b.difference(&a).take(20) {
                eprintln!("  extra in update:   {l}");
            }
            let (a, b) = (to_edge(&fresh.2), to_edge(&updated.2));
            for l in a.difference(&b).take(10) {
                eprintln!("  scopes missing in update: {l}");
            }
            for l in b.difference(&a).take(10) {
                eprintln!("  scopes extra in update:   {l}");
            }
            panic!("step {step} ({what:?}): update != build");
        }
        println!(
            "step {step:02} ok ({} edges): {}",
            fresh.0.len(),
            what.join("; ")
        );
    }
    println!("{ops} steps, {} new files added", sim.added);
}
