//! Compact terminal output, optionally with ANSI colours.

use super::{kind_str, path_line};
use crate::eval::{plural, risk_word};
use crate::report::Report;
use ripple_core::Risk;
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
    fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
}

/// Renders the report for a terminal; lists at most `max_listed` impacted files.
pub fn render(report: &Report, max_listed: usize, color: bool) -> String {
    let st = Style { color };
    let r = report;
    let mut o = String::new();
    let _ = writeln!(
        o,
        "{} risk  {} changed, {} impacted{}, {} test{} to run  {}",
        st.risk(r.risk),
        r.changed.len(),
        r.impacted.total,
        if r.impacted.total > 0 { format!(" (depth ≤{})", r.impacted.max_depth) } else { String::new() },
        r.tests_total(),
        plural(r.tests_total()),
        st.dim(&format!("gen {}", r.index.generation))
    );
    for reason in &r.reasons {
        let _ = writeln!(o, "  {:<8} {} {}", st.risk(reason.risk), st.bold(&reason.rule), reason.message);
        if let Some(p) = reason.evidence.impacted.first() {
            let _ = writeln!(o, "           {}", st.dim(&path_line(p, |f| f.to_string(), "->")));
        }
    }
    if !r.require.is_empty() {
        let _ = writeln!(o, "require: {}", r.require.join(", "));
    }
    if !r.owners.is_empty() {
        let _ = writeln!(o, "owners:  {}", r.owners.join(", "));
    }
    if !r.areas_reached.is_empty() {
        let areas: Vec<String> = r
            .areas_reached
            .iter()
            .map(|a| format!("{} ({:?}, {} file{}{})", a.area, a.tier, a.files, plural(a.files), if a.touched { ", changed" } else { "" }).to_lowercase())
            .collect();
        let _ = writeln!(o, "areas:   {}", areas.join("; "));
    }
    let _ = writeln!(o, "changed:");
    for c in &r.changed {
        let status = match c.status {
            crate::FileStatus::Added => "A",
            crate::FileStatus::Modified => "M",
            crate::FileStatus::Deleted => "D",
            crate::FileStatus::Renamed => "R",
        };
        let mut extra = vec![c.language.as_str().to_string()];
        if let (Some(a), Some(d)) = (c.added, c.deleted) {
            extra.push(format!("+{a} -{d}"));
        }
        if !c.areas.is_empty() {
            extra.push(format!("[{}]", c.areas.join(",")));
        }
        extra.push(if c.in_graph { format!("{} dependent{}", c.dependents, plural(c.dependents)) } else { "not indexed".into() });
        if c.is_global {
            extra.push("GLOBAL".into());
        }
        if c.is_test {
            extra.push("test".into());
        }
        let old = c.old_path.as_deref().map(|p| format!(" (from {p})")).unwrap_or_default();
        let _ = writeln!(o, "  {status} {}{old}  {}", c.path, st.dim(&extra.join("  ")));
    }
    if r.impacted.total > 0 {
        let depths: Vec<String> = r.impacted.by_depth.iter().map(|(d, n)| format!("{d}:{n}")).collect();
        let _ = writeln!(o, "impacted by depth: {}", depths.join("  "));
        for f in r.impacted.files.iter().take(max_listed) {
            let via = f.via.as_deref().unwrap_or("");
            let kind = f.kind.map(kind_str).unwrap_or("");
            let _ = writeln!(o, "  {} {}  {}", f.depth, f.file, st.dim(&format!("<- {via} ({kind})")));
        }
        let listed = r.impacted.files.len().min(max_listed);
        if r.impacted.total > listed {
            let _ = writeln!(o, "  {}", st.dim(&format!("... {} more (--format json for all)", r.impacted.total - listed)));
        }
    }
    if !r.tests_to_run.is_empty() {
        let shown: Vec<&str> = r.tests_to_run.iter().take(max_listed).map(|t| t.file.as_str()).collect();
        let more = r.tests_total().saturating_sub(shown.len());
        let _ = writeln!(o, "tests: {}{}", shown.join(" "), if more > 0 { format!(" ... +{more}") } else { String::new() });
    }
    for w in &r.warnings {
        let file = w.file.as_deref().map(|f| format!("{f}: ")).unwrap_or_default();
        let _ = writeln!(o, "{} {file}{}", st.paint("33", "warning:"), w.message);
    }
    o
}
