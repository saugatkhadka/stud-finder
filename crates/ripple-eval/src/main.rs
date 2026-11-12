//! `ripple-eval replay <repo>`: replays recent commits of a repository and
//! measures how well ripple's predicted impact matches what each commit
//! actually changed together. See `summary.md` in the output for the metrics.
//!
//! The given repository is never modified: a `git clone --shared
//! --no-checkout` under `<out>/.checkout` is moved from parent to parent and
//! removed at the end (also on error). Each parent is indexed cold, in
//! process (no daemon).

mod adapter;
mod metrics;
mod predict;
mod report;

use adapter::{Classifier, FileChange, Index, Status, commit_changes, git, is_source};
use anyhow::{Context, Result, bail};
use metrics::Truth;
use predict::{Ctx, Seed};
use report::{CommitResult, Selection};
use std::path::{Path, PathBuf};
use std::time::Instant;

const USAGE: &str = "usage: ripple-eval replay <repo> [--commits N] [--since REV] [--out DIR] [--config FILE] [--min-files N] [--max-files N] [--keep-checkout] [--no-history] [--history-explore]

  --commits N     last N non-merge commits reachable from HEAD (default 300)
  --since REV     only commits after REV (REV..HEAD)
  --out DIR       output directory (default ./ripple-eval-out/<repo name>)
  --config FILE   ripple.yml to use when a checked-out commit has none
  --min-files N / --max-files N
                  keep commits changing N..=M analysable source files (default 2..=40)
  --no-history    skip the co-change (ripple-history) variants
  --history-explore
                  also measure alternative history windows, sweep cut-offs and scorings";

struct Args {
    repo: PathBuf,
    commits: usize,
    since: Option<String>,
    out: Option<PathBuf>,
    config: Option<PathBuf>,
    min_files: usize,
    max_files: usize,
    keep: bool,
    history: bool,
    explore: bool,
}

fn parse_args() -> Result<Args> {
    let mut it = std::env::args().skip(1);
    match it.next().as_deref() {
        Some("replay") => {}
        Some("-h" | "--help") | None => {
            println!("{USAGE}");
            std::process::exit(0);
        }
        Some(other) => bail!("unknown command `{other}`\n{USAGE}"),
    }
    let mut a = Args { repo: PathBuf::new(), commits: 300, since: None, out: None, config: None, min_files: 2, max_files: 40, keep: false, history: true, explore: false };
    let mut repo = None;
    while let Some(arg) = it.next() {
        let mut val = |name: &str| it.next().with_context(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--commits" => a.commits = val("--commits")?.parse()?,
            "--since" => a.since = Some(val("--since")?),
            "--out" => a.out = Some(val("--out")?.into()),
            "--config" => a.config = Some(val("--config")?.into()),
            "--min-files" => a.min_files = val("--min-files")?.parse()?,
            "--max-files" => a.max_files = val("--max-files")?.parse()?,
            "--keep-checkout" => a.keep = true,
            "--no-history" => a.history = false,
            "--history-explore" => a.explore = true,
            s if s.starts_with('-') => bail!("unknown flag `{s}`\n{USAGE}"),
            s => repo = Some(PathBuf::from(s)),
        }
    }
    a.repo = repo.context(USAGE)?.canonicalize().context("repo path")?;
    Ok(a)
}

/// The throwaway checkout; removed on drop (normal exit, error or panic).
struct Checkout {
    dir: PathBuf,
    keep: bool,
}

impl Drop for Checkout {
    fn drop(&mut self) {
        if !self.keep && self.dir.exists() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

struct Candidate {
    sha: String,
    parent: Option<String>,
    subject: String,
}

fn list_commits(repo: &Path, n: usize, since: Option<&str>) -> Result<Vec<Candidate>> {
    let range = since.map_or("HEAD".to_string(), |s| format!("{s}..HEAD"));
    let out = git(repo, &["log", "--no-merges", "-n", &n.to_string(), "--format=%H%x00%P%x00%s", &range, "--"])?;
    Ok(String::from_utf8_lossy(&out)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let mut p = l.splitn(3, '\0');
            let sha = p.next().unwrap_or_default().to_string();
            let parent = p.next().and_then(|s| s.split_whitespace().next()).map(str::to_string);
            Candidate { sha, parent, subject: p.next().unwrap_or_default().to_string() }
        })
        .collect())
}

