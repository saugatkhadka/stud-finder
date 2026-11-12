//! Finding, parsing and validating `ripple.yml`.

use crate::error::{Issue, PolicyError};
use crate::policy::Policy;
use ripple_core::Config;
use std::path::{Path, PathBuf};

/// Looked up at the repo root, in this order.
pub const CONFIG_FILE_NAMES: [&str; 2] = ["ripple.yml", ".ripple.yml"];

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    /// `None` when no config file exists and defaults are in effect.
    pub path: Option<PathBuf>,
}

/// The conventional config file at `root` (ignores `--config`; used to
/// find the repository root).
pub fn find_config(root: &Path) -> Option<PathBuf> {
    CONFIG_FILE_NAMES.iter().map(|n| root.join(n)).find(|p| p.is_file())
}

static CONFIG_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// `--config PATH` (absolute): this process loads its config from `path`
/// instead of `<root>/ripple.yml`. Set once, before any config is loaded.
pub fn set_config_override(path: PathBuf) {
    let _ = CONFIG_OVERRIDE.set(path);
}

/// The `--config` path, if one was given.
pub fn config_override() -> Option<&'static Path> {
    CONFIG_OVERRIDE.get().map(PathBuf::as_path)
}

/// Is `rel` (relative to `root`) this process's config file?
pub fn is_config_file(root: &Path, rel: &str) -> bool {
    match config_override() {
        Some(p) => p == root.join(rel),
        None => CONFIG_FILE_NAMES.contains(&rel),
    }
}

/// Loads and validates the config: the `--config` file if one was given
/// (it must exist), otherwise the one at `root`. A missing file at `root`
/// yields defaults.
pub fn load_config(root: &Path) -> Result<LoadedConfig, PolicyError> {
    let Some(path) = config_override().map(Path::to_path_buf).or_else(|| find_config(root)) else {
        return Ok(LoadedConfig { config: Config::default(), path: None });
    };
    let src = std::fs::read_to_string(&path).map_err(|source| PolicyError::Io { path: path.clone(), source })?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let config = parse_config(&src, &name)?;
    Ok(LoadedConfig { config, path: Some(path) })
}

/// Parses YAML into a [`Config`] and validates it. `origin` names the source
/// in error messages (usually the file name).
pub fn parse_config(src: &str, origin: &str) -> Result<Config, PolicyError> {
    let config = parse_only(src, origin)?;
    let issues = validate(&config, Some(src));
    if issues.is_empty() { Ok(config) } else { Err(PolicyError::Invalid { origin: origin.to_string(), issues }) }
}

fn is_effectively_empty(src: &str) -> bool {
    src.lines().map(str::trim).all(|l| l.is_empty() || l.starts_with('#') || l == "---")
}

fn parse_only(src: &str, origin: &str) -> Result<Config, PolicyError> {
    if is_effectively_empty(src) {
        return Ok(Config::default());
    }
    let de = serde_yaml_ng::Deserializer::from_str(src);
    serde_path_to_error::deserialize::<_, Config>(de).map_err(|err| {
        let key = err.path().to_string();
        let inner = err.inner();
        let loc = inner.location();
        PolicyError::Parse {
            origin: origin.to_string(),
            key: Some(key.clone()),
            line: loc.as_ref().map(|l| l.line()),
            column: loc.as_ref().map(|l| l.column()),
            message: strip_key(&strip_location(&inner.to_string()), &key),
        }
    })
}

/// serde_yaml prefixes some messages with the key path; we report it separately.
fn strip_key(msg: &str, key: &str) -> String {
    // The prefix is the key itself or one of its parents (`query: unknown field ...`).
    match msg.split_once(": ") {
        Some((head, rest)) if !head.contains(' ') && key.starts_with(head) => rest.to_string(),
        _ => msg.to_string(),
    }
}

/// serde_yaml appends " at line X column Y" to messages; we report it separately.
fn strip_location(msg: &str) -> String {
    match msg.find(" at line ") {
        Some(i) => msg[..i].to_string(),
        None => msg.to_string(),
    }
}

