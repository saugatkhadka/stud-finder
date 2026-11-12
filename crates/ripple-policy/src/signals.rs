//! Per-file signals besides the dependency graph: complexity (from the
//! index), churn and co-change (git history), line coverage (existing
//! reports) and the hotspot rank. The caller (the daemon or one-shot CLI)
//! looks them up and hands them to [`crate::evaluate_with`] as plain data
//! in a [`SignalInput`]; this crate never reads git or coverage reports.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Whether a signal can be used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalState {
    #[default]
    Ready,
    /// Being computed in the background (git history, the hotspot ranking).
    Building,
    /// Being read in the background (coverage reports).
    Loading,
    /// Turned off in `ripple.yml`.
    Disabled,
    /// Cannot be had: no git repository, no coverage report, ...
    Unavailable,
}

impl SignalState {
    pub fn as_str(self) -> &'static str {
        match self {
            SignalState::Ready => "ready",
            SignalState::Building => "building",
            SignalState::Loading => "loading",
            SignalState::Disabled => "disabled",
            SignalState::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalStatus {
    pub state: SignalState,
    /// Why it is not ready (or a caveat when it is).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl SignalStatus {
    pub fn ready() -> Self {
        Self::default()
    }
    pub fn new(state: SignalState, reason: impl Into<Option<String>>) -> Self {
        Self { state, reason: reason.into() }
    }
    pub fn is_ready(&self) -> bool {
        self.state == SignalState::Ready
    }
}

/// The state of every signal, in the report and in `ripple file` / `rank`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signals {
    /// Code complexity from the index.
    pub complexity: SignalStatus,
    /// Commits per file (git history).
    pub churn: SignalStatus,
    /// Files that usually change together (git history).
    pub cochange: SignalStatus,
    /// Line coverage from existing reports.
    pub coverage: SignalStatus,
    /// The `ripple rank` position.
    pub hotspot: SignalStatus,
}

impl Signals {
    /// `(name, status)` in a fixed order.
    pub fn iter(&self) -> [(&'static str, &SignalStatus); 5] {
        [("complexity", &self.complexity), ("churn", &self.churn), ("cochange", &self.cochange), ("coverage", &self.coverage), ("hotspot", &self.hotspot)]
    }

    /// Signals that are neither ready nor turned off.
    pub fn not_ready(&self) -> Vec<(&'static str, &SignalStatus)> {
        self.iter().into_iter().filter(|(_, s)| !matches!(s.state, SignalState::Ready | SignalState::Disabled)).collect()
    }
}

/// Size, complexity, churn, coverage and hotspot rank of one file. Every
/// part is absent when unknown (or its signal is not ready).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    /// Sum of the method complexities plus decisions outside methods.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complexity: Option<u32>,
    /// The most complex method's complexity (absent without methods).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_method: Option<u32>,
    /// Code lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub churn: Option<ChurnMetric>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<CoverageMetric>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hotspot: Option<HotspotMetric>,
}

impl Metrics {
    pub fn is_empty(&self) -> bool {
        self == &Metrics::default()
    }
}

/// Commits touching the file in the history window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChurnMetric {
    pub commits: u32,
    /// ... of which in the last 90 days of the window.
    pub recent_commits: u32,
    /// Committer time (unix seconds) of the newest one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_changed: Option<i64>,
}

/// Line coverage of a file that is in a coverage report (a file in no
/// report has no `coverage` at all: unknown, not 0%).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CoverageMetric {
    /// `covered / coverable` in percent, one decimal; absent with no coverable lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<f64>,
    pub covered: u32,
    pub coverable: u32,
    /// Coverage of the lines the diff added or changed; absent without a
    /// diff or when none of them is coverable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_lines_percent: Option<f64>,
    /// Measured before the HEAD commit or longer ago than
    /// `coverage.max_age_days`: line numbers may have moved since.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
}

