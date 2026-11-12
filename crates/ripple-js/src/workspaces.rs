//! Monorepo workspace packages, discovered from `package.json` `workspaces`
//! globs (and `pnpm-workspace.yaml`) without `node_modules`.

use crate::paths;
use crate::vfs::FileSet;
use globset::{Glob, GlobBuilder, GlobSetBuilder};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Package {
    /// Repo-relative directory.
    pub dir: String,
    /// The `exports` field, if any.
    pub exports: Option<Value>,
}

#[derive(Debug, Default)]
pub struct Workspaces {
    pub packages: BTreeMap<String, Package>,
    /// npm dependency names declared by any `package.json` (all four dependency maps).
    pub declared: HashSet<String>,
    /// Every file read while discovering (for invalidation).
    pub deps: Vec<String>,
    pub problems: Vec<String>,
}

impl Workspaces {
    pub fn discover(root: &Path, files: &FileSet) -> Self {
        let mut out = Workspaces::default();
        let manifests: Vec<&String> = files.iter().filter(|f| paths::file_name(f) == "package.json" && !f.contains("node_modules/")).collect();
        let mut read: BTreeMap<String, Value> = BTreeMap::new();
        for m in &manifests {
            out.deps.push((*m).clone());
            match std::fs::read_to_string(root.join(m)).map_err(|e| e.to_string()).and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string())) {
                Ok(v) => {
                    read.insert(paths::parent(m).to_string(), v);
                }
                Err(e) => out.problems.push(format!("{m}: {e}")),
            }
        }

        for manifest in read.values() {
            for field in ["dependencies", "devDependencies", "peerDependencies", "optionalDependencies"] {
                if let Some(Value::Object(m)) = manifest.get(field) {
                    out.declared.extend(m.keys().cloned());
                }
            }
        }

        let mut member_dirs: Vec<String> = Vec::new();
        for (dir, manifest) in &read {
            let patterns = workspace_patterns(manifest);
            if !patterns.is_empty() {
                member_dirs.extend(match_dirs(dir, &patterns, read.keys()));
            }
        }
        for yaml in files.iter().filter(|f| paths::file_name(f) == "pnpm-workspace.yaml") {
            out.deps.push(yaml.clone());
            if let Ok(text) = std::fs::read_to_string(root.join(yaml)) {
                let patterns = pnpm_patterns(&text);
                member_dirs.extend(match_dirs(paths::parent(yaml), &patterns, read.keys()));
            }
        }
        member_dirs.sort();
        member_dirs.dedup();

        for dir in member_dirs {
            let Some(manifest) = read.get(&dir) else { continue };
            let Some(name) = manifest.get("name").and_then(Value::as_str) else { continue };
            out.packages.entry(name.to_string()).or_insert_with(|| Package { dir: dir.clone(), exports: manifest.get("exports").cloned() });
        }
        out
    }

    /// True when `spec` names an npm package some `package.json` declares as a dependency.
    pub fn is_declared_dependency(&self, spec: &str) -> bool {
        let mut parts = spec.split('/');
        let name = match parts.next() {
            Some(scope) if scope.starts_with('@') => match parts.next() {
                Some(n) => format!("{scope}/{n}"),
                None => return false,
            },
            Some(n) => n.to_string(),
            None => return false,
        };
        self.declared.contains(&name)
    }

    /// Finds the package a bare specifier names, with the remaining subpath
    /// (`"@canvas/foo/bar"` -> (`@canvas/foo`, `"bar"`)). Longest name wins.
    pub fn lookup<'s>(&self, spec: &'s str) -> Option<(&Package, &'s str)> {
        if self.packages.is_empty() {
            return None;
        }
        let mut end = spec.len();
        loop {
            let name = &spec[..end];
            if let Some(p) = self.packages.get(name) {
                return Some((p, spec[end..].trim_start_matches('/')));
            }
            end = spec[..end].rfind('/')?;
        }
    }
}

fn workspace_patterns(manifest: &Value) -> Vec<String> {
    let list = match manifest.get("workspaces") {
        Some(Value::Array(a)) => a.as_slice(),
        Some(Value::Object(o)) => o.get("packages").and_then(Value::as_array).map_or(&[][..], Vec::as_slice),
        _ => &[],
    };
    list.iter().filter_map(Value::as_str).map(str::to_string).collect()
}

