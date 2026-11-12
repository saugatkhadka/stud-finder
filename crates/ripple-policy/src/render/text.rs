//! Terminal output: one finding per line, most important first, no
//! decoration, so an agent can read the first lines and know what to check.
//!
//! Order: summary; removed methods still mentioned; reasons the answer may
//! be incomplete; changed files; direct dependents with evidence (callers
//! of the changed methods first), heirs that do not call them, files that
//! mention the names with no edge; what the change relies on; files further
//! away (notable only); tests; policy (one line, when present); warnings;
//! blind spots. Every cut list ends with `+N more (--all)`.

use super::{conf_str, detail_with_scope, kind_str};
use crate::changed::{Change, ChangeKind, FileStatus, WholeFileReason};
use crate::eval::{plural, risk_word};
use crate::report::{MentionHow, Report};
use ripple_core::{Confidence, Risk};
use std::fmt::Write;

struct Style {
    color: bool,
}

impl Style {
    fn paint(&self, code: &str, s: &str) -> String {
        if self.color { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }
    fn risk(&self, r: Risk) -> String {
        let code = match r {
            Risk::Low => "32",
            Risk::Medium => "33",
            Risk::High => "1;31",
            Risk::Critical => "1;37;41",
        };
        self.paint(code, risk_word(r))
    }
}

fn more(o: &mut String, shown: usize, total: usize, what: &str) {
    if total > shown {
        let _ = writeln!(o, "  +{} more{what} (--all)", total - shown);
    }
}

/// "body refund!, charge!; class body" for a change.
pub(crate) fn change_desc(c: &Change) -> String {
    let names = |v: &[String]| v.join(", ");
    let mut parts = Vec::new();
    if let Some(w) = c.whole_file {
        parts.push(
            match w {
                WholeFileReason::Added => "new file",
                WholeFileReason::Deleted => "deleted file",
                WholeFileReason::Renamed => "renamed (whole file)",
                WholeFileReason::NoDiff => "whole file (no diff)",
                WholeFileReason::UnsupportedLanguage => "whole file",
            }
            .to_string(),
        );
    }
    if !c.removed_methods.is_empty() {
        parts.push(format!("removed {}", names(&c.removed_methods)));
    }
    if !c.signatures.is_empty() {
        parts.push(format!("signature {}", names(&c.signatures)));
    }
    if c.class_body {
        parts.push("class body".to_string());
    }
    if !c.added_methods.is_empty() {
        parts.push(format!("added {}", names(&c.added_methods)));
    }
    if !c.methods.is_empty() {
        parts.push(format!("body {}", names(&c.methods)));
    }
    if parts.is_empty() && c.kind == ChangeKind::Body {
        parts.push("no method changed".to_string());
    }
    parts.join("; ")
}

/// Renders the report. Each list shows at most `max_listed` entries (the
/// report itself is already cut to its budget unless it was asked for `all`).
pub fn render(report: &Report, max_listed: usize, color: bool) -> String {
    let st = Style { color };
    let r = report;
    let cap = max_listed.max(1);
    let multi = r.counts.changed > 1;
    let mut o = String::new();
    let _ = writeln!(o, "{}", r.summary);

    // 1. Removed methods that are still mentioned: what will break.
    for m in r.removed_methods.iter().take(cap) {
        let common = if m.other_definitions > 0 { format!(" (name defined in {} other file{})", m.other_definitions, plural(m.other_definitions)) } else { String::new() };
        let _ = writeln!(o, "REMOVED {}#{} still mentioned in {} file{}{common}:", m.file, m.method, m.mentions_total, plural(m.mentions_total));
        for x in &m.mentions {
            let at = x.line.map(|l| format!(":{l}")).unwrap_or_default();
            let how = match x.how {
                MentionHow::SameFile => "same file",
                MentionHow::Dependent => "dependent",
                MentionHow::ByName => "by name only",
            };
            let _ = writeln!(o, "  {}{at}  {how}", x.file);
        }
        more(&mut o, m.mentions.len(), m.mentions_total, "");
    }
    more(&mut o, r.removed_methods.len().min(cap), r.counts.removed_methods, " removed methods");

    // 2. Why the answer may be incomplete. Never silent.
    for i in &r.incomplete {
        let file = i.file.as_deref().map(|f| format!(" {f}")).unwrap_or_default();
        let _ = writeln!(o, "INCOMPLETE{file}: {}: {}", i.reason, i.message);
    }

    // 4. Changed files.
    for c in &r.changed {
        let status = match c.status {
            FileStatus::Added => "A",
            FileStatus::Modified => "M",
            FileStatus::Deleted => "D",
            FileStatus::Renamed => "R",
        };
        let mut extra = vec![c.language.as_str().to_string()];
        if let (Some(a), Some(d)) = (c.added, c.deleted) {
            extra.push(format!("+{a} -{d}"));
        }
        let desc = change_desc(&c.change);
        if !desc.is_empty() {
            extra.push(desc);
        }
        extra.push(if c.in_graph { format!("{} file dependent{}", c.dependents, plural(c.dependents)) } else { "not indexed".into() });
        if !c.areas.is_empty() {
            extra.push(format!("[{}]", c.areas.join(",")));
        }
        if c.is_global {
            extra.push("GLOBAL".into());
        }
        if c.is_test {
            extra.push("test".into());
        }
        let old = c.old_path.as_deref().map(|p| format!(" (from {p})")).unwrap_or_default();
        let _ = writeln!(o, "changed {status} {}{old}  {}", c.path, extra.join("  "));
    }
    more(&mut o, r.changed.len(), r.counts.changed, " changed files");

    // 5. Direct dependents, with evidence.
    if r.counts.dependents > r.counts.inherit_only {
        let shown = r.dependents.len().min(cap);
        let _ = writeln!(o, "dependents {shown}/{}:", r.counts.dependents - r.counts.inherit_only);
        for d in r.dependents.iter().take(cap) {
            let mut line = d.file.clone();
            if let Some(l) = d.mention_line.or(d.line) {
                line.push_str(&format!(":{l}"));
            }
            if let Some(s) = &d.from_scope {
                line.push_str(&format!(" in {s}"));
            }
            line.push_str(&format!("  {}", kind_str(d.kind)));
            if d.inherit {
                line.push_str(" inherits");
            }
            if let Some(det) = detail_with_scope(d.detail.as_deref(), d.to_scope.as_deref()) {
                line.push_str(&format!(" {det}"));
            }
            if d.confidence != Confidence::Exact {
                line.push_str(&format!("  {}", conf_str(d.confidence)));
            }
            if multi {
                line.push_str(&format!("  -> {}", d.of));
            }
            if !d.uses_methods.is_empty() {
                let on = if d.calls_on_constant.is_empty() {
                    String::new()
                } else {
                    format!(" on the constant at {}", d.calls_on_constant.iter().map(u32::to_string).collect::<Vec<_>>().join(","))
                };
                line.push_str(&format!("  calls {}{on}", d.uses_methods.join(", ")));
            }
            if d.crosses_boundary {
                line.push_str("  crosses_boundary");
            }
            if !d.areas.is_empty() {
                line.push_str(&format!("  [{}]", d.areas.join(",")));
            }
            let _ = writeln!(o, "  {line}");
        }
        more(&mut o, shown, r.counts.dependents - r.counts.inherit_only, "");
    }
    if r.counts.inherit_only > 0 {
        let shown: Vec<&str> = r.inherit_only.iter().take(cap).map(String::as_str).collect();
        let rest = r.counts.inherit_only - shown.len();
        let more = if rest > 0 { format!(" +{rest} more (--all)") } else { String::new() };
        let _ = writeln!(o, "inherits/includes, does not call it {}: {}{more}", r.counts.inherit_only, shown.join(" "));
    }

    // Files that mention a changed method with no edge to the changed file.
    for a in &r.also_mentioned {
        let defs = if a.definitions == 1 { "defined only here".to_string() } else { format!("name is common: {} definitions", a.definitions) };
        if a.total == 0 {
            let _ = writeln!(o, "also mentions `{}`: no file outside the dependents ({defs})", a.name);
            continue;
        }
        let single = if a.singleton { "; singleton method, so calls without the constant are likely other methods" } else { "" };
        let colon = if a.files.is_empty() { " (--all lists them)" } else { ":" };
        let _ = writeln!(o, "also mentions `{}` (no dependency edge; receiver type unknown) {} file{}, {defs}{single}{colon}", a.name, a.total, plural(a.total));
        if a.files.is_empty() {
            continue;
        }
        for x in a.files.iter().take(cap) {
            let at = x.line.map(|l| format!(":{l}")).unwrap_or_default();
            let _ = writeln!(o, "  {}{at}", x.file);
        }
        more(&mut o, a.files.len().min(cap), a.total, "");
    }

    // 6. What the changed files rely on (context, not impact).
    if r.counts.relies_on > 0 {
        let shown = r.relies_on.len().min(cap);
        let _ = writeln!(o, "relies on {shown}/{}:", r.counts.relies_on);
        for d in r.relies_on.iter().take(cap) {
            let mut line = format!("{}  <- {}", d.file, d.of);
            if let Some(l) = d.line {
                line.push_str(&format!(":{l}"));
            }
            line.push_str(&format!(" {}", kind_str(d.kind)));
            if let Some(det) = &d.detail {
                line.push_str(&format!(" {det}"));
            }
            if d.confidence != Confidence::Exact {
                line.push_str(&format!("  {}", conf_str(d.confidence)));
            }
            let _ = writeln!(o, "  {line}");
        }
        more(&mut o, shown, r.counts.relies_on, "");
    }

    // 7. Further away: counts plus the notable files.
    if r.counts.further > 0 {
        let f = &r.further;
        let depths: Vec<String> = f.by_depth.iter().map(|(d, n)| format!("{d}:{n}")).collect();
        let langs: Vec<String> = f.by_language.iter().map(|(l, n)| format!("{}:{n}", l.as_str())).collect();
        let mut line = format!("further {} files  hops {}  {}", r.counts.further, depths.join(" "), langs.join(" "));
        if !f.by_area.is_empty() {
            let areas: Vec<String> = f.by_area.iter().map(|(a, n)| format!("{a}:{n}")).collect();
            line.push_str(&format!("  areas {}", areas.join(" ")));
        }
        line.push_str(&format!("  notable {}", r.counts.further_notable));
        let _ = writeln!(o, "{line}");
        let shown = f.files.len().min(cap);
        for x in f.files.iter().take(cap) {
            let mut line = format!("  {}  hop {} via {}", x.file, x.depth, x.via);
            if x.confidence != Confidence::Exact {
                line.push_str(&format!("  {}", conf_str(x.confidence)));
            }
            if !x.notable.is_empty() {
                line.push_str(&format!("  {}", x.notable.join(",")));
            }
            if !x.areas.is_empty() {
                line.push_str(&format!("  [{}]", x.areas.join(",")));
            }
            let _ = writeln!(o, "{line}");
        }
        more(&mut o, shown, r.counts.further, "");
    }

    // 8. Tests reaching the change (static reach, not coverage).
    if r.counts.tests > 0 {
        let shown = r.tests.len().min(cap);
        let _ = writeln!(o, "tests (static reach) {}: {} direct", r.counts.tests, r.counts.tests_direct);
        for t in r.tests.iter().take(cap) {
            let note = match (t.depth, t.mention_line.or(t.line)) {
                (0, _) => "  changed".to_string(),
                (1, Some(l)) => format!(":{l}"),
                (1, None) => String::new(),
                (d, _) => format!("  hop {d}"),
            };
            let _ = writeln!(o, "  {}{note}", t.file);
        }
        more(&mut o, shown, r.counts.tests, "");
    }

    // Policy: one compact line, after the correctness signals; the
    // rule messages only when asked for everything.
    if let Some(p) = &r.policy {
        let mut line = format!("risk {}", st.risk(p.risk));
        if !p.reasons.is_empty() {
            let rules: Vec<&str> = p.reasons.iter().map(|x| x.rule.strip_prefix("default.").unwrap_or(&x.rule)).collect();
            line.push_str(&format!("  rules {}", rules.join(", ")));
        }
        if !p.require.is_empty() {
            line.push_str(&format!("  require {}", p.require.join(", ")));
        }
        if !p.owners.is_empty() {
            line.push_str(&format!("  owners {}", p.owners.join(" ")));
        }
        let _ = writeln!(o, "{line}");
        if r.query.all {
            for reason in p.reasons.iter().take(cap) {
                let _ = writeln!(o, "  {} {}: {}", risk_word(reason.risk), reason.rule, reason.message);
            }
        }
    }

    for w in &r.warnings {
        let file = w.file.as_deref().map(|f| format!("{f}: ")).unwrap_or_default();
        let _ = writeln!(o, "warning: {file}{} ({})", w.message, w.code);
    }
    if !r.blind_spots.is_empty() {
        let _ = writeln!(o, "blind spots: {}", r.blind_spots.join("; "));
    }
    o
}
