//! The built-in areas and rules: what ripple applies with no `ripple.yml`,
//! and in addition to one (see `defaults:` in [`ripple_core::config::DefaultsConfig`]).
//!
//! Merge semantics, in one place:
//! * built-in areas and rules apply in addition to the file's own;
//! * an area or rule of the file with the same name / id replaces the built-in one;
//! * `defaults.disable: [complex_file, payments]` removes built-ins by id
//!   (with or without the `default.` prefix) or area name;
//! * `defaults.areas: false` / `defaults.rules: false` drop them all;
//! * a built-in rule about a built-in area (`default.schema_change`) goes
//!   away with that area.
//!
//! Built-in areas only ever match where their paths exist, so a repository
//! without, say, `db/migrate` simply has no `migrations` area in its reports.

use ripple_core::Risk;
use ripple_core::config::{Area, Rule, When};

/// Source roots the keyword areas look under (never tests: those are
/// classified first and never count toward areas reached).
const SRC: &str = "{app,lib,ee/app,ee/lib,src}";

fn area(tier: Risk, description: &str, paths: &[String]) -> Area {
    Area { paths: paths.to_vec(), tier, owners: Vec::new(), description: Some(description.to_string()) }
}

/// The built-in areas, by name.
///
/// | area | tier | what |
/// |---|---|---|
/// | `payments` | medium | billing, payments, payouts, invoices, refunds, checkout, purchases, Stripe/PayPal/Braintree |
/// | `auth` | high | sessions, authentication, passwords, OAuth/OmniAuth, two-factor, WebAuthn, logins, `app/models/user.rb`, Devise/Doorkeeper/OmniAuth/Rack::Attack initializers |
/// | `migrations` | medium | `db/migrate`, `db/post_migrate`, `db/schema.rb`, `db/structure.sql` |
/// | `api` | medium | `app/controllers/api`, GraphQL, Grape (`lib/api`) |
/// | `config` | low | `config/` initializers, environments, routes and YAML |
/// | `jobs` | low | `app/jobs`, `app/workers`, `app/sidekiq` |
/// | `mailers` | low | `app/mailers` and mailer views |
/// | `shared_components` | low | JavaScript `components/` and `vue_shared/` |
pub fn default_areas() -> Vec<(String, Area)> {
    let kw = |words: &str| format!("{SRC}/**/*{{{words}}}*");
    let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    vec![
        (
            "payments".into(),
            // Medium, not high: keyword matching is a guess, and in a payments
            // product it matches a third of the code (Gumroad: 30% of commits).
            // Redefine `payments` with your own paths and tier to sharpen it.
            area(
                Risk::Medium,
                "Billing and payments (built-in, by path keywords)",
                &[kw("billing,payment,Payment,payout,Payout,stripe,Stripe,paypal,PayPal,braintree,invoice,Invoice,refund,Refund,checkout,Checkout,purchase,Purchase")],
            ),
        ),
        (
            "auth".into(),
            area(
                Risk::High,
                "Authentication and sessions (built-in, by path keywords)",
                &[
                    vec![kw("session,Session,authenticat,Authenticat,password,Password,oauth,OAuth,omniauth,two_factor,TwoFactor,webauthn,WebAuthn,login,Login")],
                    v(&["app/models/user.rb", "config/initializers/{devise,doorkeeper,omniauth,rack_attack}*"]),
                ]
                .concat(),
            ),
        ),
        (
            "migrations".into(),
            area(
                Risk::Medium,
                "Database schema changes (built-in)",
                &v(&["{db,ee/db}/{migrate,post_migrate}/**", "db/schema.rb", "db/structure.sql", "db/*_schema.rb", "db/*_structure.sql"]),
            ),
        ),
        ("api".into(), area(Risk::Medium, "Public API endpoints (built-in)", &v(&["{app,ee/app}/controllers/api/**", "{app,ee/app}/graphql/**", "{lib,ee/lib}/api/**"]))),
        (
            "config".into(),
            area(
                Risk::Low,
                "Application configuration (built-in)",
                &v(&["config/initializers/**", "config/environments/**", "config/application.rb", "config/routes.rb", "config/routes/**", "config/*.yml"]),
            ),
        ),
        ("jobs".into(), area(Risk::Low, "Background jobs (built-in)", &v(&["{app,ee/app}/{jobs,workers,sidekiq}/**"]))),
        ("mailers".into(), area(Risk::Low, "Email (built-in)", &v(&["{app,ee/app}/mailers/**", "{app,ee/app}/views/**/*_mailer/**"]))),
        (
            "shared_components".into(),
            area(
                Risk::Low,
                "Shared front-end components (built-in)",
                &v(&["{app,ee/app}/{javascript,frontend,assets/javascripts}/**/components/**", "{app,ee/app}/assets/javascripts/vue_shared/**", "src/components/**"]),
            ),
        ),
    ]
}

fn rule(id: &str, description: &str, risk: Risk, require: &[&str], when: When) -> Rule {
    Rule {
        id: id.to_string(),
        description: Some(description.to_string()),
        when,
        risk,
        require: require.iter().map(|s| s.to_string()).collect(),
    }
}