/// The `packages:` list of a `pnpm-workspace.yaml` (a flat `- "glob"` list; no full YAML).
fn pnpm_patterns(text: &str) -> Vec<String> {
    let mut in_packages = false;
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = line.strip_prefix("packages:") {
            in_packages = rest.trim().is_empty();
        } else if in_packages {
            if let Some(item) = t.strip_prefix('-') {
                out.push(item.trim().trim_matches(['"', '\'']).to_string());
            } else if !t.is_empty() && !t.starts_with('#') {
                in_packages = false;
            }
        }
    }
    out
}

/// Directories among `candidates` matched by `patterns` (relative to `base`).
/// `!pattern` excludes.
fn match_dirs<'a>(base: &str, patterns: &[String], candidates: impl Iterator<Item = &'a String>) -> Vec<String> {
    let mut include = GlobSetBuilder::new();
    let mut exclude = GlobSetBuilder::new();
    for p in patterns {
        let (neg, p) = p.strip_prefix('!').map_or((false, p.as_str()), |r| (true, r));
        let p = p.trim_start_matches("./").trim_end_matches('/');
        let full = if base.is_empty() { p.to_string() } else { format!("{base}/{p}") };
        let Ok(glob) = GlobBuilder::new(&full).literal_separator(true).build().or_else(|_| Glob::new(&full)) else { continue };
        if neg { exclude.add(glob) } else { include.add(glob) };
    }
    let (Ok(include), Ok(exclude)) = (include.build(), exclude.build()) else { return Vec::new() };
    candidates.filter(|d| include.is_match(d.as_str()) && !exclude.is_match(d.as_str())).cloned().collect()
}

/// Candidate `exports` targets (relative, `./`-prefixed) for `subpath`
/// (`"."` or `"./x"`), in priority order. Conditions are matched against what a
/// bundler targeting the browser would accept.
pub fn export_targets(exports: &Value, subpath: &str) -> Vec<String> {
    const CONDITIONS: [&str; 7] = ["import", "module", "browser", "default", "types", "require", "development"];

    fn flatten(v: &Value, out: &mut Vec<String>, capture: Option<&str>) {
        match v {
            Value::String(s) => out.push(capture.map_or_else(|| s.clone(), |c| s.replacen('*', c, 1))),
            Value::Array(a) => a.iter().for_each(|x| flatten(x, out, capture)),
            Value::Object(o) => {
                for (k, x) in o {
                    if CONDITIONS.contains(&k.as_str()) {
                        flatten(x, out, capture);
                    }
                }
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    let Value::Object(map) = exports else {
        if subpath == "." {
            flatten(exports, &mut out, None);
        }
        return out;
    };
    if !map.keys().any(|k| k.starts_with('.')) {
        // Conditions object directly under `exports`: it is the "." entry.
        if subpath == "." {
            flatten(exports, &mut out, None);
        }
        return out;
    }
    if let Some(v) = map.get(subpath) {
        flatten(v, &mut out, None);
        return out;
    }
    // Subpath patterns: the longest prefix before `*` wins.
    let best = map
        .iter()
        .filter_map(|(k, v)| {
            let (pre, post) = k.split_once('*')?;
            (subpath.len() >= pre.len() + post.len() && subpath.starts_with(pre) && subpath.ends_with(post)).then_some((pre.len(), k, v, pre, post))
        })
        .max_by_key(|(len, ..)| *len);
    if let Some((_, _, v, pre, post)) = best {
        flatten(v, &mut out, Some(&subpath[pre.len()..subpath.len() - post.len()]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exports() {
        let e = json!({".": {"types": "./t.d.ts", "import": "./i.js"}, "./sub/*": "./src/*.ts"});
        assert_eq!(export_targets(&e, "."), vec!["./t.d.ts", "./i.js"]);
        assert_eq!(export_targets(&e, "./sub/x"), vec!["./src/x.ts"]);
        assert!(export_targets(&e, "./nope").is_empty());
        assert_eq!(export_targets(&json!("./a.js"), "."), vec!["./a.js"]);
    }

    #[test]
    fn lookup_longest() {
        let mut w = Workspaces::default();
        for n in ["@a/b", "@a/b/c"] {
            w.packages.insert(n.into(), Package { dir: n.into(), exports: None });
        }
        assert_eq!(w.lookup("@a/b/c/d").map(|(p, s)| (p.dir.as_str(), s)), Some(("@a/b/c", "d")));
        assert_eq!(w.lookup("@a/b/x").map(|(p, s)| (p.dir.as_str(), s)), Some(("@a/b", "x")));
        assert!(w.lookup("@z/q").is_none());
    }
}
