//! `tsconfig.json` / `jsconfig.json` loading: `extends`, `baseUrl`, `paths`.
//! Everything is expressed as repo-relative paths so resolution never needs
//! the absolute location of the repo.

use crate::paths;
use serde_json::Value;
use std::path::Path;

/// One `paths` entry: `"@/*": ["app/javascript/*"]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRule {
    prefix: String,
    suffix: String,
    wildcard: bool,
    /// Repo-relative target templates (with `*` where the capture goes).
    targets: Vec<String>,
}

/// Effective compiler options of one config after following `extends`.
#[derive(Debug, Clone, Default)]
pub struct TsConfig {
    /// Repo-relative path of the config file itself.
    pub file: String,
    /// Directory containing it.
    pub dir: String,
    /// `baseUrl`, repo-relative.
    pub base_url: Option<String>,
    pub rules: Vec<PathRule>,
    /// Every file read while loading (the config and its `extends` chain).
    pub deps: Vec<String>,
    /// Problems worth a warning (unreadable / unresolvable `extends`).
    pub problems: Vec<String>,
}

#[derive(Default)]
struct Layer {
    base_url: Option<String>,
    /// `(entries, directory paths are relative to when no baseUrl)`
    paths: Option<(Vec<(String, Vec<String>)>, String)>,
}

impl TsConfig {
    pub fn load(root: &Path, file: &str) -> TsConfig {
        let dir = paths::parent(file).to_string();
        let mut cfg = TsConfig { file: file.to_string(), dir: dir.clone(), ..Default::default() };
        let mut chain = Vec::new();
        let layer = load_layer(root, file, &dir, &mut cfg, &mut chain);
        cfg.base_url = layer.base_url;
        if let Some((entries, paths_dir)) = layer.paths {
            let base = cfg.base_url.clone().unwrap_or(paths_dir);
            cfg.rules = entries
                .into_iter()
                .filter_map(|(pattern, targets)| {
                    let targets: Vec<String> = targets.iter().filter_map(|t| paths::join(&base, &expand_config_dir(t, &dir))).collect();
                    PathRule::new(&pattern, targets)
                })
                .collect();
        }
        cfg
    }

    /// Candidate repo-relative targets for `spec` via `paths`, best-matching rule only.
    /// The bool is whether the rule is specific enough to *claim* the specifier
    /// (a catch-all `"*"` pointing at `node_modules` must not).
    pub fn match_paths(&self, spec: &str) -> Option<(Vec<String>, bool)> {
        let rule = self.rules.iter().filter(|r| r.matches(spec)).max_by_key(|r| r.prefix.len() + r.suffix.len() + usize::from(!r.wildcard))?;
        let captured = if rule.wildcard { &spec[rule.prefix.len()..spec.len() - rule.suffix.len()] } else { "" };
        let targets = rule.targets.iter().map(|t| t.replacen('*', captured, 1)).collect();
        let claims = !(rule.wildcard && rule.prefix.is_empty()) && !rule.targets.iter().all(|t| t.contains("node_modules"));
        Some((targets, claims))
    }

    /// `baseUrl`-relative candidate for a non-relative specifier.
    pub fn base_url_candidate(&self, spec: &str) -> Option<String> {
        paths::join(self.base_url.as_deref()?, spec)
    }
}

impl PathRule {
    fn new(pattern: &str, targets: Vec<String>) -> Option<Self> {
        if targets.is_empty() {
            return None;
        }
        Some(match pattern.split_once('*') {
            Some((p, s)) => Self { prefix: p.to_string(), suffix: s.to_string(), wildcard: true, targets },
            None => Self { prefix: pattern.to_string(), suffix: String::new(), wildcard: false, targets },
        })
    }

    fn matches(&self, spec: &str) -> bool {
        if self.wildcard {
            spec.len() >= self.prefix.len() + self.suffix.len() && spec.starts_with(&self.prefix) && spec.ends_with(&self.suffix)
        } else {
            spec == self.prefix
        }
    }
}

fn expand_config_dir(s: &str, dir: &str) -> String {
    let d = if dir.is_empty() { "." } else { dir };
    s.replace("${configDir}", d)
}