/// The built-in rule set:
///
/// | id | when | risk | require |
/// |---|---|---|---|
/// | `default.touches_critical` | touches an area of tier critical | critical | `human_review` |
/// | `default.touches_high` | touches an area of tier high (built-in: `auth`) | high | `human_review` |
/// | `default.schema_change` | touches the `migrations` area | medium | `human_review` |
/// | `default.touches_medium` | touches an area of tier medium | medium | |
/// | `default.complex_file` | one changed file is in the top 2% by complexity and has over 30 changed lines | high | `human_review` |
/// | `default.hotspot_churning` | one changed file is a hotspot with churn over 20 | high | `human_review` |
/// | `default.hotspot_change` | one changed file is a hotspot with over 50 changed lines | high | `human_review` |
/// | `default.impacted_over_40pct` | impacts more than 40% of the repository's files | high | `run_full_suite` |
/// | `default.impacted_over_5pct` | impacts more than 5% of the repository's files | medium | |
/// | `default.widely_used` | one changed file is in the top 1% by dependents and has over 10 changed lines | medium | |
/// | `default.complex_method` | one changed file is in the top 2% by most complex method and has over 10 changed lines | medium | |
/// | `default.reaches_critical_direct` | reaches a critical area within 1 hop | medium | |
/// | `default.reaches_critical` | reaches a critical area at any depth | low | |
/// | `default.untested_change` | one changed file has no test reaching it and over 50 changed lines | medium | `add_tests` |
/// | `default.touches_global` | touches a `global_files` file | low | `run_full_suite` |
/// | `default.missing_cochange` | a usual co-change partner (confidence 0.7+) is missing | low | `check_cochange_partners` |
/// | `default.crosses_boundary` | Ruby change reaches JS or vice versa | low | |
///
/// Each relative threshold has an absolute floor (`complexity_over: 50`,
/// `max_method_complexity_over: 10`, `dependents_over: 10`,
/// `impacted_files_over: 100` / `25`) so a small repository's "top 2%" is
/// still something worth a look.
///
/// Thresholds are relative to the repository (`*_top_percent`,
/// `impacted_files_over_percent`), so the same rules fit a 2,000-file and
/// a 60,000-file codebase. Calibrated by replaying ~300 recent commits each
/// of two Rails apps (Mastodon, Gumroad) offline, aiming at 10-15% of
/// commits rated high or critical. No one set reaches that on both: these
/// put 8.3% (Mastodon) and 16.9% (Gumroad) at high+, the closest pair
/// found (see docs/CONFIG.md, "Calibration").
///
/// Tier conditions mean "at least this tier", so touching a critical area
/// fires every `touches_*` rule; the report keeps only the highest rung of
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
            "default.schema_change",
            "Changes the database schema",
            Risk::Medium,
            &["human_review"],
            When { touches: vec!["migrations".into()], ..Default::default() },
        ),
        rule(
            "default.touches_medium",
            "Changes code in a medium-risk area",
            Risk::Medium,
            &[],
            When { touches_tier: Some(Risk::Medium), ..Default::default() },
        ),
        rule(
            "default.complex_file",
            "Substantial edit to one of the most complex files in the repository",
            Risk::High,
            &["human_review"],
            When { complexity_top_percent: Some(2.0), complexity_over: Some(50), changed_lines_over: Some(30), ..Default::default() },
        ),
        rule(
            "default.hotspot_churning",
            "Changes a hotspot that keeps changing",
            Risk::High,
            &["human_review"],
            When { hotspot: Some(true), churn_over: Some(20), ..Default::default() },
        ),
        rule(
            "default.hotspot_change",
            "Substantial edit to a hotspot",
            Risk::High,
            &["human_review"],
            When { hotspot: Some(true), changed_lines_over: Some(50), ..Default::default() },
        ),
        rule(
            "default.impacted_over_40pct",
            "Huge blast radius (more than 40% of the repository)",
            Risk::High,
            &["run_full_suite"],
            When { impacted_files_over_percent: Some(40.0), impacted_files_over: Some(100), ..Default::default() },
        ),
        rule(
            "default.impacted_over_5pct",
            "Wide blast radius (more than 5% of the repository)",
            Risk::Medium,
            &[],
            When { impacted_files_over_percent: Some(5.0), impacted_files_over: Some(25), ..Default::default() },
        ),
        rule(
            "default.widely_used",
            "Substantial edit to one of the most depended-on files",
            Risk::Medium,
            &[],
            When { dependents_top_percent: Some(1.0), dependents_over: Some(10), changed_lines_over: Some(10), ..Default::default() },
        ),
        rule(
            "default.complex_method",
            "Edits a file with one of the most complex methods in the repository",
            Risk::Medium,
            &[],
            When { max_method_complexity_top_percent: Some(2.0), max_method_complexity_over: Some(10), changed_lines_over: Some(10), ..Default::default() },
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

/// `defaults.disable` entry → the built-in it names, if any: `Some(true)`
/// for a rule, `Some(false)` for an area.
pub fn builtin_kind(name: &str) -> Option<bool> {
    let id = if name.starts_with("default.") { name.to_string() } else { format!("default.{name}") };
    if default_rules().iter().any(|r| r.id == id) {
        Some(true)
    } else if default_areas().iter().any(|(n, _)| n == name) {
        Some(false)
    } else {
        None
    }
}
