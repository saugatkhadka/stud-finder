//! `ripple.yml` schema. Parsing from YAML lives in `ripple-policy`; these are
//! plain serde structs so every crate can read them. Every field has a default
//! so an empty or missing file is valid.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    /// Globs (gitignore-style, repo-relative) never analysed and never reported.
    pub ignore: Vec<String>,
    pub ruby: RubyConfig,
    pub js: JsConfig,
    pub boundary: BoundaryConfig,
    /// Named parts of the codebase with a criticality tier.
    pub areas: BTreeMap<String, Area>,
    /// Files whose change is treated as affecting the entire app, because no
    /// static graph can see their consumers (initializers, lockfiles, ...).
    pub global_files: Vec<String>,
    /// Globs identifying test files. Tests are reported separately
    /// ("tests that depend on it") and do not count toward blast-radius size.
    pub tests: Vec<String>,
    pub rules: Vec<Rule>,
    pub query: QueryConfig,
    pub ci: CiConfig,
    pub daemon: DaemonConfig,
    /// Co-change statistics from git history (`ripple-history`): a separate
    /// "usually changes with" signal, never graph edges, never risk input.
    pub history: HistoryConfig,
    /// Existing test-coverage reports (`ripple-coverage`): read, never
    /// produced. A separate signal, never risk input by itself.
    pub coverage: CoverageConfig,
    /// `ripple rank` and the hotspot signal.
    pub rank: RankConfig,
    /// The built-in areas, rules and CODEOWNERS lookup that apply on top
    /// of this file (see `ripple config show`).
    pub defaults: DefaultsConfig,
}

/// `defaults:` in `ripple.yml`. Merge semantics: the built-in areas and
/// rules apply in addition to the file's own; an area or rule of the file
/// with the same name / id replaces the built-in one; `disable` removes
/// built-ins by name; `areas: false` / `rules: false` drop them all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DefaultsConfig {
    /// Use the built-in areas (payments, auth, migrations, ...), each only
    /// where its paths exist.
    pub areas: bool,
    /// Use the built-in `default.*` rules.
    pub rules: bool,
    /// Read review owners from CODEOWNERS (`.github/`, root, `docs/`, `.gitlab/`).
    pub codeowners: bool,
    /// Built-in rule ids (`default.complex_file` or `complex_file`) and
    /// built-in area names to leave out.
    pub disable: Vec<String>,
}

