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
/// | `default.touches_global` | touches a `global_files` file | high | `run_full_suite` |
/// | `default.reaches_critical_direct` | reaches a critical area within 1 hop | high | |
/// | `default.reaches_critical` | reaches a critical area at any depth | medium | |
/// | `default.impacted_over_200` | more than 200 impacted files | high | |
/// | `default.impacted_over_50` | more than 50 impacted files | medium | |
/// | `default.crosses_boundary` | Ruby change reaches JS or vice versa | medium | |
///
/// Tier conditions mean "at least this tier", so touching a critical area
/// fires both `touches_*` rules; the final risk is the maximum anyway.
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
            "default.touches_global",
            "Changes a global file whose consumers static analysis cannot see",
            Risk::High,
            &["run_full_suite"],
            When { touches_global: Some(true), ..Default::default() },
        ),
        rule(
            "default.reaches_critical_direct",
            "A file in a critical area directly depends on the change",
            Risk::High,
            &[],
            When { reaches_tier: Some(Risk::Critical), within: Some(1), ..Default::default() },
        ),
        rule(
            "default.reaches_critical",
            "The change reaches a critical area",
            Risk::Medium,
            &[],
            When { reaches_tier: Some(Risk::Critical), ..Default::default() },
        ),
        rule(
            "default.impacted_over_200",
            "Very large blast radius (more than 200 files)",
            Risk::High,
            &[],
            When { impacted_files_over: Some(200), ..Default::default() },
        ),
        rule(
            "default.impacted_over_50",
            "Large blast radius (more than 50 files)",
            Risk::Medium,
            &[],
            When { impacted_files_over: Some(50), ..Default::default() },
        ),
        rule(
            "default.crosses_boundary",
            "The change crosses the Ruby/JavaScript boundary",
            Risk::Medium,
            &[],
            When { crosses_boundary: Some(true), ..Default::default() },
        ),
    ]
}
