//! Aggregation and output: `summary.md`, `results.json`, `misses.tsv`,
//! `miss_examples.md`. Everything here is deterministic (no timings).

use crate::metrics::{BUCKETS, D, LooCase, VariantResult, percentile};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;

#[derive(Debug, Clone, Serialize)]
pub struct CommitResult {
    pub sha: String,
    pub parent: String,
    pub subject: String,
    /// Files in the diff / analysable source files among them (the 2..=40 filter).
    pub changed: usize,
    pub source_files: usize,
    /// Non-test changed files that are graph nodes at the parent (the LOO targets).
    pub targets: Vec<String>,
    /// How many targets got a method-level seed (not the whole file).
    pub scoped_targets: usize,
    /// Changed test files (Ruby / JS / templates) that are graph nodes at the parent.
    pub tests: Vec<String>,
    /// Changed non-source files under the test globs (fixtures, VCR cassettes): ignored.
    pub test_fixtures: Vec<String>,
    pub added: Vec<String>,
    pub deleted: Vec<String>,
    pub renamed: Vec<String>,
    /// Existed at the parent but not in the graph (ignored, ...).
    pub non_nodes: Vec<String>,
    pub nodes: usize,
    pub edges: usize,
    pub variants: Vec<VariantResult>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Selection {
    pub repo: String,
    pub head: String,
    pub range: String,
    pub examined: usize,
    pub no_parent: usize,
    pub too_few: usize,
    pub too_many: usize,
    pub failed: Vec<String>,
}

fn pct(num: usize, den: usize) -> String {
    if den == 0 { "–".into() } else { format!("{:.1}%", 100.0 * num as f64 / den as f64) }
}

fn variants(commits: &[CommitResult]) -> Vec<String> {
    commits.first().map(|c| c.variants.iter().map(|v| v.variant.clone()).collect()).unwrap_or_default()
}

fn of<'a>(commits: &'a [CommitResult], name: &'a str) -> impl Iterator<Item = &'a VariantResult> + 'a {
    commits.iter().flat_map(move |c| c.variants.iter().filter(move |v| v.variant == name))
}

fn within(d: Option<u32>, k: usize) -> bool {
    d.is_some_and(|d| d as usize <= k)
}

