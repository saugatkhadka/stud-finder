//! A config with all globs compiled, plus per-graph-node classification.

use crate::defaults::{builtin_kind as default_kind, default_areas, default_rules};
use crate::error::{Issue, PolicyError};
use crate::codeowners::CodeOwners;
use crate::glob::Patterns;
use crate::lang::Language;
use crate::load::{looks_like_glob, validate};
use ripple_core::config::Rule;
use ripple_core::{Config, Graph, NodeId, Risk};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct CompiledArea {
    pub name: String,
    pub tier: Risk,
    pub owners: Vec<String>,
    pub description: Option<String>,
    /// One of the built-in areas (see [`crate::defaults::default_areas`]).
    pub builtin: bool,
    pub paths: Vec<String>,
}

/// A `touches` / `reaches` list split into area references and globs.
#[derive(Debug, Clone, Default)]
pub(crate) struct Target {
    pub areas: Vec<u16>,
    pub globs: Patterns,
}

impl Target {
    pub fn is_empty(&self) -> bool {
        self.areas.is_empty() && self.globs.is_empty()
    }
    pub fn globs_match(&self, path: &str) -> bool {
        self.globs.is_match(path)
    }
    pub fn matches(&self, path: &str, areas: &[u16]) -> bool {
        areas.iter().any(|a| self.areas.contains(a)) || self.globs.is_match(path)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CompiledRule {
    pub rule: Rule,
    pub touches: Target,
    pub reaches: Target,
    pub builtin: bool,
}

/// How the policy sees one file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileClass {
    /// Indexes into [`Policy::areas`], sorted.
    pub areas: Vec<u16>,
    pub test: bool,
    pub global: bool,
    pub language: Language,
}

/// A validated config with every glob compiled once.
#[derive(Debug, Clone)]
pub struct Policy {
    pub config: Config,
    /// The file it was loaded from, if any.
    pub source: Option<PathBuf>,
    areas: Vec<CompiledArea>,
    area_globs: Patterns,
    tests: Patterns,
    global: Patterns,
    ignore: Patterns,
    pub(crate) rules: Vec<CompiledRule>,
    codeowners: Option<CodeOwners>,
}

impl Policy {
    /// Validates and compiles `config`.
    pub fn compile(config: Config) -> Result<Self, PolicyError> {
        let issues = validate(&config, None);
        if !issues.is_empty() {
            return Err(PolicyError::Invalid { origin: "ripple.yml".into(), issues });
        }
        Self::compile_unchecked(config)
    }

    /// Loads `ripple.yml` from `root` (defaults when absent) and compiles it.
    pub fn load(root: &std::path::Path) -> Result<Self, PolicyError> {
        let loaded = crate::load::load_config(root)?;
        let mut p = Self::compile(loaded.config)?;
        p.source = loaded.path;
        if p.config.defaults.codeowners {
            p.codeowners = CodeOwners::find(root);
        }
        Ok(p)
    }