impl Default for DefaultsConfig {
    fn default() -> Self {
        Self { areas: true, rules: true, codeowners: true, disable: Vec::new() }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            ignore: ["node_modules/**", "vendor/**", "tmp/**", "log/**", "public/**", "coverage/**", ".git/**", "dist/**", "build/**"]
                .map(String::from)
                .to_vec(),
            ruby: RubyConfig::default(),
            js: JsConfig::default(),
            boundary: BoundaryConfig::default(),
            areas: BTreeMap::new(),
            global_files: [
                "config/initializers/**",
                "config/application.rb",
                "config/environment.rb",
                "config/environments/**",
                "config/boot.rb",
                "Gemfile",
                "Gemfile.lock",
                "package.json",
                "yarn.lock",
                "package-lock.json",
                "pnpm-lock.yaml",
            ]
            .map(String::from)
            .to_vec(),
            // RSpec, Minitest, Jest, Vitest, Cypress, Storybook; `spec/` and
            // `test/` also inside engines and `ee/`.
            tests: [
                "spec/**",
                "test/**",
                "**/spec/**",
                "**/test/**",
                "**/__tests__/**",
                "**/__mocks__/**",
                "**/*.test.*",
                "**/*.spec.*",
                "**/*.cy.*",
                "**/*_spec.rb",
                "**/*_test.rb",
                "**/*.stories.*",
                "cypress/**",
            ]
                .map(String::from)
                .to_vec(),
            rules: Vec::new(),
            query: QueryConfig::default(),
            ci: CiConfig::default(),
            daemon: DaemonConfig::default(),
            history: HistoryConfig::default(),
            coverage: CoverageConfig::default(),
            rank: RankConfig::default(),
            defaults: DefaultsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RubyConfig {
    pub enabled: bool,
    /// A constant defined in more files than this is a shared namespace
    /// (`module Gitlab` reopened everywhere). References to it then resolve
    /// only to its Zeitwerk-canonical file, if one exists, instead of to
    /// every file that reopens it.
    pub namespace_definition_limit: usize,
    /// Extra autoload roots beyond the Rails defaults (`app/*`, `lib`).
    pub autoload_roots: Vec<String>,
    /// Narrow a change confined to some methods: report method scope ranges
    /// (so a diff maps onto changed methods) and the method names each file
    /// mentions, and keep a direct dependent of such a change only if it
    /// mentions one of the affected names (by name: Ruby has no static
    /// types; conservative fallbacks for callbacks, implicit methods and
    /// dynamic dispatch). Beyond the first hop impact is file-level. Set to
    /// false for plain file-level analysis.
    pub method_level: bool,
}

impl Default for RubyConfig {
    fn default() -> Self {
        Self { enabled: true, namespace_definition_limit: 3, autoload_roots: Vec::new(), method_level: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JsConfig {
    pub enabled: bool,
    /// tsconfig/jsconfig files to read path aliases from. Empty = auto-detect
    /// (`tsconfig.json`, `jsconfig.json` at the root and in workspaces).
    pub tsconfig: Vec<String>,
    /// Extra import aliases: specifier prefix -> repo-relative directory.
    /// For bundler aliases that tsconfig does not declare.
    pub aliases: BTreeMap<String, String>,
}

impl Default for JsConfig {
    fn default() -> Self {
        Self { enabled: true, tsconfig: Vec::new(), aliases: BTreeMap::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BoundaryConfig {
    pub enabled: bool,
    /// Route definition files (globs).
    pub routes: Vec<String>,
    /// Callee names whose string/template argument is treated as a URL, in
    /// addition to the built-ins (`fetch`, `axios[.verb]`, `$.ajax`, `request`).
    /// Dotted and call-chain forms allowed: `api`, `api().get`, `http.post`.
    pub http_callees: Vec<String>,
    /// Only URLs starting with one of these are matched against routes.
    /// Empty = any URL starting with `/`.
    pub url_prefixes: Vec<String>,
    /// Base paths prepended to call-site URLs written without a leading slash
    /// (an API client configured with a base URL: `apiRequest("GET", "v1/foo")`
    /// with base `/api/`). Each base is tried in order.
    pub url_bases: Vec<String>,
    /// Also create edges for plain navigation links (`href={Routes.x_path()}`).
    /// Off by default: a link to a page is not a dependency on its controller.
    pub link_edges: bool,
}

impl Default for BoundaryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            routes: vec!["config/routes.rb".into(), "config/routes/**/*.rb".into()],
            http_callees: Vec::new(),
            url_prefixes: Vec::new(),
            url_bases: vec!["/".into(), "/api/".into()],
            link_edges: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Area {
    /// Globs of files belonging to this area.
    pub paths: Vec<String>,
    /// How bad it is to break this area.
    pub tier: Risk,
    /// Handles or teams to request review from (`@payments`, `alice`).
    #[serde(default)]
    pub owners: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// One policy rule. All conditions present in `when` must hold (AND).
/// The final risk of a change is the maximum `risk` over all rules that fire;
/// `require` actions are unioned.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    #[serde(default)]
    pub description: Option<String>,
    pub when: When,
    pub risk: Risk,
    /// Free-form required actions surfaced in the report and CI output,
    /// e.g. `human_review`, `security_review`, `run_full_suite`.
    #[serde(default)]
    pub require: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct When {
    /// A changed file is in one of these areas (by name) or matches one of these globs.
    pub touches: Vec<String>,
    /// A changed file is in any area of at least this tier.
    pub touches_tier: Option<Risk>,
    /// The blast radius includes a file in one of these areas (by name) or matching these globs...
    pub reaches: Vec<String>,
    /// ...or in any area of at least this tier...
    pub reaches_tier: Option<Risk>,
    /// ...within this many hops (default: `query.max_depth`).
    pub within: Option<u32>,
    /// Number of impacted non-test files (excluding the changed files) is greater than this.
    pub impacted_files_over: Option<usize>,
    /// Impacted non-test files are more than this share (percent) of the
    /// repository's non-test source files.
    pub impacted_files_over_percent: Option<f64>,
    /// Number of changed files is greater than this.
    pub changed_files_over: Option<usize>,
    /// Added + deleted lines is greater than this (only when a diff is
    /// available). Next to a per-file signal condition or `untested: true`:
    /// the lines of the same changed file that satisfies them.
    pub changed_lines_over: Option<usize>,
    /// The change is in one language and the blast radius reaches the other
    /// (Ruby change impacting JS/TS or the reverse).
    pub crosses_boundary: Option<bool>,
    /// A changed file matches `global_files`.
    pub touches_global: Option<bool>,
    /// A changed non-test source file has no test file among its dependents
    /// (with `true` next to a per-file condition: the same file).
    pub untested: Option<bool>,
    /// Only consider edges of at least this confidence when evaluating
    /// `reaches*` / `impacted_files_over` / `crosses_boundary` for this rule.
    pub min_confidence: Option<crate::model::Confidence>,
    // Signal conditions. Each looks at the changed, non-deleted, non-test
    // source files; a rule whose signal is not ready never fires (and says so).
    // All the per-file ones of a rule (and `untested: true`,
    // `changed_lines_over` next to them) must hold on the same file.
    /// A changed file was changed in more than this many commits in the
    /// last 90 days of the history window (git history).
    pub churn_over: Option<u32>,
    /// A changed file's complexity (sum over its methods plus top level) is greater than this.
    pub complexity_over: Option<u32>,
    /// A changed file's complexity is in this top share (percent) of the
    /// repository's non-test source files: relative to the repo, so one
    /// rule fits large and small codebases.
    pub complexity_top_percent: Option<f64>,
    /// A changed file has a method more complex than this.
    pub max_method_complexity_over: Option<u32>,
    /// A changed file's most complex method is in this top share (percent)
    /// of the repository's non-test source files.
    pub max_method_complexity_top_percent: Option<f64>,
    /// A changed file has more direct dependents (distinct files) than this.
    pub dependents_over: Option<u32>,
    /// A changed file's number of direct dependents (distinct files) is in
    /// this top share (percent) of the repository's non-test source files.
    pub dependents_top_percent: Option<f64>,
    /// A changed file's line coverage is below this percentage. Files with
    /// unknown coverage (in no report) never satisfy it.
    pub coverage_under: Option<f64>,
    /// The changed lines of a file (diff hunks) are covered below this
    /// percentage. Needs a diff and files with known coverage.
    pub changed_lines_coverage_under: Option<f64>,
    /// `true`: a changed file has known 0% coverage (and, with
    /// `uncovered_includes_unknown`, also one that is in no report).
    pub uncovered: Option<bool>,
    /// Lets `uncovered` count files with unknown coverage.
    pub uncovered_includes_unknown: bool,
    /// A changed file is in the top `rank.hotspot_percent` of `ripple rank`.
    pub hotspot: Option<bool>,
    /// A file that usually changes with the changed files (co-change share
    /// at least `cochange_min_confidence`) is not in the diff.
    pub missing_cochange: Option<bool>,
    /// For `missing_cochange` (0..1; default 0.5). Partners below
    /// `history.min_confidence` are never found.
    pub cochange_min_confidence: Option<f32>,
}

/// Default of [`When::cochange_min_confidence`].
pub const DEFAULT_COCHANGE_MIN_CONFIDENCE: f32 = 0.5;

/// `rank:` in `ripple.yml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RankConfig {
    /// A file is a hotspot when it is in this top share of `ripple rank`
    /// (percent of the ranked files).
    pub hotspot_percent: f64,
    /// Weights of the score's factors.
    pub weights: RankWeights,
}

impl Default for RankConfig {
    fn default() -> Self {
        Self { hotspot_percent: 1.0, weights: RankWeights::default() }
    }
}

/// `rank.weights` in `ripple.yml`: how much each 0..1 factor counts in the
/// score of `ripple rank`. Defaults from a replay of 301 commits each of two
/// Rails apps, where churn was the strongest predictor of later fixes and
/// the number of dependents the weakest.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RankWeights {
    pub churn: f64,
    pub complexity: f64,
    pub dependents: f64,
    pub uncovered: f64,
}

impl Default for RankWeights {
    fn default() -> Self {
        Self { churn: 3.0, complexity: 1.0, dependents: 0.5, uncovered: 1.0 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueryConfig {
    /// Maximum hops walked from a changed file. 0 = unlimited.
    ///
    /// Defaults to 3. A file-level graph of a Rails app is densely connected:
    /// measured on Mastodon, the unlimited closure of a typical source file is
    /// ~630 of ~800 reachable non-test files (it says "everything"), while
    /// 3 hops gives a median of 9 and a 90th percentile of 192.
    pub max_depth: u32,
    /// Edges below this confidence are ignored unless a rule/CLI flag overrides.
    pub min_confidence: crate::model::Confidence,
}

impl Default for QueryConfig {
    fn default() -> Self {
        Self { max_depth: 3, min_confidence: crate::model::Confidence::Heuristic }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CiConfig {
    /// `ripple check` exits non-zero when the change's risk is at or above this. `None` = never fail.
    pub fail_on: Option<Risk>,
    /// Emit GitHub Actions annotations / step summary when running under Actions.
    pub github: bool,
    /// Cap on impacted files listed in markdown output (JSON is never truncated).
    pub max_listed: usize,
}

impl Default for CiConfig {
    fn default() -> Self {
        Self { fail_on: None, github: true, max_listed: 50 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    /// The daemon exits after this many minutes without a request (the next
    /// command starts a new one). 0 = never.
    pub idle_timeout_minutes: u64,
    /// How long a query waits for pending changes to be indexed before it
    /// answers from the current index, marked `stale` with the pending paths.
    /// `--sync` waits without limit, `--no-wait` not at all.
    pub max_wait_ms: u64,
    /// After this many seconds without requests or file changes the daemon
    /// writes its index to disk and continues from a memory-mapped copy,
    /// releasing the in-memory indexer state (a relevant edit wakes it with
    /// a full re-index). 0 = never.
    pub sleep_after_seconds: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self { idle_timeout_minutes: 30, max_wait_ms: DEFAULT_MAX_WAIT_MS, sleep_after_seconds: 300 }
    }
}

/// `history:` in `ripple.yml`. Defaults come from the replay evaluation
/// (`ripple-eval`), see the `ripple-history` crate docs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HistoryConfig {
    pub enabled: bool,
    /// Window: the last N non-merge commits. 0 = all.
    pub max_commits: usize,
    /// Window: also only commits younger than this many days.
    pub max_age_days: Option<u32>,
    /// Commits touching more files than this are sweeps and do not count
    /// toward co-change (formatting, dependency bumps, translation imports).
    pub max_files_per_commit: usize,
    /// A partner must have changed together with the file at least this often.
    pub min_together: u32,
    /// ... in at least this share of the file's commits.
    pub min_confidence: f32,
    /// ... and at least this many times more often than its own base rate.
    pub min_lift: f32,
    /// Partners shown per query.
    pub limit: usize,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self { enabled: true, max_commits: 20000, max_age_days: None, max_files_per_commit: 30, min_together: 3, min_confidence: 0.3, min_lift: 2.0, limit: 5 }
    }
}

/// `coverage:` in `ripple.yml`: where to find test-coverage reports that
/// something else (CI, a local test run) already wrote. See the
/// `ripple-coverage` crate docs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CoverageConfig {
    pub enabled: bool,
    /// Report files or directories, repo-relative. A `*` wildcard is allowed
    /// in the last component (`coverage/lcov/*.lcov`). A directory is
    /// searched for the default report names. Empty = auto-detect in
    /// `coverage/`, `tmp/coverage/`, `spec/coverage/`, `test/coverage/`,
    /// `frontend/`, `client/`, `web/`, `app/javascript/` and
    /// `app/frontend/` `coverage/`: `.resultset.json` (SimpleCov), `lcov.info`,
    /// `lcov/*.lcov`, `coverage-final.json` (Istanbul; only when there is no
    /// `lcov.info` next to it).
    pub paths: Vec<String>,
    /// Reports measured longer ago than this are labelled stale (still used).
    pub max_age_days: Option<u32>,
}

impl Default for CoverageConfig {
    fn default() -> Self {
        Self { enabled: true, paths: Vec::new(), max_age_days: None }
    }
}

/// Default of [`DaemonConfig::max_wait_ms`].
pub const DEFAULT_MAX_WAIT_MS: u64 = 150;
