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
    /// ("tests to run") and do not count toward blast-radius size.
    pub tests: Vec<String>,
    pub rules: Vec<Rule>,
    pub query: QueryConfig,
    pub ci: CiConfig,
    pub daemon: DaemonConfig,
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
            tests: ["spec/**", "test/**", "**/__tests__/**", "**/*.test.*", "**/*.spec.*", "**/*_spec.rb", "**/*_test.rb", "**/*.stories.*"]
                .map(String::from)
                .to_vec(),
            rules: Vec::new(),
            query: QueryConfig::default(),
            ci: CiConfig::default(),
            daemon: DaemonConfig::default(),
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
    /// Number of changed files is greater than this.
    pub changed_files_over: Option<usize>,
    /// Added + deleted lines is greater than this (only when a diff is available).
    pub changed_lines_over: Option<usize>,
    /// The change is in one language and the blast radius reaches the other
    /// (Ruby change impacting JS/TS or the reverse).
    pub crosses_boundary: Option<bool>,
    /// A changed file matches `global_files`.
    pub touches_global: Option<bool>,
    /// A changed non-test source file has no test file among its dependents.
    pub untested: Option<bool>,
    /// Only consider edges of at least this confidence when evaluating
    /// `reaches*` / `impacted_files_over` / `crosses_boundary` for this rule.
    pub min_confidence: Option<crate::model::Confidence>,
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
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self { idle_timeout_minutes: 30 }
    }
}