    /// Compiles without the checks in [`validate`] that do not need compiled
    /// globs; still reports bad globs and unknown area references.
    ///
    /// Merges the built-in areas and rules (see [`crate::defaults`]) into
    /// the file's own: same name / id = the file's wins; `defaults.disable`
    /// and `defaults.areas|rules: false` leave built-ins out.
    pub(crate) fn compile_unchecked(config: Config) -> Result<Self, PolicyError> {
        let mut issues = Vec::new();
        let d = &config.defaults;
        let disabled = |name: &str| {
            let short = name.strip_prefix("default.").unwrap_or(name);
            d.disable.iter().any(|x| x.strip_prefix("default.").unwrap_or(x) == short)
        };
        for (i, name) in d.disable.iter().enumerate() {
            if default_kind(name).is_none() {
                let rules: Vec<String> = default_rules().into_iter().map(|r| r.id.trim_start_matches("default.").to_string()).collect();
                let areas: Vec<String> = default_areas().into_iter().map(|(n, _)| n).collect();
                issues.push(Issue::new(
                    format!("defaults.disable[{i}]"),
                    format!("no built-in rule or area `{name}` (rules: {}; areas: {})", rules.join(", "), areas.join(", ")),
                ));
            }
        }
        // Areas: the file's, plus the built-ins it does not redefine.
        let mut merged: BTreeMap<String, (ripple_core::config::Area, bool)> = BTreeMap::new();
        if d.areas {
            for (name, a) in default_areas() {
                if !disabled(&name) {
                    merged.insert(name, (a, true));
                }
            }
        }
        for (name, a) in &config.areas {
            merged.insert(name.clone(), (a.clone(), false));
        }
        let area_names: Vec<String> = merged.keys().cloned().collect();
        let areas: Vec<CompiledArea> = merged
            .iter()
            .map(|(name, (a, builtin))| CompiledArea {
                name: name.clone(),
                tier: a.tier,
                owners: a.owners.clone(),
                description: a.description.clone(),
                builtin: *builtin,
                paths: a.paths.clone(),
            })
            .collect();
        let area_globs = Patterns::build(
            merged.iter().enumerate().flat_map(|(ai, (name, (a, builtin)))| {
                let key = if *builtin { format!("defaults.areas.{name}") } else { format!("areas.{name}") };
                a.paths.iter().enumerate().map(move |(pi, p)| (p.as_str(), ai, format!("{key}.paths[{pi}]")))
            }),
            &mut issues,
        );
        let tests = Patterns::list("tests", &config.tests, &mut issues);
        let global = Patterns::list("global_files", &config.global_files, &mut issues);
        let ignore = Patterns::list("ignore", &config.ignore, &mut issues);

        let mut rules = Vec::with_capacity(config.rules.len() + 20);
        for (i, rule) in config.rules.iter().enumerate() {
            let touches = target(&area_names, &rule.when.touches, &format!("rules[{i}].when.touches"), &mut issues);
            let reaches = target(&area_names, &rule.when.reaches, &format!("rules[{i}].when.reaches"), &mut issues);
            rules.push(CompiledRule { rule: rule.clone(), touches, reaches, builtin: false });
        }
        if d.rules {
            for rule in default_rules() {
                if disabled(&rule.id) || config.rules.iter().any(|r| r.id == rule.id) {
                    continue;
                }
                // A built-in rule about a built-in area goes away with it.
                if rule.when.touches.iter().chain(&rule.when.reaches).any(|a| !looks_like_glob(a) && !area_names.contains(a)) {
                    continue;
                }
                let key = format!("defaults.rules.{}", rule.id);
                let touches = target(&area_names, &rule.when.touches, &key, &mut issues);
                let reaches = target(&area_names, &rule.when.reaches, &key, &mut issues);
                rules.push(CompiledRule { rule, touches, reaches, builtin: true });
            }
        }
        if !issues.is_empty() {
            return Err(PolicyError::Invalid { origin: "ripple.yml".into(), issues });
        }
        Ok(Self { config, source: None, areas, area_globs, tests, global, ignore, rules, codeowners: None })
    }

    pub fn areas(&self) -> &[CompiledArea] {
        &self.areas
    }
    pub fn area(&self, idx: u16) -> &CompiledArea {
        &self.areas[idx as usize]
    }
    /// The rules in effect: the file's, then the built-ins it keeps.
    pub fn rules(&self) -> impl Iterator<Item = &Rule> {
        self.rules.iter().map(|r| &r.rule)
    }
    /// The rules in effect with a flag: true for a built-in rule.
    pub fn rules_with_origin(&self) -> impl Iterator<Item = (&Rule, bool)> {
        self.rules.iter().map(|r| (&r.rule, r.builtin))
    }
    /// True when every rule in effect is built-in (`ripple.yml` defines none).
    pub fn uses_default_rules(&self) -> bool {
        self.rules.iter().all(|r| r.builtin)
    }
    /// The CODEOWNERS file in effect, if one was found.
    pub fn codeowners(&self) -> Option<&CodeOwners> {
        self.codeowners.as_ref()
    }
    /// CODEOWNERS owners of `path` (the last matching line wins); empty
    /// without a CODEOWNERS file or `defaults.codeowners: false`.
    pub fn owners_of(&self, path: &str) -> &[String] {
        self.codeowners.as_ref().map_or(&[], |c| c.owners_of(path))
    }