pub fn summary(sel: &Selection, commits: &[CommitResult]) -> String {
    let mut s = String::new();
    let n_targets: usize = commits.iter().map(|c| c.targets.len()).sum();
    let sum = |f: fn(&CommitResult) -> usize| -> usize { commits.iter().map(f).sum() };
    let _ = writeln!(s, "# ripple-eval replay: {}\n", sel.repo);
    let _ = writeln!(
        s,
        "HEAD `{}`, range `{}`. {} non-merge commits examined; skipped: {} root, {} with <2 and {} with >40 analysable source files{}. **{} commits measured.**\n",
        &sel.head[..sel.head.len().min(12)],
        sel.range,
        sel.examined,
        sel.no_parent,
        sel.too_few,
        sel.too_many,
        if sel.failed.is_empty() { String::new() } else { format!(", {} failed to index", sel.failed.len()) },
        commits.len()
    );
    let _ = writeln!(
        s,
        "Changed files: {} total, {} LOO targets (non-test graph nodes at the parent; {} with method-level seeds), {} test files in graph (+{} test fixtures ignored), {} added, {} deleted, {} renamed, {} not in graph. Median graph: {} nodes, {} edges.\n",
        sum(|c| c.changed),
        n_targets,
        sum(|c| c.scoped_targets),
        sum(|c| c.tests.len()),
        sum(|c| c.test_fixtures.len()),
        sum(|c| c.added.len()),
        sum(|c| c.deleted.len()),
        sum(|c| c.renamed.len()),
        sum(|c| c.non_nodes.len()),
        percentile(&commits.iter().map(|c| c.nodes as u32).collect::<Vec<_>>(), 50.0),
        percentile(&commits.iter().map(|c| c.edges as u32).collect::<Vec<_>>(), 50.0),
    );
    let names = variants(commits);
    let reference = names.first().cloned().unwrap_or_default();

    // Leave-one-out recall.
    let _ = writeln!(s, "## Leave-one-out co-change recall\n\nFor each changed non-test file f, seeds = the commit's other non-test changed files. *dependent*: f is in the seeds' impact (reverse); *dependency*: f is reached forward from the seeds; *either*: union.\n");
    let _ = writeln!(s, "| variant | cases | dependent ≤1 | ≤2 | ≤3 | dependency ≤1 | ≤2 | ≤3 | either ≤1 | ≤2 | ≤3 | neither ≤3 | either ≤3, commit mean |");
    let _ = writeln!(s, "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for name in &names {
        let cases: Vec<&LooCase> = of(commits, name).flat_map(|v| &v.loo).collect();
        let n = cases.len();
        let mut row = format!("| {name} | {n} ");
        for get in [|c: &LooCase| c.rev, |c: &LooCase| c.fwd, |c: &LooCase| c.union()] {
            for k in 1..=D {
                let _ = write!(row, "| {} ", pct(cases.iter().filter(|c| within(get(c), k)).count(), n));
            }
        }
        let _ = write!(row, "| {} ", pct(cases.iter().filter(|c| c.union().is_none()).count(), n));
        // Every commit weighs the same, however many files it changed.
        let per: Vec<f64> = of(commits, name).filter(|v| !v.loo.is_empty()).map(|v| v.loo.iter().filter(|c| within(c.union(), 3)).count() as f64 / v.loo.len() as f64).collect();
        let _ = write!(row, "| {} |", if per.is_empty() { "–".to_string() } else { format!("{:.1}%", 100.0 * per.iter().sum::<f64>() / per.len() as f64) });
        let _ = writeln!(s, "{row}");
    }
    let refcases: Vec<&LooCase> = of(commits, &reference).flat_map(|v| &v.loo).collect();
    let rand = |k: usize, union: bool| -> String {
        let n = refcases.len();
        if n == 0 {
            return "–".into();
        }
        let e: f64 = refcases.iter().map(|c| ((if union { c.size_union[k] } else { c.size_rev[k] }) as f64 / c.pool.max(1) as f64).min(1.0)).sum();
        format!("{:.1}%", 100.0 * e / n as f64)
    };
    let _ = writeln!(s, "| random, same size as `{reference}` | {} | {} | {} | {} | | | | {} | {} | {} | | |\n", refcases.len(), rand(0, false), rand(1, false), rand(2, false), rand(0, true), rand(1, true), rand(2, true));

    // The same, for targets the extractors analyse.
    let _ = writeln!(s, "Source-file targets only (Ruby, JS/TS, templates; docs, yml, lockfiles etc. dropped as targets but kept as seeds):\n");
    let _ = writeln!(s, "| variant | cases | dependent ≤1 | ≤3 | dependency ≤3 | either ≤1 | ≤2 | ≤3 | either ≤3, commit mean |");
    let _ = writeln!(s, "|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    for name in &names {
        let cases: Vec<&LooCase> = of(commits, name).flat_map(|v| &v.loo).filter(|c| crate::adapter::is_source(&c.file)).collect();
        let n = cases.len();
        let r = |get: fn(&LooCase) -> Option<u32>, k: usize| pct(cases.iter().filter(|c| within(get(c), k)).count(), n);
        let per: Vec<f64> = of(commits, name)
            .filter_map(|v| {
                let src: Vec<&LooCase> = v.loo.iter().filter(|c| crate::adapter::is_source(&c.file)).collect();
                (!src.is_empty()).then(|| src.iter().filter(|c| within(c.union(), 3)).count() as f64 / src.len() as f64)
            })
            .collect();
        let mean = if per.is_empty() { "–".to_string() } else { format!("{:.1}%", 100.0 * per.iter().sum::<f64>() / per.len() as f64) };
        let _ = writeln!(s, "| {name} | {n} | {} | {} | {} | {} | {} | {} | {mean} |", r(|c| c.rev, 1), r(|c| c.rev, 3), r(|c| c.fwd, 3), r(LooCase::union, 1), r(LooCase::union, 2), r(LooCase::union, 3));
    }
    let _ = writeln!(s);

    // Predicted sizes in the LOO setting.
    let _ = writeln!(s, "Predicted set size in the LOO queries (non-test files, seeds excluded), p50 / p90:\n");
    let _ = writeln!(s, "| variant | dependents ≤1 | ≤2 | ≤3 | either ≤1 | ≤3 |");
    let _ = writeln!(s, "|---|---:|---:|---:|---:|---:|");
    for name in &names {
        let cases: Vec<&LooCase> = of(commits, name).flat_map(|v| &v.loo).collect();
        let pp = |vals: Vec<u32>| format!("{} / {}", percentile(&vals, 50.0), percentile(&vals, 90.0));
        let _ = writeln!(
            s,
            "| {name} | {} | {} | {} | {} | {} |",
            pp(cases.iter().map(|c| c.size_rev[0]).collect()),
            pp(cases.iter().map(|c| c.size_rev[1]).collect()),
            pp(cases.iter().map(|c| c.size_rev[2]).collect()),
            pp(cases.iter().map(|c| c.size_union[0]).collect()),
            pp(cases.iter().map(|c| c.size_union[2]).collect()),
        );
    }

    // Single seed.
    let _ = writeln!(s, "\n## Single-seed impact\n\nEach changed non-test file alone: size of its impact (non-test dependents) and how many of the commit's other non-test changed files it contains. Precision = Σhits / Σsize; *any hit* = seeds whose impact contains ≥1 other changed file (seeds with ≥1 other file only).\n");
    let _ = writeln!(s, "| variant | seeds | size ≤1 p50/p90/max | size ≤3 p50/p90/max | precision ≤1 | ≤2 | ≤3 | any hit ≤1 | ≤3 | empty ≤3 |");
    let _ = writeln!(s, "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for name in &names {
        let cases: Vec<_> = of(commits, name).flat_map(|v| &v.single).collect();
        let dist = |k: usize| {
            let v: Vec<u32> = cases.iter().map(|c| c.size[k]).collect();
            format!("{}/{}/{}", percentile(&v, 50.0), percentile(&v, 90.0), v.iter().max().copied().unwrap_or(0))
        };
        let prec = |k: usize| {
            let hits: u64 = cases.iter().map(|c| c.hits[k] as u64).sum();
            let size: u64 = cases.iter().map(|c| c.size[k] as u64).sum();
            if size == 0 { "–".into() } else { format!("{:.2}%", 100.0 * hits as f64 / size as f64) }
        };
        let with_others: Vec<_> = cases.iter().filter(|c| c.others > 0).collect();
        let any = |k: usize| pct(with_others.iter().filter(|c| c.hits[k] > 0).count(), with_others.len());
        let empty = pct(cases.iter().filter(|c| c.size[2] == 0).count(), cases.len());
        let _ = writeln!(s, "| {name} | {} | {} | {} | {} | {} | {} | {} | {} | {empty} |", cases.len(), dist(0), dist(2), prec(0), prec(1), prec(2), any(0), any(2));
    }
    // The same, for source-file seeds only (the table above mixes in docs/YAML seeds).
    let _ = writeln!(s, "\nSource-file seeds only (Ruby, JS/TS, templates):\n\n| variant | seeds | empty ≤1 | empty ≤3 | size ≤1 p50/p90 | precision ≤1 | ≤3 |\n|---|---:|---:|---:|---:|---:|---:|");
    for name in &names {
        let cases: Vec<_> = of(commits, name).flat_map(|v| &v.single).filter(|c| crate::adapter::is_source(&c.file)).collect();
        let (n, empty) = (cases.len(), |k: usize| pct(cases.iter().filter(|c| c.size[k] == 0).count(), cases.len()));
        let prec = |k: usize| {
            let (h, z): (u64, u64) = (cases.iter().map(|c| c.hits[k] as u64).sum(), cases.iter().map(|c| c.size[k] as u64).sum());
            if z == 0 { "–".to_string() } else { format!("{:.2}%", 100.0 * h as f64 / z as f64) }
        };
        let v: Vec<u32> = cases.iter().map(|c| c.size[0]).collect();
        let _ = writeln!(s, "| {name} | {n} | {} | {} | {}/{} | {} | {} |", empty(0), empty(2), percentile(&v, 50.0), percentile(&v, 90.0), prec(0), prec(2));
    }

    // Tests.
    let _ = writeln!(s, "\n## Changed tests vs. tests reaching the change\n\nCommits that change both non-test files in the graph and test source files in the graph (fixtures excluded). Recall = changed test files that are among the tests reaching the changed sources.\n");
    let _ = writeln!(s, "| variant | commits | changed tests | recall ≤3 | recall any depth | tests reached ≤3 p50/p90 | any depth p50/p90 |");
    let _ = writeln!(s, "|---|---:|---:|---:|---:|---:|---:|");
    for name in &names {
        let cases: Vec<_> = of(commits, name).filter_map(|v| v.tests.as_ref()).collect();
        if cases.is_empty() {
            continue;
        }
        let changed: u32 = cases.iter().map(|c| c.changed).sum();
        let d3: u32 = cases.iter().map(|c| c.reached_d3).sum();
        let all: u32 = cases.iter().map(|c| c.reached_all).sum();
        let p = |v: Vec<u32>| format!("{} / {}", percentile(&v, 50.0), percentile(&v, 90.0));
        let _ = writeln!(
            s,
            "| {name} | {} | {changed} | {} | {} | {} | {} |",
            cases.len(),
            pct(d3 as usize, changed as usize),
            pct(all as usize, changed as usize),
            p(cases.iter().map(|c| c.size_d3).collect()),
            p(cases.iter().map(|c| c.size_all).collect())
        );
    }

    // Which changed tests the reference variant misses, by directory.
    let mut dirs: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for c in commits {
        for v in c.variants.iter().filter(|v| v.variant == reference) {
            let Some(t) = &v.tests else { continue };
            for test in c.tests.iter() {
                let dir = test.split('/').take(2).collect::<Vec<_>>().join("/");
                let e = dirs.entry(dir).or_default();
                e.0 += 1;
                e.1 += usize::from(t.missed_d3.contains(test));
            }
        }
    }
    let mut dirs: Vec<(String, (usize, usize))> = dirs.into_iter().collect();
    dirs.sort_by(|a, b| b.1.1.cmp(&a.1.1).then(a.0.cmp(&b.0)));
    let shown: Vec<String> = dirs.iter().filter(|d| d.1.1 > 0).take(6).map(|(d, (n, m))| format!("`{d}` {m}/{n}")).collect();
    if !shown.is_empty() {
        let _ = writeln!(s, "\nMissed within 3 hops (`{reference}`), by directory, missed/changed: {}.", shown.join(", "));
    }

    // Buckets.
    let _ = writeln!(s, "\n## Recall by kind of target file (`{reference}`)\n");
    let _ = writeln!(s, "| bucket | cases | dependent ≤3 | either ≤1 | either ≤3 | misses ≤3 |");
    let _ = writeln!(s, "|---|---:|---:|---:|---:|---:|");
    for b in BUCKETS {
        let cases: Vec<&&LooCase> = refcases.iter().filter(|c| c.bucket == b).collect();
        if cases.is_empty() {
            continue;
        }
        let n = cases.len();
        let _ = writeln!(
            s,
            "| {b} | {n} | {} | {} | {} | {} |",
            pct(cases.iter().filter(|c| within(c.rev, 3)).count(), n),
            pct(cases.iter().filter(|c| within(c.union(), 1)).count(), n),
            pct(cases.iter().filter(|c| within(c.union(), 3)).count(), n),
            cases.iter().filter(|c| c.union().is_none()).count()
        );
    }
    let _ = writeln!(s, "\nMisses (neither direction within 3 hops, `{reference}`) are listed in `misses.tsv`; the most frequent per bucket in `miss_examples.md`.");
    s
}

