//! Rules applied when `ripple.yml` defines none.

use ripple_core::Risk;
use ripple_core::config::{Rule, When};

fn rule(id: &str, description: &str, risk: Risk, require: &[&str], when: When) -> Rule {
    Rule {
        id: id.to_string(),
        description: Some(description.to_string()),
        when,
        risk,
        require: require.iter().map(|s| s.to_string()).collect(),
    }
}

/// The built-in rule set, used only when `config.rules` is empty:
///
/// | id | when | risk | require |
/// |---|---|---|---|
/// | `default.touches_critical` | touches an area of tier critical | critical | `human_review` |
/// | `default.touches_high` | touches an area of tier high | high | `human_review` |
/// | `default.complex_file` | one changed file has complexity over 150 and over 20 changed lines | high | `human_review` |
/// | `default.hotspot_churning` | one changed file is a hotspot with churn over 10 | high | `human_review` |
/// | `default.hotspot_change` | one changed file is a hotspot with over 10 changed lines | high | `human_review` |
/// | `default.impacted_over_1500` | more than 1500 impacted files | high | `run_full_suite` |
/// | `default.impacted_over_200` | more than 200 impacted files | medium | |
/// | `default.reaches_critical_direct` | reaches a critical area within 1 hop | medium | |
/// | `default.reaches_critical` | reaches a critical area at any depth | low | |
/// | `default.complex_method` | one changed file has a method over 25 and over 10 changed lines | medium | |
/// | `default.untested_change` | one changed file has no test reaching it and over 50 changed lines | medium | `add_tests` |
/// | `default.touches_global` | touches a `global_files` file | low | `run_full_suite` |
/// | `default.missing_cochange` | a usual co-change partner (confidence 0.7+) is missing | low | `check_cochange_partners` |
/// | `default.crosses_boundary` | Ruby change reaches JS or vice versa | low | |
///
/// Calibrated by replaying ~300 recent commits each of two Rails apps:
/// global files and boundary crossings fire on a large share of ordinary
/// commits and so only add their required action; complexity and hotspot
/// rules need a substantial edit to that same file (per-file conditions).
/// Metric rules stay silent until history/metrics are available.
///
/// Tier conditions mean "at least this tier", so touching a critical area
/// fires both `touches_*` rules; the report keeps only the highest rung of
/// each such ladder (see `evaluate`).
pub fn default_rules() -> Vec<Rule> {
    vec![
        rule(
            "default.touches_critical",
            "Changes code in a critical area",
            Risk::Critical,
            &["human_review"],
            When { touches_tier: Some(Risk::Critical), ..Default::default() },
        ),
        rule(
            "default.touches_high",
            "Changes code in a high-risk area",
            Risk::High,
            &["human_review"],
            When { touches_tier: Some(Risk::High), ..Default::default() },
        ),
        rule(
            "default.complex_file",
            "Substantial edit to a very complex file",
            Risk::High,
            &["human_review"],
            When { complexity_over: Some(150), changed_lines_over: Some(20), ..Default::default() },
        ),
        rule(
            "default.hotspot_churning",
            "Changes a hotspot that keeps changing",
            Risk::High,
            &["human_review"],
            When { hotspot: Some(true), churn_over: Some(10), ..Default::default() },
        ),
        rule(
            "default.hotspot_change",
            "Substantial edit to a hotspot",
            Risk::High,
            &["human_review"],
            When { hotspot: Some(true), changed_lines_over: Some(10), ..Default::default() },
        ),
        rule(
            "default.impacted_over_1500",
            "Huge blast radius (more than 1500 files)",
            Risk::High,
            &["run_full_suite"],
            When { impacted_files_over: Some(1500), ..Default::default() },
        ),
        rule(
            "default.impacted_over_200",
            "Wide blast radius (more than 200 files)",
            Risk::Medium,
            &[],
            When { impacted_files_over: Some(200), ..Default::default() },
        ),
        rule(
            "default.reaches_critical_direct",
            "A file in a critical area directly depends on the change",
            Risk::Medium,
            &[],
            When { reaches_tier: Some(Risk::Critical), within: Some(1), ..Default::default() },
        ),
        rule(
            "default.reaches_critical",
            "The change reaches a critical area",
            Risk::Low,
            &[],
            When { reaches_tier: Some(Risk::Critical), ..Default::default() },
        ),
        rule(
            "default.complex_method",
            "Edits a file with a very complex method",
            Risk::Medium,
            &[],
            When { max_method_complexity_over: Some(25), changed_lines_over: Some(10), ..Default::default() },
        ),
        rule(
            "default.untested_change",
            "Substantial edit to a file no test reaches",
            Risk::Medium,
            &["add_tests"],
            When { untested: Some(true), changed_lines_over: Some(50), ..Default::default() },
        ),
        rule(
            "default.touches_global",
            "Changes a global file whose consumers static analysis cannot see",
            Risk::Low,
            &["run_full_suite"],
            When { touches_global: Some(true), ..Default::default() },
        ),
        rule(
            "default.missing_cochange",
            "A file that usually changes with this one is not in the change",
            Risk::Low,
            &["check_cochange_partners"],
            When { missing_cochange: Some(true), cochange_min_confidence: Some(0.7), ..Default::default() },
        ),
        rule(
            "default.crosses_boundary",
            "The change crosses the Ruby/JavaScript boundary",
            Risk::Low,
            &[],
            When { crosses_boundary: Some(true), ..Default::default() },
        ),
    ]
}