/// Semantic validation: everything serde cannot check. Returns every issue found.
pub fn validate(config: &Config, src: Option<&str>) -> Vec<Issue> {
    let mut issues = Vec::new();
    if config.version != 1 {
        issues.push(Issue::new("version", format!("unsupported version {} (this ripple understands version 1)", config.version)));
    }
    // Compiling checks every glob, area reference and rule.
    if let Err(PolicyError::Invalid { issues: more, .. }) = Policy::compile_unchecked(config.clone()) {
        issues.extend(more);
    }
    let mut seen = std::collections::BTreeMap::new();
    for (i, rule) in config.rules.iter().enumerate() {
        if rule.id.trim().is_empty() {
            issues.push(Issue::new(format!("rules[{i}].id"), "rule id must not be empty"));
        } else if let Some(first) = seen.insert(rule.id.as_str(), i) {
            issues.push(Issue::new(format!("rules[{i}].id"), format!("duplicate rule id `{}` (also rules[{first}])", rule.id)));
        }
        let w = &rule.when;
        if w.within.is_some() && w.reaches.is_empty() && w.reaches_tier.is_none() {
            issues.push(Issue::new(format!("rules[{i}].when.within"), "`within` only applies together with `reaches` or `reaches_tier`"));
        }
        if w.within == Some(0) {
            issues.push(Issue::new(format!("rules[{i}].when.within"), "`within` must be at least 1 hop (omit it to use query.max_depth)"));
        }
        for (key, v) in [("coverage_under", w.coverage_under), ("changed_lines_coverage_under", w.changed_lines_coverage_under)] {
            if v.is_some_and(|v| !(0.0..=100.0).contains(&v)) {
                issues.push(Issue::new(format!("rules[{i}].when.{key}"), "a percentage from 0 to 100"));
            }
        }
        if w.cochange_min_confidence.is_some_and(|v| !(0.0..=1.0).contains(&v)) {
            issues.push(Issue::new(format!("rules[{i}].when.cochange_min_confidence"), "a share from 0 to 1"));
        }
        if w.cochange_min_confidence.is_some() && w.missing_cochange.is_none() {
            issues.push(Issue::new(format!("rules[{i}].when.cochange_min_confidence"), "`cochange_min_confidence` only applies together with `missing_cochange`"));
        }
        if w.uncovered_includes_unknown && w.uncovered.is_none() {
            issues.push(Issue::new(format!("rules[{i}].when.uncovered_includes_unknown"), "`uncovered_includes_unknown` only applies together with `uncovered`"));
        }
        for (j, r) in rule.require.iter().enumerate() {
            if r.trim().is_empty() {
                issues.push(Issue::new(format!("rules[{i}].require[{j}]"), "required action must not be empty"));
            }
        }
    }
    let hp = config.rank.hotspot_percent;
    if !(hp > 0.0 && hp <= 100.0) {
        issues.push(Issue::new("rank.hotspot_percent", "a percentage above 0, at most 100"));
    }
    let rw = &config.rank.weights;
    for (key, v) in [("churn", rw.churn), ("complexity", rw.complexity), ("dependents", rw.dependents), ("uncovered", rw.uncovered)] {
        if !(v.is_finite() && v >= 0.0) {
            issues.push(Issue::new(format!("rank.weights.{key}"), "a weight of 0 or more"));
        }
    }
    if rw.complexity + rw.dependents <= 0.0 {
        issues.push(Issue::new("rank.weights", "`complexity` or `dependents` must be above 0 (they are the only factors every file has)"));
    }
    for (name, area) in &config.areas {
        if area.paths.is_empty() {
            issues.push(Issue::new(format!("areas.{name}.paths"), "an area needs at least one path glob"));
        }
        if name.trim().is_empty() || looks_like_glob(name) {
            issues.push(Issue::new(format!("areas.{name}"), "area names must be plain identifiers (no `/`, `*`, `.`, ...)"));
        }
    }
    if let Some(src) = src {
        for issue in &mut issues {
            issue.line = locate(src, config, &issue.key);
        }
    }
    issues.sort_by(|a, b| (a.line, &a.key).cmp(&(b.line, &b.key)));
    issues.dedup();
    issues
}

/// True when a `touches`/`reaches` entry is a glob rather than an area name.
pub(crate) fn looks_like_glob(s: &str) -> bool {
    s.contains(['/', '*', '?', '[', '{', '.'])
}