    pub fn is_ignored(&self, path: &str) -> bool {
        self.ignore.is_match(path)
    }
    pub fn is_test(&self, path: &str) -> bool {
        self.tests.is_match(path)
    }
    pub fn is_global(&self, path: &str) -> bool {
        self.global.is_match(path)
    }
    pub fn areas_of(&self, path: &str) -> Vec<u16> {
        let mut scratch = Vec::new();
        self.area_globs.owners_matching(path, &mut scratch).into_iter().map(|i| i as u16).collect()
    }
    pub fn area_names(&self, areas: &[u16]) -> Vec<String> {
        areas.iter().map(|&a| self.area(a).name.clone()).collect()
    }

    pub fn classify(&self, path: &str) -> FileClass {
        FileClass { areas: self.areas_of(path), test: self.is_test(path), global: self.is_global(path), language: Language::of(path) }
    }

    /// Highest tier among `areas`, if any.
    pub fn max_tier(&self, areas: &[u16]) -> Option<Risk> {
        areas.iter().map(|&a| self.area(a).tier).max()
    }

    /// True when anything that affects extraction (as opposed to only
    /// evaluation) differs between the two configs.
    pub fn extraction_changed(a: &Config, b: &Config) -> bool {
        let key = |c: &Config| serde_json::to_string(&(&c.ignore, &c.ruby, &c.js, &c.boundary)).unwrap_or_default();
        key(a) != key(b)
    }
}

fn target(area_names: &[String], entries: &[String], key: &str, issues: &mut Vec<Issue>) -> Target {
    let mut t = Target::default();
    let mut globs = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        if let Some(pos) = area_names.iter().position(|k| k == e) {
            t.areas.push(pos as u16);
        } else if looks_like_glob(e) {
            globs.push((e.as_str(), i, format!("{key}[{i}]")));
        } else {
            let known: Vec<&str> = area_names.iter().map(String::as_str).collect();
            let known = if known.is_empty() { "none defined".to_string() } else { known.join(", ") };
            issues.push(Issue::new(
                format!("{key}[{i}]"),
                format!("unknown area `{e}` (known areas: {known}); globs must contain `/`, `*` or `.`"),
            ));
        }
    }
    t.areas.sort_unstable();
    t.areas.dedup();
    t.globs = Patterns::build(globs, issues);
    t
}

/// The repository's non-test source files, sorted by one metric: what the
/// `*_top_percent` conditions compare against.
#[derive(Debug, Default, Clone)]
pub struct Distribution {
    sorted: Vec<u32>,
}

impl Distribution {
    pub fn new(mut values: Vec<u32>) -> Self {
        values.sort_unstable();
        Self { sorted: values }
    }
    pub fn len(&self) -> usize {
        self.sorted.len()
    }
    pub fn is_empty(&self) -> bool {
        self.sorted.is_empty()
    }
    /// 1-based rank of `v` from the top (ties share the best rank).
    pub fn rank(&self, v: u32) -> usize {
        self.sorted.len() - self.sorted.partition_point(|&x| x <= v) + 1
    }
    /// Is `v` in the top `pct` percent? Never for 0 (a file without
    /// methods, dependents, ...) or an empty distribution.
    pub fn in_top(&self, v: u32, pct: f64) -> bool {
        v > 0 && !self.sorted.is_empty() && (self.rank(v) as f64) <= (self.sorted.len() as f64 * pct / 100.0).ceil()
    }
    /// The smallest value in the top `pct` percent, if any.
    pub fn threshold(&self, pct: f64) -> Option<u32> {
        let n = self.sorted.len();
        let k = ((n as f64 * pct / 100.0).ceil() as usize).min(n);
        if k == 0 {
            return None;
        }
        let t = self.sorted[n - k];
        // Ties below the cut-off share its rank only when they equal it.
        Some(t).filter(|&t| t > 0 && self.in_top(t, pct))
    }
}