/// Position in `ripple rank` (1 = the riskiest file). In a report only for
/// hotspots; `ripple file` shows it for any ranked file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct HotspotMetric {
    pub rank: u32,
    /// Files ranked (not serialised: the same for every file; `ripple rank`
    /// and `ripple file` show it).
    #[serde(default, skip_serializing)]
    pub of: u32,
    /// Share of the ranked files with a lower score, in percent (one decimal).
    pub percentile: f64,
    /// In the top `rank.hotspot_percent`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hotspot: bool,
}

/// A file outside the diff (and its direct dependents and dependencies)
/// that usually changes together with a changed file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CoChangeEntry {
    pub path: String,
    /// The changed file it changes with (named in `why`; not serialised, to
    /// keep the report small).
    #[serde(default, skip_serializing)]
    pub via: String,
    /// Commits with both ...
    pub together: u32,
    /// ... of the commits touching `via`.
    pub of: u32,
    /// One line: why it is listed.
    pub why: String,
}

/// Per-file signal values computed by the caller.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileSignals {
    pub churn: Option<ChurnMetric>,
    pub coverage: Option<CoverageMetric>,
    pub hotspot: Option<HotspotMetric>,
}

/// What [`crate::evaluate_with`] needs to report and evaluate signals.
#[derive(Debug, Clone, Default)]
pub struct SignalInput {
    pub status: Signals,
    /// Keyed by changed path.
    pub files: BTreeMap<String, FileSignals>,
    /// For the report: partners outside the diff and its direct graph
    /// neighbours, best first (at most [`USUALLY_CHANGES_WITH`] are shown).
    pub usually_changes_with: Vec<CoChangeEntry>,
    /// For `missing_cochange`: partners outside the diff (graph neighbours
    /// included), with their co-change share.
    pub partners: Vec<(CoChangeEntry, f32)>,
    /// `rank.hotspot_percent`.
    pub hotspot_percent: f64,
}

/// Entries of `usually_changes_with` in a report.
pub const USUALLY_CHANGES_WITH: usize = 5;

/// "with `via` in 12 of its 20 commits (60%)": the share of the commits
/// touching `via` that touched the partner too.
pub fn cochange_why(via: &str, together: u32, of: u32) -> String {
    let pct = if of == 0 { 0.0 } else { f64::from(together) * 100.0 / f64::from(of) };
    format!("with {via} in {together} of its {of} commits ({pct:.0}%)")
}

/// `x` rounded to one decimal.
pub fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// The compact one-line rendering used by the text output:
/// `cx 433 (max 88) · churn 41/90d · cov 62% (changed lines 20%) · hotspot #3`
/// (the rank is shown only for a hotspot).
pub fn metrics_line(m: &Metrics) -> String {
    let mut parts = Vec::new();
    if let Some(cx) = m.complexity {
        match m.max_method {
            Some(mx) => parts.push(format!("cx {cx} (max {mx})")),
            None => parts.push(format!("cx {cx}")),
        }
    }
    if let Some(c) = m.churn {
        parts.push(format!("churn {}/90d", c.recent_commits));
    }
    if let Some(c) = m.coverage {
        let mut s = match c.percent {
            Some(p) => format!("cov {}%", fmt_pct(p)),
            None => "cov n/a".to_string(),
        };
        if let Some(cl) = c.changed_lines_percent {
            s.push_str(&format!(" (changed lines {}%)", fmt_pct(cl)));
        }
        if c.stale {
            s.push_str(" stale");
        }
        parts.push(s);
    }
    if let Some(h) = m.hotspot.filter(|h| h.hotspot) {
        parts.push(format!("hotspot #{}", h.rank));
    }
    parts.join(" · ")
}

/// 62.0 -> "62", 62.5 -> "62.5".
pub fn fmt_pct(p: f64) -> String {
    if (p - p.round()).abs() < 0.05 { format!("{:.0}", p) } else { format!("{p:.1}") }
}
