//! `ripple-eval breaks <repo>`: a sharper ground truth for impact than
//! "files changed in the same commit". For each commit that removes or
//! renames a Ruby method or constant, the files that *had to change* are the
//! other files of the commit whose removed lines reference the symbol (more
//! mentions removed than added). ripple is asked, with the graph built at the
//! parent, for the direct dependents of the defining file (method-scoped and
//! whole-file) and compared with `grep -w` and "same directory".
//!
//! Outputs `breaks-summary.md`, `breaks.json`, `break-misses.md`. The
//! repository is never modified: a shared clone under `<out>/.checkout` is
//! moved from parent to parent and removed at the end.

use crate::adapter::{Confidence, Index, NodeId, Query, is_source, language};
use crate::metrics::percentile;
use crate::symbols::{self, CONSTANT_STOPLIST, STOPLIST, SymbolKind, mention_count};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

const USAGE: &str = "usage: ripple-eval breaks <repo> [--commits N] [--out DIR] [--config FILE] [--max-files N] [--keep-checkout]

  --commits N     scan the last N non-merge commits reachable from HEAD (default 1500)
  --out DIR       output directory (default ./ripple-eval-out/<repo name>)
  --config FILE   ripple.yml to use when a checked-out commit has none
  --max-files N   skip commits touching more than N files (default 80)";

const D: usize = 3;

struct Args {
    repo: PathBuf,
    commits: usize,
    out: Option<PathBuf>,
    config: Option<PathBuf>,
    max_files: usize,
    keep: bool,
}

fn parse_args(mut it: impl Iterator<Item = String>) -> Result<Args> {
    let mut a = Args { repo: PathBuf::new(), commits: 1500, out: None, config: None, max_files: 80, keep: false };
    let mut repo = None;
    while let Some(arg) = it.next() {
        let mut val = |name: &str| it.next().with_context(|| format!("{name} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "--commits" => a.commits = val("--commits")?.parse()?,
            "--out" => a.out = Some(val("--out")?.into()),
            "--config" => a.config = Some(val("--config")?.into()),
            "--max-files" => a.max_files = val("--max-files")?.parse()?,
            "--keep-checkout" => a.keep = true,
            s if s.starts_with('-') => bail!("unknown flag `{s}`\n{USAGE}"),
            s => repo = Some(PathBuf::from(s)),
        }
    }
    a.repo = repo.context(USAGE)?.canonicalize().context("repo path")?;
    Ok(a)
}

// ---------------------------------------------------------------- diff parsing

/// One file of a `git diff -U0` patch.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct FilePatch {
    pub old: Option<String>,
    pub new: Option<String>,
    /// (line at the parent, text)
    pub removed: Vec<(u32, String)>,
    /// (line at the commit, text)
    pub added: Vec<(u32, String)>,
}

fn unquote(p: &str) -> String {
    p.trim_matches('"').to_string()
}

fn hunk_range(tok: &str) -> (u32, u32) {
    let tok = &tok[1..];
    let (a, b) = tok.split_once(',').unwrap_or((tok, "1"));
    (a.parse().unwrap_or(0), b.parse().unwrap_or(1))
}

/// Parses a `git diff -U0 --src-prefix=a/ --dst-prefix=b/` patch.
pub fn parse_patch(text: &str) -> Vec<FilePatch> {
    let mut out = Vec::new();
    let mut cur: Option<FilePatch> = None;
    let (mut rem_left, mut add_left, mut ol, mut nl) = (0u32, 0u32, 0u32, 0u32);
    for line in text.split('\n') {
        if rem_left > 0 || add_left > 0 {
            if let Some(c) = cur.as_mut() {
                if rem_left > 0
                    && let Some(r) = line.strip_prefix('-')
                {
                    c.removed.push((ol, r.to_string()));
                    ol += 1;
                    rem_left -= 1;
                    continue;
                }
                if add_left > 0
                    && let Some(r) = line.strip_prefix('+')
                {
                    c.added.push((nl, r.to_string()));
                    nl += 1;
                    add_left -= 1;
                    continue;
                }
            }
            if line.starts_with('\\') {
                continue;
            }
            rem_left = 0;
            add_left = 0;
        }
        if let Some(rest) = line.strip_prefix("diff --git ") {
            out.extend(cur.take());
            let mut p = FilePatch::default();
            if let Some(rest) = rest.strip_prefix("a/").or_else(|| rest.strip_prefix("\"a/"))
                && let Some(i) = rest.find(" b/").or_else(|| rest.find(" \"b/"))
            {
                p.old = Some(unquote(&rest[..i]));
                p.new = Some(unquote(rest[i + 1..].trim_start_matches('"').trim_start_matches("b/")));
            }
            cur = Some(p);
            continue;
        }
        let Some(c) = cur.as_mut() else { continue };
        if line.starts_with("new file mode") {
            c.old = None;
        } else if line.starts_with("deleted file mode") {
            c.new = None;
        } else if let Some(p) = line.strip_prefix("rename from ") {
            c.old = Some(unquote(p));
        } else if let Some(p) = line.strip_prefix("rename to ") {
            c.new = Some(unquote(p));
        } else if line == "--- /dev/null" {
            c.old = None;
        } else if line == "+++ /dev/null" {
            c.new = None;
        } else if let Some(p) = line.strip_prefix("--- ") {
            c.old = Some(unquote(p).trim_start_matches("a/").to_string());
        } else if let Some(p) = line.strip_prefix("+++ ") {
            c.new = Some(unquote(p).trim_start_matches("b/").to_string());
        } else if line.starts_with("@@ -") {
            let toks: Vec<&str> = line.split_whitespace().collect();
            if toks.len() >= 3 {
                let (a, b) = hunk_range(toks[1]);
                let (cc, d) = hunk_range(toks[2]);
                (ol, rem_left, nl, add_left) = (a, b, cc, d);
            }
        }
    }
    out.extend(cur);
    out
}

