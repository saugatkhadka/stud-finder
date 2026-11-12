//! Markdown for a pull-request comment / step summary. Written for a reader
//! who has never heard of ripple.

use super::{conf_str, kind_str, path_line};
use crate::eval::{plural, risk_word};
use crate::report::{ImpactedFile, Report};
use ripple_core::Risk;
use std::fmt::Write;

/// Hidden marker so CI can find and update its previous comment.
pub const COMMENT_MARKER: &str = "<!-- ripple-impact-report -->";

fn code(s: &str) -> String {
    format!("`{}`", s.replace('`', "'"))
}

fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn badge(r: Risk) -> &'static str {
    match r {
        Risk::Low => "![risk: low](https://img.shields.io/badge/risk-low-2ea44f)",
        Risk::Medium => "![risk: medium](https://img.shields.io/badge/risk-medium-dbab09)",
        Risk::High => "![risk: high](https://img.shields.io/badge/risk-high-e36209)",
        Risk::Critical => "![risk: critical](https://img.shields.io/badge/risk-critical-d73a49)",
    }
}

fn lines(added: Option<u32>, deleted: Option<u32>) -> String {
    match (added, deleted) {
        (None, None) => String::new(),
        (a, d) => format!("+{} −{}", a.unwrap_or(0), d.unwrap_or(0)),
    }
}