/// One row per leave-one-out miss of the reference variant.
pub fn misses(commits: &[CommitResult]) -> (String, String) {
    let reference = variants(commits).first().cloned().unwrap_or_default();
    let mut tsv = String::from("commit\tbucket\tmissed\tseeds\n");
    // bucket -> missed file -> (count, example: (seed count, commit, seeds))
    let mut by: BTreeMap<&str, BTreeMap<String, (usize, (usize, String, Vec<String>))>> = BTreeMap::new();
    for c in commits {
        for v in c.variants.iter().filter(|v| v.variant == reference) {
            for case in v.loo.iter().filter(|x| x.union().is_none()) {
                let seeds: Vec<String> = c.targets.iter().filter(|t| **t != case.file).cloned().collect();
                let _ = writeln!(tsv, "{}\t{}\t{}\t{}", &c.sha[..12.min(c.sha.len())], case.bucket, case.file, seeds.join(","));
                let e = by.entry(case.bucket).or_default().entry(case.file.clone()).or_insert((0, (usize::MAX, String::new(), Vec::new())));
                e.0 += 1;
                if seeds.len() < e.1.0 {
                    e.1 = (seeds.len(), c.sha.clone(), seeds);
                }
            }
        }
    }
    let mut md = format!("# Leave-one-out misses (`{reference}`, neither direction within 3 hops)\n\nTop 15 missed files per bucket by number of commits that missed them; each with the smallest seed set that missed it.\n");
    for b in BUCKETS {
        let Some(files) = by.get(b) else { continue };
        let total: usize = files.values().map(|v| v.0).sum();
        let _ = writeln!(md, "\n## {b} ({total} misses, {} distinct files)\n", files.len());
        let mut v: Vec<(&String, &(usize, (usize, String, Vec<String>)))> = files.iter().collect();
        v.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(b.0)));
        for (file, (count, (_, sha, seeds))) in v.into_iter().take(15) {
            let shown: Vec<&str> = seeds.iter().take(6).map(String::as_str).collect();
            let more = if seeds.len() > 6 { format!(" …+{}", seeds.len() - 6) } else { String::new() };
            let _ = writeln!(md, "- `{file}` ×{count} ← {}{more} ({})", shown.iter().map(|s| format!("`{s}`")).collect::<Vec<_>>().join(", "), &sha[..12.min(sha.len())]);
        }
    }
    (tsv, md)
}