// ---------------------------------------------------------------- git helpers

/// Runs git; exit code 1 (no match for `grep`) is an empty success.
fn git_lenient(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git").arg("-C").arg(dir).args(["-c", "core.quotepath=off"]).args(args).stdin(Stdio::null()).output().context("running git")?;
    match out.status.code() {
        Some(0) => Ok(out.stdout),
        Some(1) => Ok(Vec::new()),
        _ => bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()),
    }
}

fn show(dir: &Path, rev: &str, path: &str) -> Option<String> {
    crate::adapter::git(dir, &["show", &format!("{rev}:{path}")]).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Files at `rev` containing `word` as a whole word (`git grep -l -w -F`).
fn grep_files(dir: &Path, rev: &str, word: &str) -> Result<BTreeSet<String>> {
    let out = git_lenient(dir, &["grep", "-l", "-I", "-w", "-F", "-e", word, rev, "--"])?;
    let prefix = format!("{rev}:");
    Ok(String::from_utf8_lossy(&out).lines().map(|l| l.strip_prefix(&prefix).unwrap_or(l).to_string()).collect())
}

// ---------------------------------------------------------------- cases

#[derive(Debug, Clone, Serialize)]
pub struct TruthFile {
    pub path: String,
    pub test: bool,
    pub lang: &'static str,
    /// First removed line mentioning the symbol (line at the parent) and its text.
    pub line: u32,
    pub text: String,
    pub removed_mentions: usize,
    pub added_mentions: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Score {
    /// Answer size within depth 1..=D (tests included) and tests among them.
    pub size: [u32; D],
    pub size_tests: [u32; D],
    /// Members of T in the answer within depth 1..=D, and test members.
    pub hits: [u32; D],
    pub hits_tests: [u32; D],
}

#[derive(Debug, Clone, Serialize)]
pub struct Miss {
    pub path: String,
    pub class: String,
    pub line: u32,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Case {
    pub sha: String,
    pub parent: String,
    pub subject: String,
    /// The defining file at the parent, and what happened to it.
    pub file: String,
    pub file_status: &'static str,
    pub name: String,
    pub kind: SymbolKind,
    pub def_line: u32,
    /// Another file of the commit adds a definition of the same name.
    pub moved: bool,
    /// The defining file gained new definitions (a rename is likely).
    pub replaced: bool,
    pub truth: Vec<TruthFile>,
    /// Files whose removed lines mention the name but no more than their added lines do.
    pub loose_only: usize,
    /// Non-source / ignored files whose removed lines mention the name (not in T).
    pub nonsource: usize,
    /// Files that would be in T but define the same name themselves (not in T).
    pub self_defined: usize,
    /// Files at the commit still mentioning the name outside the defining file.
    pub leftovers: usize,
    pub seed_in_graph: bool,
    pub truth_in_graph: usize,
    /// "method" (method-scoped; whole-file for constants), "file", "grep", "grep_src", "same_dir".
    pub scores: BTreeMap<&'static str, Score>,
    /// Size of `grep -w` over every tracked file (not only graph nodes).
    pub grep_all_size: u32,
    pub misses_method: Vec<Miss>,
    pub misses_file: Vec<Miss>,
    /// Files of T that `grep -w` over graph files does not return.
    pub grep_missed: Vec<String>,
    #[serde(skip)]
    f_constants: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct Scan {
    pub repo: String,
    pub head: String,
    pub examined: usize,
    pub no_parent: usize,
    pub too_many_files: usize,
    pub commits_with_removals: usize,
    pub commits_with_cases: usize,
    pub removed: BTreeMap<&'static str, usize>,
    pub operators: usize,
    pub empty_truth: BTreeMap<&'static str, usize>,
    /// (name, kind, |T|, commit) of stoplisted symbols.
    pub stoplisted: Vec<(String, &'static str, usize, String)>,
    /// Files dropped from T because they define the same name themselves.
    pub self_defined: usize,
    /// Removed constants still defined in another file at the commit (skipped).
    pub constant_still_defined: usize,
    pub failed: Vec<String>,
}

/// Is constant `name` defined (`class`/`module`, possibly qualified) in some file at `rev`?
fn defined_elsewhere(dir: &Path, rev: &str, name: &str) -> bool {
    let re = format!("^[[:space:]]*(class|module)[[:space:]]+([A-Za-z0-9_]+::)*{name}([^A-Za-z0-9_]|$)");
    git_lenient(dir, &["grep", "-l", "-I", "-E", "-e", &re, rev, "--", "*.rb"]).is_ok_and(|o| !o.is_empty())
}

fn is_test_path(p: &str) -> bool {
    p.starts_with("spec/") || p.starts_with("test/") || p.contains("/spec/") || p.contains("/test/") || p.contains("__tests__") || p.ends_with("_spec.rb") || p.ends_with("_test.rb")
}

fn interesting_removal(line: &str) -> bool {
    let t = line.trim_start();
    t.contains("def ") || t.starts_with("class ") || t.starts_with("module ") || ["Struct.new", "Class.new", "Module.new", "Data.define"].iter().any(|p| t.contains(p))
}

/// The symbols a commit removed and their ground truth (no index needed).
fn find_cases(repo: &Path, sha: &str, parent: &str, subject: &str, patches: &[FilePatch], ignored: &dyn Fn(&str) -> bool, scan: &mut Scan) -> Vec<Case> {
    let mut cases = Vec::new();
    let mut blobs: HashMap<String, String> = HashMap::new();
    for p in patches {
        let Some(old) = &p.old else { continue };
        if !old.ends_with(".rb") || !p.removed.iter().any(|(_, l)| interesting_removal(l)) {
            continue;
        }
        let Some(before) = show(repo, parent, old) else { continue };
        let after = p.new.as_ref().and_then(|n| show(repo, sha, n));
        let controller = old.contains("app/controllers/") || old.starts_with("controllers/");
        let removed = symbols::removed(&before, after.as_deref(), controller);
        if removed.is_empty() {
            continue;
        }
        let before_defs: BTreeSet<String> = symbols::ruby_defs(&before).into_iter().map(|d| d.name).collect();
        let replaced = after.as_deref().is_some_and(|a| symbols::ruby_defs(a).iter().any(|d| !before_defs.contains(&d.name)));
        let f_constants: Vec<String> = symbols::ruby_constants(&before).into_iter().map(|c| c.0).collect();
        let file_status = match (&p.new, p.new.as_deref() == Some(old.as_str())) {
            (None, _) => "deleted",
            (Some(_), true) => "modified",
            (Some(_), false) => "renamed",
        };
        for r in removed {
            *scan.removed.entry(r.kind.as_str()).or_default() += 1;
            if r.kind != SymbolKind::Constant && symbols::is_operator(&r.name) {
                scan.operators += 1;
                continue;
            }
            let mut truth = Vec::new();
            let (mut loose_only, mut nonsource, mut moved, mut self_defined) = (0, 0, false, 0);
            for g in patches {
                if g.old.as_deref() == Some(old.as_str()) || g.new.as_deref() == p.new.as_deref() && p.new.is_some() {
                    continue;
                }
                if g.new.as_ref().is_some_and(|n| n.ends_with(".rb")) {
                    moved |= g.added.iter().any(|(_, l)| {
                        if r.kind == SymbolKind::Constant {
                            symbols::ruby_constants(l).iter().any(|c| c.0 == r.name)
                        } else {
                            symbols::ruby_defs(l).iter().any(|d| d.name == r.name)
                        }
                    });
                }
                let Some(gold) = &g.old else { continue };
                let rem: usize = g.removed.iter().map(|(_, l)| symbols::reference_count(l, &r.name, r.kind)).sum();
                if rem == 0 {
                    continue;
                }
                if !is_source(gold) || ignored(gold) {
                    nonsource += 1;
                    continue;
                }
                let add: usize = g.added.iter().map(|(_, l)| symbols::reference_count(l, &r.name, r.kind)).sum();
                if rem <= add {
                    loose_only += 1;
                    continue;
                }
                // A file defining the same name itself (a sibling class losing
                // its own copy, an override) references its own definition.
                if gold.ends_with(".rb") {
                    let src = blobs.entry(gold.clone()).or_insert_with(|| show(repo, parent, gold).unwrap_or_default());
                    let own = if r.kind == SymbolKind::Constant { symbols::ruby_constants(src).iter().any(|c| c.0 == r.name) } else { symbols::ruby_defs(src).iter().any(|d| d.name == r.name) };
                    if own {
                        self_defined += 1;
                        continue;
                    }
                }
                let (line, text) = g.removed.iter().find(|(_, l)| symbols::reference_count(l, &r.name, r.kind) > 0).map(|(n, l)| (*n, l.trim().chars().take(140).collect())).unwrap_or_default();
                truth.push(TruthFile { path: gold.clone(), test: is_test_path(gold), lang: language(gold), line, text, removed_mentions: rem, added_mentions: add });
            }
            if (r.kind != SymbolKind::Constant && STOPLIST.contains(&r.name.as_str())) || (r.kind == SymbolKind::Constant && CONSTANT_STOPLIST.contains(&r.name.as_str())) {
                scan.stoplisted.push((r.name.clone(), r.kind.as_str(), truth.len(), sha[..10].to_string()));
                continue;
            }
            scan.self_defined += self_defined;
            if truth.is_empty() {
                *scan.empty_truth.entry(r.kind.as_str()).or_default() += 1;
                continue;
            }
            // A constant still defined in another file at the commit (a reopened
            // namespace, a class moved elsewhere) is not gone: names cannot tell them apart.
            if r.kind == SymbolKind::Constant && defined_elsewhere(repo, sha, &r.name) {
                scan.constant_still_defined += 1;
                continue;
            }
            truth.sort_by(|a, b| a.path.cmp(&b.path));
            cases.push(Case {
                sha: sha.to_string(),
                parent: parent.to_string(),
                subject: subject.to_string(),
                file: old.clone(),
                file_status,
                name: r.name,
                kind: r.kind,
                def_line: r.line,
                moved,
                replaced,
                truth,
                loose_only,
                nonsource,
                self_defined,
                leftovers: 0,
                seed_in_graph: false,
                truth_in_graph: 0,
                scores: BTreeMap::new(),
                grep_all_size: 0,
                misses_method: Vec::new(),
                misses_file: Vec::new(),
                grep_missed: Vec::new(),
                f_constants: f_constants.clone(),
            });
        }
    }
    cases
}

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

fn score(index: &Index, answer: &HashMap<NodeId, u32>, truth: &[TruthFile]) -> Score {
    let mut s = Score::default();
    let bump = |arr: &mut [u32; D], d: u32| {
        for (k, slot) in arr.iter_mut().enumerate() {
            if d as usize <= k + 1 {
                *slot += 1;
            }
        }
    };
    for (&n, &d) in answer {
        bump(&mut s.size, d);
        if index.is_test(n) {
            bump(&mut s.size_tests, d);
        }
    }
    for t in truth {
        let d = index.node(&t.path).and_then(|n| answer.get(&n).copied());
        if let Some(d) = d {
            bump(&mut s.hits, d);
            if t.test {
                bump(&mut s.hits_tests, d);
            }
        }
    }
    s
}

/// Why a file of T is not in a depth-1 answer.
fn classify(repo: &Path, index: &Index, case: &Case, seed: NodeId, t: &TruthFile, scoped: bool, file_dep: &HashMap<NodeId, u32>) -> String {
    let g = index.core_graph();
    let Some(tn) = index.node(&t.path) else { return "not_in_graph".into() };
    let edges: Vec<u32> = g.dependents(seed).filter(|&(n, _)| n == tn).map(|(_, e)| e).collect();
    if !edges.is_empty() {
        if !scoped {
            return "edge_exists_but_dropped".into();
        }
        if edges.iter().all(|&e| g.edge_scopes(e).1.is_some()) {
            return "filtered/action_scope".into();
        }
        let id = ripple_core::mentions::name_id(&case.name);
        return match g.mentions(tn) {
            Some(m) if !m.mentions_any(&[id]) => "filtered/mention_facts_lack_name".into(),
            _ => "filtered/other".into(),
        };
    }
    if let Some(d) = file_dep.get(&tn) {
        return format!("depth_{d}");
    }
    let what = if index.is_test(tn) {
        "test"
    } else {
        match language(&t.path) {
            "template" => "template",
            "js" => "js",
            _ => "ruby",
        }
    };
    let names_constant = show(repo, &case.parent, &t.path).is_some_and(|src| src.lines().any(|l| case.f_constants.iter().any(|c| mention_count(l, c, SymbolKind::Constant) > 0)));
    format!("no_path/{what}/{}", if names_constant { "names_f_constant" } else { "no_f_constant" })
}

fn evaluate(repo: &Path, co: &Path, index: &Index, ignored: &dyn Fn(&str) -> bool, case: &mut Case) -> Result<()> {
    let Some(seed) = index.node(&case.file) else { return Ok(()) };
    case.seed_in_graph = true;
    case.truth_in_graph = case.truth.iter().filter(|t| index.node(&t.path).is_some()).count();
    for t in case.truth.iter_mut() {
        if let Some(n) = index.node(&t.path) {
            t.test = index.is_test(n);
        }
    }
    let q = |file_level| Query { max_depth: D as u32, min_confidence: Confidence::Heuristic, file_level };
    let scopes = (case.kind != SymbolKind::Constant).then(|| vec![case.name.clone()]);
    let method: HashMap<NodeId, u32> = index.impact(&[(seed, scopes)], q(false)).into_iter().collect();
    let file: HashMap<NodeId, u32> = index.impact(&[(seed, None)], q(true)).into_iter().collect();
    let word = case.name.trim_end_matches('=').to_string();
    let grep: BTreeSet<String> = grep_files(co, &case.parent, &word)?.into_iter().filter(|p| *p != case.file && !ignored(p)).collect();
    let grep_nodes: HashMap<NodeId, u32> = grep.iter().filter_map(|p| index.node(p)).map(|n| (n, 1)).collect();
    let grep_src: HashMap<NodeId, u32> = grep_nodes.iter().filter(|(n, _)| is_source(index.path(**n))).map(|(&n, &d)| (n, d)).collect();
    case.grep_all_size = grep.len() as u32;
    let mut left = grep_files(co, &case.sha, &word)?;
    left.remove(&case.file);
    case.leftovers = left.into_iter().filter(|p| !ignored(p)).count();
    let dir = dir_of(&case.file);
    let same: HashMap<NodeId, u32> = (0..index.node_count() as NodeId).filter(|&n| n != seed && dir_of(index.path(n)) == dir).map(|n| (n, 1)).collect();
    case.scores.insert("method", score(index, &method, &case.truth));
    case.scores.insert("file", score(index, &file, &case.truth));
    case.scores.insert("grep", score(index, &grep_nodes, &case.truth));
    case.scores.insert("grep_src", score(index, &grep_src, &case.truth));
    case.scores.insert("same_dir", score(index, &same, &case.truth));
    case.grep_missed = case.truth.iter().filter(|t| !index.node(&t.path).is_some_and(|n| grep_nodes.contains_key(&n))).map(|t| t.path.clone()).collect();
    let in1 = |m: &HashMap<NodeId, u32>, p: &str| index.node(p).and_then(|n| m.get(&n)).is_some_and(|&d| d == 1);
    let snapshot = case.clone();
    for t in &snapshot.truth {
        if !in1(&method, &t.path) {
            let class = classify(repo, index, &snapshot, seed, t, snapshot.kind != SymbolKind::Constant, &file);
            case.misses_method.push(Miss { path: t.path.clone(), class, line: t.line, text: t.text.clone() });
        }
        if !in1(&file, &t.path) {
            let class = classify(repo, index, &snapshot, seed, t, false, &file);
            case.misses_file.push(Miss { path: t.path.clone(), class, line: t.line, text: t.text.clone() });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- report

fn pct(num: u64, den: u64) -> String {
    if den == 0 { "–".into() } else { format!("{:.1}%", 100.0 * num as f64 / den as f64) }
}

const PREDICTORS: [(&str, &str); 5] = [
    ("method", "ripple, method-scoped (`--scope name`)"),
    ("file", "ripple, whole file"),
    ("grep", "`grep -w name` (all indexed files)"),
    ("grep_src", "`grep -w name`, Ruby/JS/templates only"),
    ("same_dir", "same directory"),
];

fn sum<F: Fn(&Case) -> u64>(cases: &[&Case], f: F) -> u64 {
    cases.iter().map(|c| f(c)).sum()
}

pub fn summary(scan: &Scan, all: &[Case]) -> String {
    let mut s = String::new();
    let cases: Vec<&Case> = all.iter().filter(|c| c.seed_in_graph).collect();
    let _ = writeln!(s, "# ripple-eval breaks: {}\n", scan.repo);
    let kinds = |m: &BTreeMap<&'static str, usize>| SymbolKind::ALL.iter().map(|k| format!("{} {}", m.get(k.as_str()).copied().unwrap_or(0), k.as_str())).collect::<Vec<_>>().join(", ");
    let _ = writeln!(
        s,
        "HEAD `{}`. {} non-merge commits scanned ({} root, {} touching >80 files skipped); {} remove a Ruby method or constant. Removed symbols: {} (+{} operators ignored). No file had to change (empty T): {}. Stoplisted: {}. Constants still defined in another file (skipped): {}. **{} cases** with non-empty T in {} commits; {} of them have a defining file outside the graph{}.\n",
        &scan.head[..scan.head.len().min(12)],
        scan.examined,
        scan.no_parent,
        scan.too_many_files,
        scan.commits_with_removals,
        kinds(&scan.removed),
        scan.operators,
        kinds(&scan.empty_truth),
        scan.stoplisted.len(),
        scan.constant_still_defined,
        all.len(),
        scan.commits_with_cases,
        all.len() - cases.len(),
        if scan.failed.is_empty() { String::new() } else { format!("; {} commits failed to index", scan.failed.len()) }
    );
    let t_all = sum(&cases, |c| c.truth.len() as u64);
    let t_graph = sum(&cases, |c| c.truth_in_graph as u64);
    let _ = writeln!(
        s,
        "T = other files of the commit, Ruby/JS/templates, whose removed lines mention the symbol more often than their added lines (word match; comments, hash keys and assignments excluded). |T| = {t_all} files ({t_graph} graph nodes at the parent; {} tests), median {} per case. Not in T: {} files with only balanced mentions, {} non-source/ignored files, {} files defining the same name themselves (scan-wide, incl. symbols with empty T: {}); {} files at the commit still mention the name (U, median per case {}).\n",
        sum(&cases, |c| c.truth.iter().filter(|t| t.test).count() as u64),
        percentile(&cases.iter().map(|c| c.truth.len() as u32).collect::<Vec<_>>(), 50.0),
        sum(&cases, |c| c.loose_only as u64),
        sum(&cases, |c| c.nonsource as u64),
        sum(&cases, |c| c.self_defined as u64),
        scan.self_defined,
        sum(&cases, |c| c.leftovers as u64),
        percentile(&cases.iter().map(|c| c.leftovers as u32).collect::<Vec<_>>(), 50.0),
    );

    let _ = writeln!(s, "## All cases\n\nRecall = Σ|answer ∩ T| / Σ|T| (files). Precision = Σ|answer₁ ∩ T| / Σ|answer₁| at depth 1. Size = depth-1 answer, tests included. For constants the method-scoped row is the whole-file query.\n");
    let _ = writeln!(s, "| answer | recall ≤1 | ≤2 | ≤3 | all of T ≤1 | size ≤1 p50 / p90 / max | empty ≤1 | precision ≤1 | precision ≤1, case median |");
    let _ = writeln!(s, "|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    for (key, label) in PREDICTORS {
        let sc = |c: &Case| c.scores.get(key).cloned().unwrap_or_default();
        let deep = matches!(key, "method" | "file");
        let r = |k: usize| if deep || k == 0 { pct(sum(&cases, |c| sc(c).hits[k] as u64), t_all) } else { "–".into() };
        let full = cases.iter().filter(|c| sc(c).hits[0] as usize == c.truth.len()).count() as u64;
        let sizes: Vec<u32> = cases.iter().map(|c| sc(c).size[0]).collect();
        let empty = sizes.iter().filter(|&&x| x == 0).count() as u64;
        // Per-case precision in 0.1% steps, cases with a non-empty answer.
        let per: Vec<u32> = cases.iter().map(|c| sc(c)).filter(|x| x.size[0] > 0).map(|x| (1000 * x.hits[0] as u64 / x.size[0] as u64) as u32).collect();
        let _ = writeln!(
            s,
            "| {label} | {} | {} | {} | {} | {} / {} / {} | {} | {} | {:.1}% |",
            r(0),
            r(1),
            r(2),
            pct(full, cases.len() as u64),
            percentile(&sizes, 50.0),
            percentile(&sizes, 90.0),
            sizes.iter().max().copied().unwrap_or(0),
            pct(empty, cases.len() as u64),
            pct(sum(&cases, |c| sc(c).hits[0] as u64), sum(&cases, |c| sc(c).size[0] as u64)),
            percentile(&per, 50.0) as f64 / 10.0,
        );
    }

    let _ = writeln!(s, "\n## By symbol kind (depth 1)\n");
    let _ = writeln!(s, "| kind | cases | \\|T\\| | recall method / file / grep | size p50 method / file / grep | precision method / file / grep |");
    let _ = writeln!(s, "|---|---:|---:|---:|---:|---:|");
    for k in SymbolKind::ALL {
        let sub: Vec<&Case> = cases.iter().copied().filter(|c| c.kind == k).collect();
        if sub.is_empty() {
            continue;
        }
        let t = sum(&sub, |c| c.truth.len() as u64);
        let get = |key: &str, c: &Case| c.scores.get(key).cloned().unwrap_or_default();
        let three = |f: &dyn Fn(&str) -> String| ["method", "file", "grep"].iter().map(|k| f(k)).collect::<Vec<_>>().join(" / ");
        let _ = writeln!(
            s,
            "| {} | {} | {t} | {} | {} | {} |",
            k.as_str(),
            sub.len(),
            three(&|key| pct(sum(&sub, |c| get(key, c).hits[0] as u64), t)),
            three(&|key| percentile(&sub.iter().map(|c| get(key, c).size[0]).collect::<Vec<_>>(), 50.0).to_string()),
            three(&|key| pct(sum(&sub, |c| get(key, c).hits[0] as u64), sum(&sub, |c| get(key, c).size[0] as u64))),
        );
    }
    // By what happened to the defining file / whether the symbol moved.
    for (label, pred) in [
        ("defining file modified", Box::new(|c: &Case| c.file_status == "modified") as Box<dyn Fn(&Case) -> bool>),
        ("defining file deleted or renamed", Box::new(|c: &Case| c.file_status != "modified")),
        ("name re-defined in another file of the commit (moved)", Box::new(|c: &Case| c.moved)),
    ] {
        let sub: Vec<&Case> = cases.iter().copied().filter(|c| pred(c)).collect();
        if sub.is_empty() {
            continue;
        }
        let t = sum(&sub, |c| c.truth.len() as u64);
        let get = |key: &str, c: &Case| c.scores.get(key).cloned().unwrap_or_default();
        let three = |f: &dyn Fn(&str) -> String| ["method", "file", "grep"].iter().map(|k| f(k)).collect::<Vec<_>>().join(" / ");
        let _ = writeln!(
            s,
            "| *{label}* | {} | {t} | {} | {} | {} |",
            sub.len(),
            three(&|key| pct(sum(&sub, |c| get(key, c).hits[0] as u64), t)),
            three(&|key| percentile(&sub.iter().map(|c| get(key, c).size[0]).collect::<Vec<_>>(), 50.0).to_string()),
            three(&|key| pct(sum(&sub, |c| get(key, c).hits[0] as u64), sum(&sub, |c| get(key, c).size[0] as u64))),
        );
    }

    let _ = writeln!(s, "\n## Tests vs. non-tests (depth 1)\n");
    let t_tests = sum(&cases, |c| c.truth.iter().filter(|t| t.test).count() as u64);
    let _ = writeln!(s, "| answer | recall, T tests ({t_tests}) | recall, T non-tests ({}) | answer tests p50 | answer non-tests p50 | precision tests | precision non-tests |", t_all - t_tests);
    let _ = writeln!(s, "|---|---:|---:|---:|---:|---:|---:|");
    for (key, label) in PREDICTORS {
        let sc = |c: &Case| c.scores.get(key).cloned().unwrap_or_default();
        let ht = sum(&cases, |c| sc(c).hits_tests[0] as u64);
        let hn = sum(&cases, |c| (sc(c).hits[0] - sc(c).hits_tests[0]) as u64);
        let st = sum(&cases, |c| sc(c).size_tests[0] as u64);
        let sn = sum(&cases, |c| (sc(c).size[0] - sc(c).size_tests[0]) as u64);
        let _ = writeln!(
            s,
            "| {label} | {} | {} | {} | {} | {} | {} |",
            pct(ht, t_tests),
            pct(hn, t_all - t_tests),
            percentile(&cases.iter().map(|c| sc(c).size_tests[0]).collect::<Vec<_>>(), 50.0),
            percentile(&cases.iter().map(|c| sc(c).size[0] - sc(c).size_tests[0]).collect::<Vec<_>>(), 50.0),
            pct(ht, st),
            pct(hn, sn)
        );
    }

    let _ = writeln!(s, "\n## T by language of the file that had to change (recall ≤1, method / file / grep)\n");
    let _ = writeln!(s, "| language | \\|T\\| | method | file | grep |");
    let _ = writeln!(s, "|---|---:|---:|---:|---:|");
    for lang in ["ruby", "template", "js"] {
        let mut n = 0u64;
        let mut hit = [0u64; 3];
        for c in &cases {
            for t in c.truth.iter().filter(|t| t.lang == lang && !t.test) {
                n += 1;
                let miss_m = c.misses_method.iter().any(|m| m.path == t.path);
                let miss_f = c.misses_file.iter().any(|m| m.path == t.path);
                hit[0] += u64::from(!miss_m);
                hit[1] += u64::from(!miss_f);
                hit[2] += u64::from(!c.grep_missed.contains(&t.path));
            }
        }
        if n > 0 {
            let _ = writeln!(s, "| {lang} (non-test) | {n} | {} | {} | {} |", pct(hit[0], n), pct(hit[1], n), pct(hit[2], n));
        }
    }
    {
        let n: u64 = sum(&cases, |c| c.truth.iter().filter(|t| t.test).count() as u64);
        let hm: u64 = sum(&cases, |c| c.truth.iter().filter(|t| t.test && !c.misses_method.iter().any(|m| m.path == t.path)).count() as u64);
        let hf: u64 = sum(&cases, |c| c.truth.iter().filter(|t| t.test && !c.misses_file.iter().any(|m| m.path == t.path)).count() as u64);
        let hg: u64 = sum(&cases, |c| c.truth.iter().filter(|t| t.test && !c.grep_missed.contains(&t.path)).count() as u64);
        let _ = writeln!(s, "| tests (any language) | {n} | {} | {} | {} |", pct(hm, n), pct(hf, n), pct(hg, n));
    }

    let _ = writeln!(s, "\n## Why ripple's depth-1 answer misses a file of T\n");
    let mut classes: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for c in &cases {
        for m in &c.misses_method {
            classes.entry(m.class.clone()).or_default().0 += 1;
        }
        for m in &c.misses_file {
            classes.entry(m.class.clone()).or_default().1 += 1;
        }
    }
    let _ = writeln!(s, "| class | method-scoped misses | whole-file misses |");
    let _ = writeln!(s, "|---|---:|---:|");
    let mut rows: Vec<_> = classes.into_iter().collect();
    rows.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(b.1.1.cmp(&a.1.1)).then(a.0.cmp(&b.0)));
    for (class, (m, f)) in rows {
        let _ = writeln!(s, "| {class} | {m} | {f} |");
    }
    let _ = writeln!(
        s,
        "\n`not_in_graph`: the file is no node at the parent. `filtered/*`: a direct edge exists but the method-scoped query drops it (`mention_facts_lack_name`: ripple's mention facts for the file do not contain the name although its removed lines do). `depth_N`: reached only at depth N (whole-file BFS). `no_path/<kind>/…`: no path within 3 hops; `names_f_constant`: the file names a constant defined in the defining file (an unresolved reference), `no_f_constant`: it does not (receiver type unknown, association, dynamic call, view local). Examples in `break-misses.md`."
    );
    if !scan.stoplisted.is_empty() {
        let mut by: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for (n, _, t, _) in &scan.stoplisted {
            let e = by.entry(n.as_str()).or_default();
            e.0 += 1;
            e.1 += t;
        }
        let shown: Vec<String> = by.iter().map(|(n, (k, t))| format!("`{n}` ×{k} (|T| {t})")).collect();
        let _ = writeln!(s, "\nStoplisted (not measured): {}.", shown.join(", "));
    }
    s
}

pub fn misses_md(all: &[Case]) -> String {
    let mut by: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for c in all.iter().filter(|c| c.seed_in_graph) {
        for m in &c.misses_method {
            by.entry(m.class.clone()).or_default().push(format!(
                "- `{}:{}` ← `{}` {} `{}` ({} {}): `{}`",
                m.path,
                m.line,
                c.file,
                c.kind.as_str(),
                c.name,
                &c.sha[..10],
                c.file_status,
                m.text.replace('`', "'")
            ));
        }
    }
    let mut md = String::from("# Files that had to change but are not in ripple's method-scoped depth-1 answer\n\nUp to 20 examples per class: `missed file:line (at the parent)` ← defining file, symbol (commit, what happened to the defining file): the removed line.\n");
    let mut classes: Vec<_> = by.into_iter().collect();
    classes.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    for (class, v) in classes {
        let _ = writeln!(md, "\n## {class} ({})\n", v.len());
        for line in v.iter().take(20) {
            let _ = writeln!(md, "{line}");
        }
    }
    md
}

// ---------------------------------------------------------------- driver

pub fn run_cli(args: impl Iterator<Item = String>) -> Result<()> {
    let a = parse_args(args)?;
    let started = Instant::now();
    let name = a.repo.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    let out = a.out.clone().unwrap_or_else(|| PathBuf::from("ripple-eval-out").join(&name));
    std::fs::create_dir_all(&out)?;
    let out = out.canonicalize()?;
    if out.starts_with(&a.repo) {
        bail!("--out must be outside the repository");
    }
    let config = a.config.as_ref().map(|c| c.canonicalize()).transpose().context("--config")?;
    let head = String::from_utf8_lossy(&crate::adapter::git(&a.repo, &["rev-parse", "HEAD"])?).trim().to_string();
    let co = crate::Checkout { dir: out.join(".checkout"), keep: a.keep };
    if co.dir.exists() {
        std::fs::remove_dir_all(&co.dir)?;
    }
    let status = Command::new("git").args(["clone", "-q", "--shared", "--no-checkout"]).arg(&a.repo).arg(&co.dir).status()?;
    if !status.success() {
        bail!("git clone failed");
    }
    crate::ensure_config(&co.dir, config.as_deref())?;
    let classifier = crate::adapter::Classifier::load(&co.dir)?;
    let ignored = |p: &str| classifier.is_ignored(p);

    let mut scan = Scan { repo: name.clone(), head, ..Default::default() };
    let candidates = crate::list_commits(&a.repo, a.commits, None)?;
    scan.examined = candidates.len();
    let mut cases: Vec<Case> = Vec::new();
    for c in candidates.iter().rev() {
        let Some(parent) = &c.parent else {
            scan.no_parent += 1;
            continue;
        };
        let names = crate::adapter::git(&co.dir, &["diff", "--name-only", "-M", "--no-ext-diff", parent, &c.sha, "--"])?;
        if String::from_utf8_lossy(&names).lines().count() > a.max_files {
            scan.too_many_files += 1;
            continue;
        }
        let patch = crate::adapter::git(&co.dir, &["diff", "-U0", "--no-color", "-M", "--no-ext-diff", "--src-prefix=a/", "--dst-prefix=b/", parent, &c.sha, "--"])?;
        let patches = parse_patch(&String::from_utf8_lossy(&patch));
        let before = scan.removed.values().sum::<usize>();
        let found = find_cases(&co.dir, &c.sha, parent, &c.subject, &patches, &ignored, &mut scan);
        scan.commits_with_removals += usize::from(scan.removed.values().sum::<usize>() > before);
        scan.commits_with_cases += usize::from(!found.is_empty());
        cases.extend(found);
    }
    eprintln!("scanned {} commits in {:.1}s: {} cases in {} commits", scan.examined, started.elapsed().as_secs_f64(), cases.len(), scan.commits_with_cases);

    // Evaluate, one index per parent.
    let mut i = 0;
    let mut done = 0;
    while i < cases.len() {
        let parent = cases[i].parent.clone();
        let j = i + cases[i..].iter().take_while(|c| c.parent == parent).count();
        let t = Instant::now();
        crate::adapter::git(&co.dir, &["checkout", "-q", "-f", "--detach", &parent])?;
        crate::ensure_config(&co.dir, config.as_deref())?;
        match Index::build(&co.dir) {
            Ok(index) => {
                for case in &mut cases[i..j] {
                    evaluate(&co.dir, &co.dir, &index, &ignored, case)?;
                }
            }
            Err(e) => {
                eprintln!("{} index failed: {e:#}", &parent[..10]);
                scan.failed.push(cases[i].sha.clone());
            }
        }
        done += 1;
        eprintln!("[{done}/{}] {} {} cases, {} ms | {:.0}s", scan.commits_with_cases, &cases[i].sha[..10], j - i, t.elapsed().as_millis(), started.elapsed().as_secs_f64());
        i = j;
    }

    std::fs::write(out.join("breaks-summary.md"), summary(&scan, &cases))?;
    std::fs::write(out.join("break-misses.md"), misses_md(&cases))?;
    std::fs::write(out.join("breaks.json"), serde_json::to_string_pretty(&serde_json::json!({"scan": scan, "cases": cases}))?)?;
    eprintln!("done in {:.1}s; wrote {}/{{breaks-summary.md,breaks.json,break-misses.md}}", started.elapsed().as_secs_f64(), out.display());
    drop(co);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_patches_with_renames_deletions_and_dashes() {
        let text = "diff --git a/app/a.rb b/app/a.rb\nindex 1..2 100644\n--- a/app/a.rb\n+++ b/app/a.rb\n@@ -3,2 +3 @@ class A\n-  def old\n--- not a header\n+  def new\n@@ -10 +9,0 @@\n-  x.old\ndiff --git a/lib/b.rb b/lib/c.rb\nsimilarity index 100%\nrename from lib/b.rb\nrename to lib/c.rb\ndiff --git a/gone.rb b/gone.rb\ndeleted file mode 100644\n--- a/gone.rb\n+++ /dev/null\n@@ -1 +0,0 @@\n-class Gone; end\n\\ No newline at end of file\n";
        let p = parse_patch(text);
        assert_eq!(p.len(), 3);
        assert_eq!((p[0].old.as_deref(), p[0].new.as_deref()), (Some("app/a.rb"), Some("app/a.rb")));
        assert_eq!(p[0].removed, vec![(3, "  def old".to_string()), (4, "-- not a header".to_string()), (10, "  x.old".to_string())]);
        assert_eq!(p[0].added, vec![(3, "  def new".to_string())]);
        assert_eq!((p[1].old.as_deref(), p[1].new.as_deref()), (Some("lib/b.rb"), Some("lib/c.rb")));
        assert_eq!((p[2].old.as_deref(), p[2].new.clone(), p[2].removed.len()), (Some("gone.rb"), None, 1));
    }
}