/// Renders the report. At most `max_listed` impacted files are listed (the
/// counts are always complete).
pub fn render(report: &Report, max_listed: usize) -> String {
    let mut o = String::new();
    let r = report;
    let _ = writeln!(o, "{COMMENT_MARKER}");
    let _ = writeln!(o, "## Change impact: {} risk\n", risk_word(r.risk));
    let mut head = vec![
        badge(r.risk).to_string(),
        format!("**{}** changed file{}", r.changed.len(), plural(r.changed.len())),
        format!("**{}** impacted file{}", r.impacted.total, plural(r.impacted.total)),
        format!("**{}** test{} to run", r.tests_total(), plural(r.tests_total())),
    ];
    if !r.require.is_empty() {
        head.push(format!("requires {}", r.require.iter().map(|s| code(s)).collect::<Vec<_>>().join(", ")));
    }
    let _ = writeln!(o, "{}\n", head.join(" · "));
    let _ = writeln!(
        o,
        "<sub>This report lists every file that depends, directly or through other files, on the files changed here \
         (Ruby constants and requires, Rails conventions, JS/TS imports, frontend API calls to Rails routes). \
         The risk level comes from the rules in this repository's `ripple.yml`.</sub>\n"
    );

    // Why.
    let _ = writeln!(o, "### Why this risk level\n");
    if r.reasons.is_empty() {
        let _ = writeln!(o, "No rule matched, so the risk is **low**.\n");
    }
    for reason in &r.reasons {
        let _ = write!(o, "- **{}** {}: {}", risk_word(reason.risk), code(&reason.rule), cell(&reason.message));
        if let Some(d) = &reason.description {
            let _ = write!(o, " — _{}_", cell(d));
        }
        let _ = writeln!(o);
        if let Some(p) = reason.evidence.impacted.first() {
            let _ = writeln!(o, "  - e.g. {}", path_line(p, code, "→"));
        }
    }
    let _ = writeln!(o);

    if !r.require.is_empty() {
        let _ = writeln!(o, "### Required before merging\n");
        for req in &r.require {
            let who: Vec<&str> = r.reasons.iter().filter(|x| x.require.contains(req)).map(|x| x.rule.as_str()).collect();
            let _ = writeln!(o, "- [ ] {} (from {})", code(req), who.iter().map(|w| code(w)).collect::<Vec<_>>().join(", "));
        }
        if !r.owners.is_empty() {
            let _ = writeln!(o, "\nSuggested reviewers (owners of the areas involved): {}", r.owners.join(", "));
        }
        let _ = writeln!(o);
    }

    if !r.areas_reached.is_empty() {
        let _ = writeln!(o, "### Areas involved\n");
        let _ = writeln!(o, "| Area | Tier | How | Files | Owners | Closest path |");
        let _ = writeln!(o, "|---|---|---|---:|---|---|");
        for a in &r.areas_reached {
            let how = if a.touched { "changed directly".to_string() } else { format!("{} hop{} away", a.min_depth, plural(a.min_depth as usize)) };
            let _ = writeln!(
                o,
                "| {} | {} | {} | {} | {} | {} |",
                cell(&a.area),
                format!("{:?}", a.tier).to_lowercase(),
                how,
                a.files,
                cell(&a.owners.join(", ")),
                cell(&path_line(&a.example, code, "→"))
            );
        }
        let _ = writeln!(o);
    }

    let _ = writeln!(o, "### Changed files\n");
    let _ = writeln!(o, "| File | Status | Lines | Language | Areas | Direct dependents |");
    let _ = writeln!(o, "|---|---|---|---|---|---:|");
    for c in &r.changed {
        let mut file = code(&c.path);
        if let Some(old) = &c.old_path {
            file = format!("{} (was {})", file, code(old));
        }
        let mut notes = Vec::new();
        if c.is_test {
            notes.push("test");
        }
        if c.is_global {
            notes.push("**global**");
        }
        if !c.in_graph {
            notes.push("not indexed");
        }
        let deps = if c.in_graph { c.dependents.to_string() } else { "?".into() };
        let _ = writeln!(
            o,
            "| {}{} | {} | {} | {} | {} | {} |",
            cell(&file),
            if notes.is_empty() { String::new() } else { format!(" ({})", notes.join(", ")) },
            format!("{:?}", c.status).to_lowercase(),
            lines(c.added, c.deleted),
            c.language.as_str(),
            cell(&c.areas.join(", ")),
            deps
        );
    }
    let _ = writeln!(o);

    let imp = &r.impacted;
    let _ = writeln!(o, "### Impacted files ({})\n", imp.total);
    if imp.total == 0 {
        let _ = writeln!(o, "Nothing else in the dependency graph depends on these changes.\n");
    } else {
        let by_conf: Vec<String> = imp.by_min_confidence.iter().rev().map(|(c, n)| format!("{n} {}", conf_str(*c))).collect();
        let _ = writeln!(o, "Weakest link on the path: {}. Exact = resolved by a parser; inferred = Rails/bundler convention; heuristic = name or URL matching.\n", by_conf.join(", "));
        let mut listed = 0;
        let mut depth_groups: Vec<(u32, Vec<&ImpactedFile>)> = Vec::new();
        for f in &imp.files {
            match depth_groups.last_mut() {
                Some((d, v)) if *d == f.depth => v.push(f),
                _ => depth_groups.push((f.depth, vec![f])),
            }
        }
        for (depth, files) in depth_groups {
            if listed >= max_listed {
                break;
            }
            let total_here = imp.by_depth.get(&depth).copied().unwrap_or(files.len());
            let label = if depth == 1 { "Directly depend on a changed file".to_string() } else { format!("{depth} hops away") };
            let _ = writeln!(o, "<details{}><summary>{label} ({total_here})</summary>\n", if depth == 1 { " open" } else { "" });
            for f in files.iter().take(max_listed - listed) {
                let mut why = Vec::new();
                if let Some(k) = f.kind {
                    why.push(kind_str(k).to_string());
                }
                if let Some(d) = &f.detail {
                    why.push(code(d));
                }
                if let Some(c) = f.path_confidence.filter(|c| *c != ripple_core::Confidence::Exact) {
                    why.push(conf_str(c).to_string());
                }
                let via = f.via.as_deref().map(|v| format!(" ← {}", code(v))).unwrap_or_default();
                let areas = if f.areas.is_empty() { String::new() } else { format!(" [{}]", f.areas.join(", ")) };
                let _ = writeln!(o, "- {}{}{} {}", code(&f.file), areas, via, if why.is_empty() { String::new() } else { format!("({})", why.join(", ")) });
                listed += 1;
            }
            let _ = writeln!(o, "\n</details>\n");
        }
        if imp.total > listed {
            let _ = writeln!(o, "…and {} more (see the JSON report for the full list).\n", imp.total - listed);
        }
    }

    if !r.tests_to_run.is_empty() {
        let _ = writeln!(o, "### Tests to run ({})\n", r.tests_total());
        let _ = writeln!(o, "```");
        for t in r.tests_to_run.iter().take(max_listed.max(1)) {
            let _ = writeln!(o, "{}", t.file);
        }
        let _ = writeln!(o, "```");
        let shown = r.tests_to_run.len().min(max_listed.max(1));
        if r.tests_total() > shown {
            let _ = writeln!(o, "…and {} more.", r.tests_total() - shown);
        }
        let _ = writeln!(o);
    }

    if !r.warnings.is_empty() {
        let _ = writeln!(o, "### Warnings\n");
        for w in r.warnings.iter().take(20) {
            let file = w.file.as_deref().map(|f| format!("{}: ", code(f))).unwrap_or_default();
            let _ = writeln!(o, "- {file}{} ({})", cell(&w.message), code(&w.code));
        }
        if r.warnings.len() > 20 {
            let _ = writeln!(o, "- …and {} more", r.warnings.len() - 20);
        }
        let _ = writeln!(o);
    }

    let _ = writeln!(
        o,
        "<sub>ripple · index generation {} · {} files, {} dependencies · depth limit {} · min confidence {}</sub>",
        r.index.generation,
        r.index.nodes,
        r.index.edges,
        if r.query.max_depth == 0 { "none".to_string() } else { r.query.max_depth.to_string() },
        conf_str(r.query.min_confidence)
    );
    o
}
