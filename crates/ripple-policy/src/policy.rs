//! A config with all globs compiled, plus per-graph-node classification.

use crate::defaults::default_rules;
use crate::error::{Issue, PolicyError};
use crate::glob::Patterns;
use crate::lang::Language;
use crate::load::{looks_like_glob, validate};
use ripple_core::config::Rule;
use ripple_core::{Config, Graph, NodeId, Risk};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct CompiledArea {
    pub name: String,
    pub tier: Risk,
    pub owners: Vec<String>,
    pub description: Option<String>,
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
    defaults: bool,
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
        Ok(p)
    }

    /// Compiles without the checks in [`validate`] that do not need compiled
    /// globs; still reports bad globs and unknown area references.
    pub(crate) fn compile_unchecked(config: Config) -> Result<Self, PolicyError> {
        let mut issues = Vec::new();
        let areas: Vec<CompiledArea> = config
            .areas
            .iter()
            .map(|(name, a)| CompiledArea { name: name.clone(), tier: a.tier, owners: a.owners.clone(), description: a.description.clone() })
            .collect();
        let area_globs = Patterns::build(
            config.areas.iter().enumerate().flat_map(|(ai, (name, a))| {
                a.paths.iter().enumerate().map(move |(pi, p)| (p.as_str(), ai, format!("areas.{name}.paths[{pi}]")))
            }),
            &mut issues,
        );
        let tests = Patterns::list("tests", &config.tests, &mut issues);
        let global = Patterns::list("global_files", &config.global_files, &mut issues);
        let ignore = Patterns::list("ignore", &config.ignore, &mut issues);

        let defaults = config.rules.is_empty();
        let rules_src = if defaults { default_rules() } else { config.rules.clone() };
        let mut rules = Vec::with_capacity(rules_src.len());
        for (i, rule) in rules_src.into_iter().enumerate() {
            let touches = target(&config, &rule.when.touches, &format!("rules[{i}].when.touches"), &mut issues);
            let reaches = target(&config, &rule.when.reaches, &format!("rules[{i}].when.reaches"), &mut issues);
            rules.push(CompiledRule { rule, touches, reaches });
        }
        if !issues.is_empty() {
            return Err(PolicyError::Invalid { origin: "ripple.yml".into(), issues });
        }
        Ok(Self { config, source: None, areas, area_globs, tests, global, ignore, rules, defaults })
    }

    pub fn areas(&self) -> &[CompiledArea] {
        &self.areas
    }
    pub fn area(&self, idx: u16) -> &CompiledArea {
        &self.areas[idx as usize]
    }
    /// The rules in effect (the built-in defaults when the config has none).
    pub fn rules(&self) -> impl Iterator<Item = &Rule> {
        self.rules.iter().map(|r| &r.rule)
    }
    pub fn uses_default_rules(&self) -> bool {
        self.defaults
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

fn target(config: &Config, entries: &[String], key: &str, issues: &mut Vec<Issue>) -> Target {
    let mut t = Target::default();
    let mut globs = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        if let Some(pos) = config.areas.keys().position(|k| k == e) {
            t.areas.push(pos as u16);
        } else if looks_like_glob(e) {
            globs.push((e.as_str(), i, format!("{key}[{i}]")));
        } else {
            let known: Vec<&str> = config.areas.keys().map(String::as_str).collect();
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

/// [`FileClass`] of every node in a graph, computed once per index generation
/// so queries never run glob matching on the impacted set.
#[derive(Debug, Default)]
pub struct PolicyIndex {
    classes: Vec<FileClass>,
}

impl PolicyIndex {
    pub fn build(policy: &Policy, graph: &Graph) -> Self {
        let paths = graph.paths();
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8);
        let chunk = paths.len().div_ceil(threads).max(1024);
        let classes = std::thread::scope(|s| {
            let handles: Vec<_> = paths
                .chunks(chunk)
                .map(|part| s.spawn(move || part.iter().map(|p| policy.classify(p)).collect::<Vec<_>>()))
                .collect();
            handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
        });
        Self { classes }
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
            "areas:\n  billing: {paths: [app/models/billing/**, app/services/pay*], tier: critical, owners: ['@pay']}\n  models: {paths: [app/models], tier: medium}\n",
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
        assert_eq!(p.rules().count(), 8);
    }

    #[test]
    fn explicit_rules_replace_defaults() {
        let p = policy("rules:\n  - id: big\n    risk: high\n    when: {changed_files_over: 10}\n");
        assert!(!p.uses_default_rules());
        assert_eq!(p.rules().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["big"]);
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