/// Puts the configured `ripple.yml` into the checkout unless the commit has its own.
fn ensure_config(root: &Path, config: Option<&Path>) -> Result<()> {
    let Some(src) = config else { return Ok(()) };
    if ["ripple.yml", ".ripple.yml"].iter().any(|n| root.join(n).exists()) {
        return Ok(());
    }
    std::fs::copy(src, root.join("ripple.yml")).with_context(|| format!("copying {}", src.display()))?;
    Ok(())
}

/// History windows (built and folded forward side by side) and the
/// co-change configurations measured on them. Index 0 is the default the
/// standard `cochange@K` rows use.
fn history_plan(explore: bool, ignore: &[String]) -> (Vec<(String, ripple_history::Options)>, Vec<metrics::GainConfig>) {
    use ripple_history::{Options, PartnerQuery, Score};
    let base = Options { max_commits: 20000, since: None, max_files_per_commit: 30, min_together: 1, keep_per_file: 48, ignore: ignore.to_vec() };
    let mut builds = vec![("w20000/f30".to_string(), base.clone())];
    let dq = predict::default_query();
    let mut gains = vec![metrics::GainConfig { name: "default (t≥3, conf≥0.3, lift≥2) w20000/f30".into(), history: 0, query: dq }];
    if explore {
        for (name, o) in [
            ("w1000/f30", Options { max_commits: 1000, ..base.clone() }),
            ("w5000/f30", Options { max_commits: 5000, ..base.clone() }),
            ("all/f30", Options { max_commits: 0, ..base.clone() }),
            ("w20000/f15", Options { max_files_per_commit: 15, ..base.clone() }),
            ("w20000/f45", Options { max_files_per_commit: 45, ..base.clone() }),
            ("w20000/f60", Options { max_files_per_commit: 60, ..base.clone() }),
            ("w20000/f100", Options { max_files_per_commit: 100, ..base.clone() }),
        ] {
            gains.push(metrics::GainConfig { name: format!("default {name}"), history: builds.len(), query: dq });
            builds.push((name.to_string(), o));
        }
        let q = |t: u32, c: f32, l: f32| PartnerQuery { min_together: t, min_confidence: c, min_lift: l, ..dq };
        for (name, query) in [
            ("t≥1 unfiltered", q(1, 0.0, 0.0)),
            ("t≥2 unfiltered", q(2, 0.0, 0.0)),
            ("t≥3 unfiltered", q(3, 0.0, 0.0)),
            ("t≥2 conf≥0.3 lift≥2", q(2, 0.3, 2.0)),
            ("t≥5 conf≥0.3 lift≥2", q(5, 0.3, 2.0)),
            ("t≥2 conf≥0.5 lift≥2", q(2, 0.5, 2.0)),
            ("t≥3 conf≥0.4 lift≥2", q(3, 0.4, 2.0)),
            ("t≥3 conf≥0.1 lift≥2", q(3, 0.1, 2.0)),
            ("t≥3 conf≥0.2 lift≥2", q(3, 0.2, 2.0)),
            ("t≥3 conf≥0.5 lift≥2", q(3, 0.5, 2.0)),
            ("t≥3 conf≥0.3 no lift", q(3, 0.3, 0.0)),
            ("t≥3 conf≥0.3 lift≥5", q(3, 0.3, 5.0)),
            ("t≥3 lift≥2 only", q(3, 0.0, 2.0)),
            ("t≥2 unfiltered, jaccard", PartnerQuery { score: Score::Jaccard, ..q(2, 0.0, 0.0) }),
            ("t≥2 unfiltered, shrunk", PartnerQuery { score: Score::Shrunk, ..q(2, 0.0, 0.0) }),
            ("default, noisy-or", PartnerQuery { noisy_or: true, ..dq }),
        ] {
            gains.push(metrics::GainConfig { name: format!("{name} w20000/f30"), history: 0, query });
        }
    }
    (builds, gains)
}