fn load_layer(root: &Path, file: &str, final_dir: &str, cfg: &mut TsConfig, chain: &mut Vec<String>) -> Layer {
    let mut layer = Layer::default();
    if chain.iter().any(|f| f == file) || chain.len() > 16 {
        cfg.problems.push(format!("circular or too deep `extends` at {file}"));
        return layer;
    }
    chain.push(file.to_string());
    cfg.deps.push(file.to_string());
    let value = match std::fs::read_to_string(root.join(file)).map_err(|e| e.to_string()).and_then(|t| parse_jsonc(&t)) {
        Ok(v) => v,
        Err(e) => {
            cfg.problems.push(format!("cannot read {file}: {e}"));
            chain.pop();
            return layer;
        }
    };
    let dir = paths::parent(file).to_string();

    let extends: Vec<&str> = match value.get("extends") {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    for ext in extends {
        match find_extended(root, &dir, ext) {
            Some(parent) => {
                let parent_layer = load_layer(root, &parent, final_dir, cfg, chain);
                if parent_layer.base_url.is_some() {
                    layer.base_url = parent_layer.base_url;
                }
                if parent_layer.paths.is_some() {
                    layer.paths = parent_layer.paths;
                }
            }
            None => cfg.problems.push(format!("{file}: cannot resolve extends \"{ext}\" (npm package configs need node_modules)")),
        }
    }

    if let Some(opts) = value.get("compilerOptions") {
        if let Some(b) = opts.get("baseUrl").and_then(Value::as_str) {
            layer.base_url = paths::join(&dir, &expand_config_dir(b, final_dir));
        }
        if let Some(Value::Object(map)) = opts.get("paths") {
            let entries = map
                .iter()
                .map(|(k, v)| (k.clone(), v.as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()))
                .collect();
            layer.paths = Some((entries, dir.clone()));
        }
    }
    chain.pop();
    layer
}

/// Finds the file an `extends` value points at. Only relative / absolute-in-repo
/// specifiers can be followed; package configs live in `node_modules`.
fn find_extended(root: &Path, dir: &str, spec: &str) -> Option<String> {
    if !(spec.starts_with('.') || spec.starts_with('/')) {
        return None;
    }
    let base = paths::join(dir, spec)?;
    [base.clone(), format!("{base}.json"), format!("{base}/tsconfig.json")].into_iter().find(|c| root.join(c).is_file())
}

pub fn parse_jsonc(text: &str) -> Result<Value, String> {
    serde_json::from_str(&strip_jsonc(text)).map_err(|e| e.to_string())
}

/// Removes `//` and `/* */` comments and trailing commas, leaving string
/// contents untouched. Byte offsets are preserved only roughly (not needed).
pub fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_str = false;
    while let Some(c) = chars.next() {
        if in_str {
            out.push(c);
            if c == '\\' {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = '\0';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    remove_trailing_commas(&out)
}

fn remove_trailing_commas(s: &str) -> String {
    let bytes: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut in_str = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < bytes.len() {
                i += 1;
                out.push(bytes[i]);
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
            out.push(c);
        } else if c == ',' {
            let next = bytes[i + 1..].iter().find(|ch| !ch.is_whitespace());
            if !matches!(next, Some('}' | ']')) {
                out.push(c);
            }
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonc() {
        let v = parse_jsonc("{\n // c\n \"a\": \"http://x\", /* b */ \"b\": [1,2,],\n}").unwrap();
        assert_eq!(v["a"], "http://x");
        assert_eq!(v["b"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn path_matching() {
        let rules = vec![
            PathRule::new("@/*", vec!["src/*".into()]).unwrap(),
            PathRule::new("@/special/*", vec!["other/*".into()]).unwrap(),
            PathRule::new("exact", vec!["lib/exact".into()]).unwrap(),
        ];
        let cfg = TsConfig { rules, ..Default::default() };
        assert_eq!(cfg.match_paths("@/special/a").unwrap().0, vec!["other/a"]);
        assert_eq!(cfg.match_paths("@/a/b").unwrap().0, vec!["src/a/b"]);
        assert_eq!(cfg.match_paths("exact").unwrap().0, vec!["lib/exact"]);
        assert!(cfg.match_paths("nope").is_none());
    }
}