/// Best-effort line of the YAML node a validation issue refers to: the
/// rule's `id:` line for `rules[i]...`, the area's key line for `areas.x...`,
/// the top-level key otherwise.
fn locate(src: &str, config: &Config, key: &str) -> Option<usize> {
    let find = |pred: &dyn Fn(&str) -> bool| src.lines().position(pred).map(|i| i + 1);
    if let Some(rest) = key.strip_prefix("rules[") {
        let idx: usize = rest.split(']').next()?.parse().ok()?;
        let id = &config.rules.get(idx)?.id;
        // Pick the idx-th occurrence of this id so duplicates point at the right rule.
        let nth = config.rules[..idx].iter().filter(|r| &r.id == id).count();
        let mut hits = src.lines().enumerate().filter(|(_, l)| {
            let t = l.trim_start().trim_start_matches("- ").trim_start();
            t.strip_prefix("id:").map(|v| v.trim().trim_matches(['"', '\''])) == Some(id.as_str())
        });
        return hits.nth(nth).map(|(i, _)| i + 1);
    }
    if let Some(rest) = key.strip_prefix("areas.") {
        let name = rest.split('.').next()?;
        let mut in_areas = false;
        for (i, l) in src.lines().enumerate() {
            if !l.starts_with([' ', '\t']) && !l.trim().is_empty() {
                in_areas = l.starts_with("areas:");
            } else if in_areas && l.trim_start().trim_matches(['"', '\'']).starts_with(&format!("{name}:")) {
                return Some(i + 1);
            }
        }
        return None;
    }
    let top = key.split(['.', '[']).next()?;
    find(&|l: &str| l.starts_with(&format!("{top}:")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(src: &str) -> String {
        parse_config(src, "ripple.yml").unwrap_err().to_string()
    }

    #[test]
    fn empty_and_comment_only_files_are_defaults() {
        for src in ["", "# nothing\n", "---\n"] {
            let c = parse_config(src, "ripple.yml").unwrap();
            assert_eq!(c.version, 1);
            assert!(c.rules.is_empty());
        }
    }

    #[test]
    fn missing_file_is_defaults() {
        let dir = std::env::temp_dir().join(format!("ripple-policy-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let l = load_config(&dir).unwrap();
        assert!(l.path.is_none());
        std::fs::write(dir.join(".ripple.yml"), "version: 1\n").unwrap();
        assert!(load_config(&dir).unwrap().path.unwrap().ends_with(".ripple.yml"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unknown_key_names_key_and_line() {
        let e = err("version: 1\nrules:\n  - id: a\n    risk: high\n    when:\n      touchez: [x]\n");
        assert!(e.contains("line 6"), "{e}");
        assert!(e.contains("rules[0].when"), "{e}");
        assert!(e.contains("touchez"), "{e}");
    }

    #[test]
    fn wrong_type_names_key() {
        let e = err("ci:\n  max_listed: lots\n");
        assert!(e.contains("ci.max_listed"), "{e}");
        assert!(e.contains("line 2"), "{e}");
        let e = err("areas:\n  billing:\n    paths: [a/**]\n    tier: severe\n");
        assert!(e.contains("areas.billing.tier"), "{e}");
    }

    #[test]
    fn semantic_errors_are_collected() {
        let src = "\
areas:
  billing:
    paths: [\"app/billing/[x\"]
    tier: critical
rules:
  - id: one
    risk: high
    when: { touches: [payments] }
  - id: one
    risk: low
    when: { within: 2 }
";
        let e = parse_config(src, "ripple.yml").unwrap_err();
        let PolicyError::Invalid { issues, .. } = &e else { panic!("{e}") };
        let text = e.to_string();
        assert!(text.contains("areas.billing.paths[0]") && text.contains("invalid glob"), "{text}");
        assert!(text.contains("rules[0].when.touches[0]") && text.contains("unknown area `payments`"), "{text}");
        assert!(text.contains("duplicate rule id `one`"), "{text}");
        assert!(text.contains("rules[1].when.within"), "{text}");
        let dup = issues.iter().find(|i| i.message.contains("duplicate")).unwrap();
        assert_eq!(dup.line, Some(9), "{dup:?}");
        let glob = issues.iter().find(|i| i.key == "areas.billing.paths[0]").unwrap();
        assert_eq!(glob.line, Some(2));
    }

    #[test]
    fn unsupported_version() {
        assert!(err("version: 2\n").contains("unsupported version 2"));
    }

    #[test]
    fn signal_conditions_are_checked() {
        let ok = parse_config(
            "rank: {hotspot_percent: 2.5}\nrules:\n  - id: a\n    risk: high\n    when: {churn_over: 10, complexity_over: 50, max_method_complexity_over: 15, coverage_under: 40, changed_lines_coverage_under: 60, uncovered: true, uncovered_includes_unknown: true, hotspot: true, missing_cochange: true, cochange_min_confidence: 0.4}\n",
            "ripple.yml",
        )
        .unwrap();
        assert_eq!(ok.rank.hotspot_percent, 2.5);
        let d = parse_config("", "ripple.yml").unwrap().rank;
        assert_eq!(d.hotspot_percent, 1.0);
        assert_eq!((d.weights.churn, d.weights.complexity, d.weights.dependents, d.weights.uncovered), (3.0, 1.0, 0.5, 1.0));
        let w = parse_config("rank: {weights: {churn: 1, dependents: 0}}\n", "ripple.yml").unwrap().rank.weights;
        assert_eq!((w.churn, w.complexity, w.dependents, w.uncovered), (1.0, 1.0, 0.0, 1.0));
        let e = err("rank: {weights: {churn: -1, complexity: 0, dependents: 0}}\n");
        assert!(e.contains("rank.weights.churn") && e.contains("must be above 0"), "{e}");
        assert!(err("rank: {weights: {age: 1}}\n").contains("age"));
        let e = err("rank: {hotspot_percent: 0}\nrules:\n  - {id: a, risk: high, when: {coverage_under: 120, cochange_min_confidence: 0.5, uncovered_includes_unknown: true}}\n");
        for want in ["rank.hotspot_percent", "rules[0].when.coverage_under", "rules[0].when.cochange_min_confidence", "rules[0].when.uncovered_includes_unknown"] {
            assert!(e.contains(want), "{want}: {e}");
        }
        assert!(err("rules:\n  - {id: a, risk: high, when: {missing_cochange: true, cochange_min_confidence: 2}}\n").contains("a share from 0 to 1"));
        assert!(err("rank: {hotspot: 5}\n").contains("hotspot"));
    }
}