fn measure_commit(index: &Index, c: &Candidate, parent: &str, changes: &[FileChange], source_files: usize, history: &[ripple_history::History], gains: &[metrics::GainConfig]) -> CommitResult {
    let mut r = CommitResult {
        sha: c.sha.clone(),
        parent: parent.to_string(),
        subject: c.subject.clone(),
        changed: changes.len(),
        source_files,
        targets: Vec::new(),
        scoped_targets: 0,
        tests: Vec::new(),
        test_fixtures: Vec::new(),
        added: Vec::new(),
        deleted: Vec::new(),
        renamed: Vec::new(),
        non_nodes: Vec::new(),
        nodes: index.node_count(),
        edges: index.edge_count(),
        variants: Vec::new(),
        history: Vec::new(),
    };
    let mut truth = Truth { targets: Vec::new(), tests: Vec::new() };
    for ch in changes {
        match ch.status {
            Status::Added => r.added.push(ch.path.clone()),
            Status::Deleted => r.deleted.push(ch.path.clone()),
            Status::Renamed => r.renamed.push(ch.path.clone()),
            Status::Modified => {}
        }
        let Some(pp) = &ch.parent_path else { continue };
        let Some(node) = index.node(pp) else {
            r.non_nodes.push(pp.clone());
            continue;
        };
        if index.is_test(node) && !is_source(pp) {
            // VCR cassettes, fixtures: under the test globs but nothing to run.
            r.test_fixtures.push(pp.clone());
        } else if index.is_test(node) {
            r.tests.push(pp.clone());
            truth.tests.push(node);
        } else {
            let scopes = index.seed_scopes(pp, ch.status, ch.hunks.as_deref());
            r.scoped_targets += usize::from(scopes.is_some());
            r.targets.push(pp.clone());
            truth.targets.push(Seed { node, scopes });
        }
    }
    let ctx = Ctx { index, diff: changes, history };
    let mut preds = predict::standard();
    if !history.is_empty() {
        preds.extend(predict::cochange_variants());
    }
    r.variants = preds.iter().map(|p| metrics::measure(&ctx, &truth, p.as_ref())).collect();
    r.history = metrics::gain(&ctx, &truth, gains);
    r
}

