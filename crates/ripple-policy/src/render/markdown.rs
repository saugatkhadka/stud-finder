//! Markdown for a pull-request comment / step summary. Written for a reader
//! who has never heard of ripple.

use super::{conf_str, detail_with_scope, kind_str, mode_line, path_line, thousands};
use crate::eval::{plural, risk_word};
use crate::report::{MentionHow, Report};
use ripple_core::{Confidence, Risk};
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

/// "**212** impacted files (method-level; 1,362 if whole files are assumed changed)".
fn impacted_head(r: &Report) -> String {
    let (n, whole) = (r.counts.impacted, r.counts.impacted_file_level);
    let mut s = format!("**{}** impacted file{}", thousands(n), plural(n));
    if !r.query.file_level && whole != n {
        s.push_str(&format!(" (method-level; {} if whole files are assumed changed)", thousands(whole)));
    }
    s
}

fn more(o: &mut String, shown: usize, total: usize) {
    if total > shown {
        let _ = writeln!(o, "- …and {} more (`--all` or the JSON report lists them)", total - shown);
    }
}

/// Renders the report. Each list shows at most `max_listed` entries (the
/// counts are always complete).
pub fn render(report: &Report, max_listed: usize) -> String {
    let mut o = String::new();
    let r = report;
    let cap = max_listed.max(1);
    let risk = r.policy.as_ref().map(|p| p.risk);
    let _ = writeln!(o, "{COMMENT_MARKER}");
    match risk {
        Some(k) => {
            let _ = writeln!(o, "## Change impact: {} risk\n", risk_word(k));
        }
        None => {
            let _ = writeln!(o, "## Change impact\n");
        }
    }
    let mut head = Vec::new();
    if let Some(k) = risk {
        head.push(badge(k).to_string());
    }
    head.push(format!("**{}** changed file{}", r.counts.changed, plural(r.counts.changed)));
    head.push(format!("**{}** direct dependent{}", thousands(r.counts.dependents), plural(r.counts.dependents)));
    head.push(impacted_head(r));
    head.push(format!("**{}** test{} reach it", thousands(r.counts.tests), plural(r.counts.tests)));
    if let Some(p) = &r.policy
        && !p.require.is_empty()
    {
        head.push(format!("requires {}", p.require.iter().map(|s| code(s)).collect::<Vec<_>>().join(", ")));
    }
    let _ = writeln!(o, "{}\n", head.join(" · "));
    let _ = writeln!(
        o,
        "<sub>Files that depend, directly or through other files, on the files changed here \
         (Ruby constants and requires, Rails conventions, JS/TS imports, frontend API calls to Rails routes). \
         Risk comes from the rules in this repository's `ripple.yml`.</sub>\n"
    );

    if !r.removed_methods.is_empty() {
        let _ = writeln!(o, "### Removed methods still mentioned\n");
        for m in r.removed_methods.iter().take(cap) {
            let _ = writeln!(o, "- {}: {} file{} still mention it", code(&format!("{}#{}", m.file, m.method)), m.mentions_total, plural(m.mentions_total));
            for x in &m.mentions {
                let at = x.line.map(|l| format!(":{l}")).unwrap_or_default();
                let how = match x.how {
                    MentionHow::SameFile => "same file",
                    MentionHow::Dependent => "dependent",
                    MentionHow::ByName => "by name only",
                };
                let _ = writeln!(o, "  - {} ({how})", code(&format!("{}{at}", x.file)));
            }
        }
        more(&mut o, r.removed_methods.len().min(cap), r.counts.removed_methods);
        let _ = writeln!(o);
    }

    if !r.incomplete.is_empty() {
        let _ = writeln!(o, "### The impact may be incomplete\n");
        for i in &r.incomplete {
            let file = i.file.as_deref().map(|f| format!("{} ", code(f))).unwrap_or_default();
            let _ = writeln!(o, "- {file}{}: {}", code(&i.reason), cell(&i.message));
        }
        let _ = writeln!(o);
    }

    if let Some(p) = &r.policy {
        let _ = writeln!(o, "### Why this risk level\n");
        if p.reasons.is_empty() {
            let _ = writeln!(o, "No rule matched, so the risk is **low**.\n");
        }
        for reason in &p.reasons {
            let _ = write!(o, "- **{}** {}: {}", risk_word(reason.risk), code(&reason.rule), cell(&reason.message));
            if let Some(d) = &reason.description {
                let _ = write!(o, " — _{}_", cell(d));
            }
            let _ = writeln!(o);
            if let Some(path) = reason.evidence.impacted.first() {
                let _ = writeln!(o, "  - e.g. {}", path_line(path, code, "→"));
            }
        }
        let _ = writeln!(o);

        if !p.require.is_empty() {
            let _ = writeln!(o, "### Required before merging\n");
            for req in &p.require {
                let who: Vec<&str> = p.reasons.iter().filter(|x| x.require.contains(req)).map(|x| x.rule.as_str()).collect();
                let _ = writeln!(o, "- [ ] {} (from {})", code(req), who.iter().map(|w| code(w)).collect::<Vec<_>>().join(", "));
            }
            if !p.owners.is_empty() {
                let _ = writeln!(o, "\nSuggested reviewers (owners of the areas involved): {}", p.owners.join(", "));
            }
            let _ = writeln!(o);
        }

        if !p.areas.is_empty() {
            let _ = writeln!(o, "### Areas involved\n");
            let _ = writeln!(o, "| Area | Tier | How | Files | Owners | Closest path |");
            let _ = writeln!(o, "|---|---|---|---:|---|---|");
            for a in &p.areas {
                let how = if a.touched { "changed directly".to_string() } else { format!("{} hop{} away", a.min_depth, plural(a.min_depth as usize)) };
                let example = a.example.as_ref().map(|e| cell(&path_line(e, code, "→"))).unwrap_or_default();
                let _ = writeln!(
                    o,
                    "| {} | {} | {} | {} | {} | {} |",
                    cell(&a.area),
                    risk_word(a.tier).to_lowercase(),
                    how,
                    a.files,
                    cell(&a.owners.join(", ")),
                    example
                );
            }
            let _ = writeln!(o);
        }
    }

    let _ = writeln!(o, "### Changed files\n");
    let with_metrics = r.changed.iter().any(|c| c.metrics.as_ref().is_some_and(|m| !m.is_empty()));
    let (mh, ms) = if with_metrics { (" Metrics |", "---|") } else { ("", "") };
    let _ = writeln!(o, "| File | Status | Lines | Change | Language | Areas | Direct dependents |{mh}");
    let _ = writeln!(o, "|---|---|---|---|---|---|---:|{ms}");
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
            "| {}{} | {} | {} | {} | {} | {} | {} |",
            cell(&file),
            if notes.is_empty() { String::new() } else { format!(" ({})", notes.join(", ")) },
            format!("{:?}", c.status).to_lowercase(),
            lines(c.added, c.deleted),
            cell(&super::text::change_desc(&c.change)),
            c.language.as_str(),
            cell(&c.areas.join(", ")),
            deps
        );
        if with_metrics {
            o.pop();
            let m = c.metrics.as_ref().map(crate::signals::metrics_line).unwrap_or_default();
            let _ = writeln!(o, " {} |", cell(&m));
        }
    }
    if r.counts.changed > r.changed.len() {
        let _ = writeln!(o, "\n…and {} more changed files.", r.counts.changed - r.changed.len());
    }
    let _ = writeln!(o);
    if !r.usually_changes_with.is_empty() {
        let _ = writeln!(o, "### Usually changes with\n");
        let _ = writeln!(o, "<sub>Files not in this diff that git history shows changing together with a changed file.</sub>\n");
        for u in r.usually_changes_with.iter().take(crate::signals::USUALLY_CHANGES_WITH) {
            let _ = writeln!(o, "- {}: {}", code(&u.path), cell(&u.why));
        }
        let _ = writeln!(o);
    }

    let _ = writeln!(o, "### Direct dependents ({})\n", r.counts.dependents);
    if r.counts.dependents == 0 {
        let _ = writeln!(o, "Nothing in the dependency graph depends directly on these changes.\n");
    } else {
        for d in r.dependents.iter().take(cap) {
            let mut why = vec![kind_str(d.kind).to_string()];
            if let Some(det) = detail_with_scope(d.detail.as_deref(), d.to_scope.as_deref()) {
                why.push(code(&det));
            }
            if let Some(fs) = &d.from_scope {
                why.push(format!("in {}", code(fs)));
            }
            if d.confidence != Confidence::Exact {
                why.push(conf_str(d.confidence).to_string());
            }
            if !d.uses_methods.is_empty() {
                why.push(format!("calls {}", d.uses_methods.iter().map(|m| code(m)).collect::<Vec<_>>().join(", ")));
            }
            if d.crosses_boundary {
                why.push("crosses Ruby/JS".to_string());
            }
            let at = d.mention_line.or(d.line).map(|l| format!(":{l}")).unwrap_or_default();
            let areas = if d.areas.is_empty() { String::new() } else { format!(" [{}]", d.areas.join(", ")) };
            let _ = writeln!(o, "- {}{areas} → {} ({})", code(&format!("{}{at}", d.file)), code(&d.of), why.join(", "));
        }
        more(&mut o, r.dependents.len().min(cap), r.counts.dependents - r.counts.inherit_only);
        if r.counts.inherit_only > 0 {
            let files: Vec<String> = r.inherit_only.iter().take(cap).map(|f| code(f)).collect();
            let rest = r.counts.inherit_only - files.len();
            let tail = if rest > 0 { format!(" and {rest} more") } else { String::new() };
            let _ = writeln!(o, "\nInherit from or include it but do not call the changed methods ({}): {}{tail}", r.counts.inherit_only, files.join(", "));
        }
        let _ = writeln!(o);
    }

    for a in r.also_mentioned.iter().filter(|a| a.total > 0) {
        let defs = if a.definitions == 1 { "defined only in the changed file".to_string() } else { format!("{} files define a method of that name", a.definitions) };
        let _ = writeln!(o, "### Also mention {} ({}, no dependency edge)\n", code(&a.name), a.total);
        let _ = writeln!(o, "Matched by name only (receiver type unknown); {defs}.\n");
        for x in a.files.iter().take(cap) {
            let at = x.line.map(|l| format!(":{l}")).unwrap_or_default();
            let _ = writeln!(o, "- {}", code(&format!("{}{at}", x.file)));
        }
        more(&mut o, a.files.len().min(cap), a.total);
        let _ = writeln!(o);
    }

    if r.counts.further > 0 {
        let f = &r.further;
        let depths: Vec<String> = f.by_depth.iter().map(|(d, n)| format!("{n} at {d} hops")).collect();
        let _ = writeln!(o, "### Further away ({})\n", r.counts.further);
        let _ = writeln!(o, "{}. Weakest link on each path: exact = resolved by a parser; inferred = Rails/bundler convention; heuristic = name or URL matching.\n", depths.join(", "));
        if !f.files.is_empty() {
            let _ = writeln!(o, "<details><summary>{} listed</summary>\n", f.files.len().min(cap));
            for x in f.files.iter().take(cap) {
                let mut why = vec![kind_str(x.kind).to_string()];
                if x.confidence != Confidence::Exact {
                    why.push(conf_str(x.confidence).to_string());
                }
                why.extend(x.notable.iter().cloned());
                let areas = if x.areas.is_empty() { String::new() } else { format!(" [{}]", x.areas.join(", ")) };
                let _ = writeln!(o, "- {}{areas} ← {} ({} hops; {})", code(&x.file), code(&x.via), x.depth, why.join(", "));
            }
            let _ = writeln!(o, "\n</details>\n");
        }
    }

    if r.counts.tests > 0 {
        let _ = writeln!(o, "### Tests that reach the change ({}; {} direct)\n", thousands(r.counts.tests), r.counts.tests_direct);
        let _ = writeln!(o, "```");
        for t in r.tests.iter().take(cap) {
            let _ = writeln!(o, "{}", t.file);
        }
        let _ = writeln!(o, "```");
        let shown = r.tests.len().min(cap);
        if r.counts.tests > shown {
            let _ = writeln!(o, "…and {} more.", r.counts.tests - shown);
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
    if let Some(line) = r.signals.as_ref().and_then(super::text::not_ready_line) {
        let _ = writeln!(o, "<sub>{}</sub>\n", cell(&line));
    }

    let _ = writeln!(
        o,
        "<sub>ripple · {} · index generation {} · {} files, {} dependencies · depth limit {} · min confidence {}</sub>",
        mode_line(r),
        r.index.generation,
        r.index.nodes,
        r.index.edges,
        if r.query.max_depth == 0 { "none".to_string() } else { r.query.max_depth.to_string() },
        conf_str(r.query.min_confidence)
    );
    o
}
