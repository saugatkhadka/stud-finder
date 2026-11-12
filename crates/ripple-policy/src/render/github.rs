//! GitHub Actions output: workflow-command annotations on stdout, the
//! markdown report appended to `$GITHUB_STEP_SUMMARY`, and step outputs
//! (`risk`, `require`, `impacted`, `tests`, `changed`) appended to `$GITHUB_OUTPUT`.

use crate::eval::risk_word;
use crate::report::Report;
use std::collections::BTreeSet;
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

/// Workflow commands: one `::warning` per (changed file, fired rule) pair,
/// then a `::notice` with the summary.
pub fn workflow_commands(report: &Report) -> String {
    let mut out = String::new();
    let mut seen = BTreeSet::new();
    let mut count = 0;
    for reason in &report.reasons {
        for file in &reason.evidence.changed {
            if count >= MAX_ANNOTATIONS || !seen.insert((file.clone(), reason.rule.clone())) {
                continue;
            }
            let line = report.changed.iter().find(|c| &c.path == file).and_then(|c| c.first_line).unwrap_or(1);
            let title = format!("ripple: {} risk ({})", risk_word(reason.risk).to_lowercase(), reason.rule);
            out.push_str(&format!(
                "::warning file={},line={line},title={}::{}\n",
                escape_prop(file),
                escape_prop(&title),
                escape_data(&reason.message)
            ));
            count += 1;
        }
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
        "risk={}\nrequire={}\nimpacted={}\ntests={}\nchanged={}\n",
        risk_word(report.risk).to_lowercase(),
        report.require.join(","),
        report.impacted.total,
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
