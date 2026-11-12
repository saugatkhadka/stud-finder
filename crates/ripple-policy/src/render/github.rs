//! GitHub Actions output: workflow-command annotations on stdout, the
//! markdown report appended to `$GITHUB_STEP_SUMMARY`, and step outputs
//! (`risk`, `require`, `impacted`, `impacted_file_level`, `tests`, `changed`) appended to `$GITHUB_OUTPUT`.

use crate::eval::risk_word;
use crate::report::Report;
use std::io::Write;
use std::path::Path;

/// Annotations are capped so a huge change does not flood the log
/// (GitHub itself shows only a handful per step).
pub const MAX_ANNOTATIONS: usize = 50;

fn escape_data(s: &str) -> String {
    s.replace('%', "%25").replace('\r', "%0D").replace('\n', "%0A")
}

fn escape_prop(s: &str) -> String {
    escape_data(s).replace(':', "%3A").replace(',', "%2C")
}

/// Workflow commands: one `::warning` per changed file, carrying its
/// highest-risk reason (reasons are sorted highest first) and a count of the
/// rest, then a `::notice` with the summary.
pub fn workflow_commands(report: &Report) -> String {
    let mut out = String::new();
    let mut per_file: Vec<(&str, Vec<&crate::report::Reason>)> = Vec::new();
    for reason in &report.reasons {
        for file in &reason.evidence.changed {
            match per_file.iter_mut().find(|(f, _)| *f == file.as_str()) {
                Some((_, rs)) => rs.push(reason),
                None => per_file.push((file, vec![reason])),
            }
        }
    }
    for (file, reasons) in per_file.into_iter().take(MAX_ANNOTATIONS) {
        let top = reasons[0];
        let line = report.changed.iter().find(|c| c.path == file).and_then(|c| c.first_line).unwrap_or(1);
        let title = format!("ripple: {} risk ({})", risk_word(top.risk).to_lowercase(), top.rule);
        let mut message = top.message.clone();
        if reasons.len() > 1 {
            let rest: Vec<&str> = reasons[1..].iter().map(|r| r.rule.as_str()).collect();
            message.push_str(&format!(" (also: {})", rest.join(", ")));
        }
        out.push_str(&format!("::warning file={},line={line},title={}::{}\n", escape_prop(file), escape_prop(&title), escape_data(&message)));
    }
    for w in report.warnings.iter().filter(|w| w.code == "global_file_changed") {
        if let Some(f) = &w.file {
            out.push_str(&format!("::notice file={},title=ripple: global file::{}\n", escape_prop(f), escape_data(&w.message)));
        }
    }
    out.push_str(&format!("::notice title=ripple::{}\n", escape_data(&report.summary)));
    out
}

/// `key=value` lines for `$GITHUB_OUTPUT`.
pub fn outputs(report: &Report) -> String {
    format!(
        "risk={}\nrequire={}\nimpacted={}\nimpacted_file_level={}\ntests={}\nchanged={}\n",
        risk_word(report.risk).to_lowercase(),
        report.require.join(","),
        report.impacted.total,
        report.impacted.file_level_total,
        report.tests_total(),
        report.changed.len()
    )
}

fn append(path: &Path, text: &str) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(text.as_bytes())
}

/// Writes everything: commands to `stdout`, markdown to `summary`, outputs to `output`.
pub fn emit(report: &Report, max_listed: usize, stdout: &mut dyn Write, summary: Option<&Path>, output: Option<&Path>) -> std::io::Result<()> {
    stdout.write_all(workflow_commands(report).as_bytes())?;
    if let Some(p) = summary {
        append(p, &super::markdown::render(report, max_listed))?;
    }
    if let Some(p) = output {
        append(p, &outputs(report))?;
    }
    Ok(())
}