fn run(a: Args) -> Result<()> {
    let started = Instant::now();
    let name = a.repo.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    let out = a.out.clone().unwrap_or_else(|| PathBuf::from("ripple-eval-out").join(&name));
    std::fs::create_dir_all(&out)?;
    let out = out.canonicalize()?;
    if out.starts_with(&a.repo) {
        bail!("--out must be outside the repository");
    }
    let config = a.config.as_ref().map(|c| c.canonicalize()).transpose().context("--config")?;
    let head = String::from_utf8_lossy(&git(&a.repo, &["rev-parse", "HEAD"])?).trim().to_string();

    let co = Checkout { dir: out.join(".checkout"), keep: a.keep };
    if co.dir.exists() {
        std::fs::remove_dir_all(&co.dir)?;
    }
    eprintln!("cloning {} (shared, no checkout) into {}", a.repo.display(), co.dir.display());
    let status = std::process::Command::new("git").args(["clone", "-q", "--shared", "--no-checkout"]).arg(&a.repo).arg(&co.dir).status()?;
    if !status.success() {
        bail!("git clone failed");
    }
    ensure_config(&co.dir, config.as_deref())?;
    let classifier = Classifier::load(&co.dir)?;

    // Select commits.
    let candidates = list_commits(&a.repo, a.commits, a.since.as_deref())?;
    let mut sel = Selection { repo: name.clone(), head: head.clone(), range: a.since.as_ref().map_or(format!("last {} non-merge", a.commits), |s| format!("{s}..HEAD, last {} non-merge", a.commits)), examined: candidates.len(), ..Default::default() };
    let mut work: Vec<(Candidate, String, Vec<FileChange>, usize)> = Vec::new();
    for c in candidates.into_iter().rev() {
        let Some(parent) = c.parent.clone() else {
            sel.no_parent += 1;
            continue;
        };
        let changes = commit_changes(&a.repo, &parent, &c.sha)?;
        let n = changes.iter().filter(|f| is_source(&f.path) && !classifier.is_ignored(&f.path)).count();
        if n < a.min_files {
            sel.too_few += 1;
        } else if n > a.max_files {
            sel.too_many += 1;
        } else {
            work.push((c, parent, changes, n));
        }
    }
    eprintln!("{} commits examined, {} to measure ({:.1}s)", sel.examined, work.len(), started.elapsed().as_secs_f64());

    // Co-change histories: built at the oldest measured commit's parent and
    // folded forward parent by parent, so each commit is measured with
    // statistics from strictly older commits only.
    let (builds, gains) = if a.history { history_plan(a.explore, &ripple_policy::load_config(&co.dir).map_err(|e| anyhow::anyhow!("{e}"))?.config.ignore) } else { (Vec::new(), Vec::new()) };
    let no_cancel = std::sync::atomic::AtomicBool::new(false);
    let mut hists: Vec<ripple_history::History> = Vec::new();
    if let Some((_, first_parent, _, _)) = work.first() {
        for (name, o) in &builds {
            let t = Instant::now();
            let h = ripple_history::History::build(&a.repo, first_parent, o, &no_cancel)?;
            eprintln!("history {name}: {} commits ({} non-sweep), {} files at {} in {} ms", h.commit_count(), h.pair_commit_count(), h.file_count(), &first_parent[..10], t.elapsed().as_millis());
            hists.push(h);
        }
    }
    sel.history = builds.iter().zip(&hists).map(|((name, _), h)| format!("{name}: {} commits ({} non-sweep) at the first measured parent", h.commit_count(), h.pair_commit_count())).collect();

    let mut results = Vec::new();
    let mut timing = Vec::new();
    let total = work.len();
    for (i, (c, parent, changes, n)) in work.iter().enumerate() {
        let t = Instant::now();
        let mut rebuilds = 0;
        for ((_, o), h) in builds.iter().zip(hists.iter_mut()) {
            let (next, stats) = h.update(&a.repo, parent, o, &no_cancel)?;
            rebuilds += usize::from(!stats.incremental);
            *h = next;
            // No leakage: the window ends at the parent and never holds the commit itself.
            assert_eq!(h.head(), parent.as_str(), "history not at the parent of {}", c.sha);
            assert!(!h.contains_commit(&c.sha), "history window contains the measured commit {}", c.sha);
        }
        let history_ms = t.elapsed().as_millis() as u64;
        let t = Instant::now();
        git(&co.dir, &["checkout", "-q", "-f", "--detach", parent])?;
        ensure_config(&co.dir, config.as_deref())?;
        let checkout_ms = t.elapsed().as_millis() as u64;
        let index = match Index::build(&co.dir) {
            Ok(ix) => ix,
            Err(e) => {
                eprintln!("[{}/{total}] {} index failed: {e:#}", i + 1, &c.sha[..10]);
                sel.failed.push(c.sha.clone());
                continue;
            }
        };
        let t2 = Instant::now();
        let r = measure_commit(&index, c, parent, changes, *n, &hists, &gains);
        let eval_ms = t2.elapsed().as_millis() as u64;
        eprintln!(
            "[{}/{total}] {} {} targets, {} tests | history {history_ms} ms ({rebuilds} rebuilt), checkout {checkout_ms} ms, index {} ms, eval {eval_ms} ms | {:.0}s | {}",
            i + 1,
            &c.sha[..10],
            r.targets.len(),
            r.tests.len(),
            index.build_ms,
            started.elapsed().as_secs_f64(),
            c.subject.chars().take(60).collect::<String>()
        );
        timing.push(serde_json::json!({"sha": c.sha, "history_ms": history_ms, "history_rebuilds": rebuilds, "checkout_ms": checkout_ms, "index_ms": index.build_ms, "eval_ms": eval_ms}));
        results.push(r);
    }

    let summary = report::summary(&sel, &results);
    let (tsv, examples) = report::misses(&results);
    std::fs::write(out.join("summary.md"), &summary)?;
    std::fs::write(out.join("results.json"), serde_json::to_string_pretty(&serde_json::json!({"selection": sel, "commits": results}))?)?;
    std::fs::write(out.join("misses.tsv"), tsv)?;
    std::fs::write(out.join("miss_examples.md"), examples)?;
    let secs = started.elapsed().as_secs_f64();
    std::fs::write(out.join("timing.json"), serde_json::to_string_pretty(&serde_json::json!({"total_s": secs, "commits": timing}))?)?;
    eprintln!("done: {} commits in {secs:.1}s; wrote {}/{{summary.md,results.json,misses.tsv,miss_examples.md,timing.json}}", results.len(), out.display());
    drop(co);
    Ok(())
}

fn main() {
    let result = parse_args().and_then(run);
    if let Err(e) = result {
        eprintln!("ripple-eval: {e:#}");
        std::process::exit(2);
    }
}