/// [`FileClass`] of every node in a graph, computed once per index generation
/// so queries never run glob matching on the impacted set; plus the
/// distributions the relative (`*_top_percent`, `*_over_percent`)
/// conditions use.
#[derive(Debug, Default)]
pub struct PolicyIndex {
    classes: Vec<FileClass>,
    pub complexity: Distribution,
    pub max_method: Distribution,
    pub dependents: Distribution,
    /// Non-test files in the graph: the base of `impacted_files_over_percent`.
    pub non_test_files: usize,
}

impl PolicyIndex {
    pub fn build(policy: &Policy, graph: &Graph) -> Self {
        let n = graph.node_count();
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8);
        let chunk = n.div_ceil(threads).max(1024);
        let parts: Vec<(Vec<FileClass>, Vec<u32>, Vec<u32>, Vec<u32>, usize)> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..n)
                .step_by(chunk)
                .map(|a| {
                    s.spawn(move || {
                        let (mut cx, mut mm, mut fi, mut non_test) = (Vec::new(), Vec::new(), Vec::new(), 0);
                        let classes: Vec<FileClass> = (a..(a + chunk).min(n))
                            .map(|i| {
                                let node = i as NodeId;
                                let c = policy.classify(graph.path(node));
                                if !c.test {
                                    non_test += 1;
                                    if c.language.is_source() {
                                        fi.push(graph.fan_in(node) as u32);
                                        if let Some(m) = graph.metrics(node) {
                                            cx.push(m.complexity);
                                            mm.push(u32::from(m.max_method));
                                        }
                                    }
                                }
                                c
                            })
                            .collect();
                        (classes, cx, mm, fi, non_test)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
        });
        let mut out = Self::default();
        let (mut cx, mut mm, mut fi) = (Vec::new(), Vec::new(), Vec::new());
        for (classes, c, m, f, n) in parts {
            out.classes.extend(classes);
            cx.extend(c);
            mm.extend(m);
            fi.extend(f);
            out.non_test_files += n;
        }
        out.complexity = Distribution::new(cx);
        out.max_method = Distribution::new(mm);
        out.dependents = Distribution::new(fi);
        out
    }

    pub fn get(&self, node: NodeId) -> Option<&FileClass> {
        self.classes.get(node as usize)
    }

    pub fn len(&self) -> usize {
        self.classes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.classes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load::parse_config;

    fn policy(src: &str) -> Policy {
        Policy::compile(parse_config(src, "t").unwrap()).unwrap()
    }

    #[test]
    fn file_to_areas() {
        let p = policy(
            "defaults: {areas: false}\nareas:\n  billing: {paths: [app/models/billing/**, app/services/pay*], tier: critical, owners: ['@pay']}\n  models: {paths: [app/models], tier: medium}\n",
        );
        let names = |path: &str| p.area_names(&p.areas_of(path));
        assert_eq!(names("app/models/billing/invoice.rb"), ["billing", "models"]);
        assert_eq!(names("app/services/payment.rb"), ["billing"]);
        assert_eq!(names("app/models/user.rb"), ["models"]);
        assert!(names("lib/x.rb").is_empty());
        assert_eq!(p.max_tier(&p.areas_of("app/models/billing/a.rb")), Some(Risk::Critical));
    }

    #[test]
    fn test_global_ignore_classification_uses_defaults() {
        let p = policy("");
        assert!(p.is_test("spec/models/user_spec.rb"));
        assert!(p.is_test("app/javascript/__tests__/a.js"));
        assert!(p.is_test("app/javascript/a.test.tsx"));
        assert!(!p.is_test("app/models/user.rb"));
        assert!(p.is_global("config/initializers/stripe.rb"));
        assert!(p.is_global("Gemfile.lock"));
        assert!(!p.is_global("config/routes.rb"));
        assert!(p.is_ignored("node_modules/react/index.js"));
        assert!(!p.is_ignored("app/x.js"));
        assert!(p.uses_default_rules());
        assert_eq!(p.rules().count(), crate::defaults::default_rules().len());
        assert_eq!(p.areas().len(), crate::defaults::default_areas().len());
    }

    #[test]
    fn explicit_rules_extend_or_replace_defaults() {
        let n = crate::defaults::default_rules().len();
        let p = policy("rules:\n  - id: big\n    risk: high\n    when: {changed_files_over: 10}\n");
        assert!(!p.uses_default_rules());
        assert_eq!(p.rules().count(), n + 1);
        assert_eq!(p.rules().next().unwrap().id, "big");
        // Same id: the file's rule replaces the built-in one.
        let p = policy("rules:\n  - id: default.complex_file\n    risk: low\n    when: {complexity_over: 10}\n");
        assert_eq!(p.rules().count(), n);
        assert_eq!(p.rules().filter(|r| r.id == "default.complex_file").map(|r| r.risk).collect::<Vec<_>>(), [Risk::Low]);
        // `rules: false` drops them all; `disable` drops some, with or without the prefix.
        let p = policy("defaults: {rules: false}\nrules:\n  - id: big\n    risk: high\n    when: {changed_files_over: 10}\n");
        assert_eq!(p.rules().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["big"]);
        let p = policy("defaults: {disable: [complex_file, default.widely_used, migrations]}\n");
        let ids: Vec<&str> = p.rules().map(|r| r.id.as_str()).collect();
        assert!(!ids.contains(&"default.complex_file") && !ids.contains(&"default.widely_used"));
        // The schema rule goes away with its area.
        assert!(!ids.contains(&"default.schema_change"));
        assert!(p.areas().iter().all(|a| a.name != "migrations"));
        // Unknown names are errors.
        assert!(parse_config("defaults: {disable: [nope]}\n", "t").is_err());
    }

    #[test]
    fn builtin_areas_match_where_they_exist() {
        let p = policy("");
        let names = |path: &str| p.area_names(&p.areas_of(path));
        assert_eq!(names("app/business/payments/charge.rb"), ["payments"]);
        assert_eq!(names("app/javascript/components/Checkout/Form.tsx"), ["payments", "shared_components"]);
        assert_eq!(names("app/controllers/sessions_controller.rb"), ["auth"]);
        assert_eq!(names("app/models/user.rb"), ["auth"]);
        assert_eq!(names("db/migrate/20240101_add_x.rb"), ["migrations"]);
        assert_eq!(names("app/controllers/api/v1/statuses_controller.rb"), ["api"]);
        assert_eq!(names("app/workers/foo_worker.rb"), ["jobs"]);
        assert_eq!(names("app/mailers/user_mailer.rb"), ["mailers"]);
        assert!(names("app/models/status.rb").is_empty());
        // A file's own area of the same name replaces the built-in one.
        let p = policy("areas:\n  payments: {paths: [app/money/**], tier: critical}\n");
        assert_eq!(p.area_names(&p.areas_of("app/money/x.rb")), ["payments"]);
        assert!(p.areas_of("app/business/payments/charge.rb").is_empty());
        assert!(!p.areas().iter().find(|a| a.name == "payments").unwrap().builtin);
    }

    #[test]
    fn distribution_top_percent() {
        let d = Distribution::new((1..=100).collect());
        assert!(d.in_top(100, 1.0) && !d.in_top(99, 1.0));
        assert!(d.in_top(99, 2.0) && !d.in_top(98, 2.0));
        assert_eq!(d.rank(100), 1);
        assert_eq!(d.threshold(2.0), Some(99));
        assert!(!d.in_top(0, 100.0));
        assert!(!Distribution::default().in_top(5, 50.0));
    }

    #[test]
    fn extraction_relevant_change_detection() {
        let a = parse_config("ci: {max_listed: 3}\n", "t").unwrap();
        let b = parse_config("ci: {max_listed: 9}\nareas: {x: {paths: [a/**], tier: low}}\n", "t").unwrap();
        let c = parse_config("ignore: [vendor/**]\n", "t").unwrap();
        assert!(!Policy::extraction_changed(&a, &b));
        assert!(Policy::extraction_changed(&a, &c));
    }
}
