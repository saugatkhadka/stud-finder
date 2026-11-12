//! Resolving a mount's name (`"Pages/Show"`, `'application.ts'`, `:gradebook`)
//! to JS files in the workspace, by bundler configuration where the repo has
//! one and by convention otherwise.

use super::templates::{MountKind, MountRef};
use regex::Regex;
use ripple_core::{Confidence, RelPath, Workspace};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;

pub const JS_EXTS: &[&str] = &["js", "jsx", "ts", "tsx", "mjs", "cjs", "vue", "svelte", "coffee"];

pub fn is_js(path: &str) -> bool {
    path.rsplit_once('.').is_some_and(|(_, ext)| JS_EXTS.contains(&ext))
}

pub fn is_test_like(path: &str) -> bool {
    path.contains("__tests__/") || path.contains("/spec/") || path.starts_with("spec/") || path.contains("/test/") || path.starts_with("test/") || [".test.", ".spec.", ".stories."].iter().any(|m| path.contains(m))
}

/// The outcome of resolving one mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Resolved(Vec<RelPath>, Confidence),
    /// Nothing in the repo to point at (`vite_client_tag`, a CDN URL).
    NoTarget,
    /// The name is not a literal.
    Dynamic,
    Unresolved,
    /// More than three candidate files.
    Ambiguous(usize),
}

/// Bundler settings read from the repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundlerConfig {
    /// Vite Ruby: (sourceCodeDir, sourceCodeDir/entrypointsDir), from `config/vite.json`.
    pub vite: Option<(String, String)>,
    /// Shakapacker/Webpacker: (source_path, source_path/source_entry_path).
    pub packs: (String, String),
    pub packs_configured: bool,
    /// Import alias prefix -> directory (tsconfig `paths` + `js.aliases`).
    pub aliases: Vec<(String, String)>,
}

/// Lenient JSON (tsconfig allows comments and trailing commas).
fn parse_jsonc(text: &str) -> Option<serde_json::Value> {
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
        match (c, chars.peek()) {
            ('"', _) => {
                in_str = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut prev = ' ';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
            }
            _ => out.push(c),
        }
    }
    static TRAILING: OnceLock<Regex> = OnceLock::new();
    let re = TRAILING.get_or_init(|| Regex::new(r",(\s*[}\]])").expect("regex"));
    serde_json::from_str(&re.replace_all(&out, "$1")).ok()
}

fn yaml_key(text: &str, key: &str) -> Option<String> {
    // First occurrence wins: the `default:` block comes first in these files.
    let re = Regex::new(&format!(r"(?m)^\s*{key}:\s*['\x22]?([^'\x22#\s]+)")).ok()?;
    re.captures(text).and_then(|c| c.get(1)).map(|m| m.as_str().trim_matches('/').to_string())
}

fn join_dir(a: &str, b: &str) -> String {
    match (a.trim_matches('/'), b.trim_matches('/')) {
        ("", b) => b.to_string(),
        (a, "") => a.to_string(),
        (a, b) => format!("{a}/{b}"),
    }
}

impl BundlerConfig {
    pub fn read(ws: &Workspace) -> Self {
        let read = |p: &str| std::fs::read_to_string(ws.abs(p)).ok();
        let vite = read("config/vite.json").and_then(|t| parse_jsonc(&t)).map(|v| {
            let all = &v["all"];
            let src = all["sourceCodeDir"].as_str().unwrap_or("app/frontend").trim_matches('/').to_string();
            let entry = all["entrypointsDir"].as_str().unwrap_or("entrypoints");
            let entry_dir = join_dir(&src, entry);
            (src, entry_dir)
        });
        let packer = read("config/shakapacker.yml").or_else(|| read("config/webpacker.yml"));
        let packs_configured = packer.is_some();
        let packs = {
            let src = packer.as_deref().and_then(|t| yaml_key(t, "source_path")).unwrap_or_else(|| "app/javascript".into());
            let entry = packer.as_deref().and_then(|t| yaml_key(t, "source_entry_path")).unwrap_or_else(|| "packs".into());
            let entry_dir = join_dir(&src, &entry);
            (src, entry_dir)
        };
        let mut aliases: Vec<(String, String)> = ws.config.js.aliases.iter().map(|(k, v)| (k.clone(), v.trim_end_matches('/').to_string())).collect();
        for ts in ["tsconfig.json", "jsconfig.json"] {
            let Some(v) = read(ts).and_then(|t| parse_jsonc(&t)) else { continue };
            let base = v["compilerOptions"]["baseUrl"].as_str().unwrap_or(".").trim_start_matches("./").trim_matches('/').to_string();
            if let Some(paths) = v["compilerOptions"]["paths"].as_object() {
                for (k, targets) in paths {
                    let Some(t) = targets.as_array().and_then(|a| a.first()).and_then(|t| t.as_str()) else { continue };
                    let prefix = k.trim_end_matches('*').to_string();
                    let dir = join_dir(&base, t.trim_start_matches("./").trim_end_matches('*'));
                    if !prefix.is_empty() {
                        aliases.push((prefix, dir.trim_end_matches('/').to_string()));
                    }
                }
            }
        }
        aliases.sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
        BundlerConfig { vite, packs, packs_configured, aliases }
    }
}

/// Lookup structures over the workspace's JS files.
pub struct JsIndex<'a> {
    files: HashSet<&'a str>,
    /// File stem (or parent dir name for `index.*`) -> JS files, test files excluded.
    by_stem: HashMap<String, Vec<&'a str>>,
    pages_roots: Vec<String>,
    config: &'a BundlerConfig,
}

fn stem_of(path: &str) -> &str {
    let file = path.rsplit('/').next().unwrap_or(path);
    let stem = file.split('.').next().unwrap_or(file);
    if stem == "index" {
        let mut parts = path.rsplit('/');
        parts.next();
        return parts.next().unwrap_or(stem);
    }
    stem
}

impl<'a> JsIndex<'a> {
    pub fn new(files: &'a [RelPath], config: &'a BundlerConfig) -> Self {
        let mut by_stem: HashMap<String, Vec<&str>> = HashMap::new();
        let mut dirs: HashSet<&str> = HashSet::new();
        for f in files.iter().filter(|f| is_js(f)) {
            if !is_test_like(f) {
                by_stem.entry(stem_of(f).to_string()).or_default().push(f);
            }
            let mut d = f.as_str();
            while let Some((parent, _)) = d.rsplit_once('/') {
                if !dirs.insert(parent) {
                    break;
                }
                d = parent;
            }
        }
        let mut roots = vec![config.packs.0.clone(), "app/javascript".into(), "app/frontend".into(), "client".into(), "frontend".into()];
        if let Some((src, _)) = &config.vite {
            roots.insert(0, src.clone());
        }
        let mut pages_roots = Vec::new();
        for r in roots {
            for p in ["pages", "Pages"] {
                let dir = join_dir(&r, p);
                if dirs.contains(dir.as_str()) && !pages_roots.contains(&dir) {
                    pages_roots.push(dir);
                }
            }
        }
        JsIndex { files: files.iter().map(String::as_str).collect(), by_stem, pages_roots, config }
    }

    pub fn exists(&self, path: &str) -> bool {
        self.files.contains(path)
    }

    /// `base`, `base.<ext>` or `base/index.<ext>`.
    fn probe(&self, base: &str) -> Option<RelPath> {
        let base = base.trim_start_matches("./").trim_start_matches('/');
        if is_js(base) && self.files.contains(base) {
            return Some(base.to_string());
        }
        for ext in JS_EXTS {
            let p = format!("{base}.{ext}");
            if self.files.contains(p.as_str()) {
                return Some(p);
            }
        }
        for ext in JS_EXTS {
            let p = format!("{base}/index.{ext}");
            if self.files.contains(p.as_str()) {
                return Some(p);
            }
        }
        None
    }

    fn by_basename(&self, name: &str) -> Outcome {
        let stem = name.rsplit(['/', '.']).next().unwrap_or(name);
        let mut c: Vec<RelPath> = self.by_stem.get(stem).map(|v| v.iter().map(|s| s.to_string()).collect()).unwrap_or_default();
        // Prefer candidates whose path ends with the full name (`admin/Nav`).
        if name.contains('/') {
            let suffix = name.trim_matches('/');
            let narrowed: Vec<RelPath> = c.iter().filter(|p| p.rsplit_once('.').is_some_and(|(b, _)| b.ends_with(suffix) || b.ends_with(&format!("{suffix}/index")))).cloned().collect();
            if !narrowed.is_empty() {
                c = narrowed;
            }
        }
        c.sort();
        match c.len() {
            0 => Outcome::Unresolved,
            1 => Outcome::Resolved(c, Confidence::Inferred),
            2..=3 => Outcome::Resolved(c, Confidence::Heuristic),
            n => Outcome::Ambiguous(n),
        }
    }

    /// Resolves an import specifier written in `from` (relative, aliased, or a
    /// suffix match as last resort).
    pub fn resolve_specifier(&self, from: &str, spec: &str) -> Option<(RelPath, Confidence)> {
        if spec.starts_with('.') {
            let dir = from.rsplit_once('/').map_or("", |(d, _)| d);
            let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
            for seg in spec.split('/') {
                match seg {
                    "." | "" => {}
                    ".." => {
                        parts.pop();
                    }
                    s => parts.push(s),
                }
            }
            return self.probe(&parts.join("/")).map(|p| (p, Confidence::Exact));
        }
        for (prefix, dir) in &self.config.aliases {
            if let Some(rest) = spec.strip_prefix(prefix.as_str())
                && let Some(p) = self.probe(&join_dir(dir, rest)) {
                    return Some((p, Confidence::Exact));
                }
        }
        let rest = spec.split_once('/').map_or(spec, |(_, r)| r);
        match self.by_basename(rest) {
            Outcome::Resolved(v, _) if v.len() == 1 => v.into_iter().next().map(|p| (p, Confidence::Inferred)),
            _ => None,
        }
    }

    fn entry(&self, helper: &str, name: &str) -> Outcome {
        if name.contains("://") || name.starts_with("//") {
            return Outcome::NoTarget;
        }
        let c = self.config;
        let (dirs, confidence): (Vec<String>, Confidence) = match helper {
            "vite_javascript_tag" | "vite_typescript_tag" => match &c.vite {
                Some((src, entry)) => {
                    if let Some(rest) = name.strip_prefix("~/") {
                        return self.probe(&join_dir(src, rest)).map_or(Outcome::Unresolved, |p| Outcome::Resolved(vec![p], Confidence::Exact));
                    }
                    (vec![entry.clone()], Confidence::Exact)
                }
                None => (vec!["app/frontend/entrypoints".into(), "app/javascript/entrypoints".into()], Confidence::Inferred),
            },
            "javascript_pack_tag" | "javascript_packs_with_chunks_tag" => {
                let conf = if c.packs_configured { Confidence::Exact } else { Confidence::Inferred };
                (vec![c.packs.1.clone(), "app/javascript/packs".into(), "app/javascript/entries".into()], conf)
            }
            "javascript_importmap_tags" => (vec!["app/javascript".into()], Confidence::Inferred),
            _ => (vec!["app/assets/javascripts".into(), "app/javascript".into(), "app/javascript/entrypoints".into()], Confidence::Inferred),
        };
        for d in dirs {
            if let Some(p) = self.probe(&join_dir(&d, name)) {
                return Outcome::Resolved(vec![p], confidence);
            }
        }
        Outcome::Unresolved
    }

    /// Canvas `js_bundle :name`: `ui/features/<name>/index.*`, or a feature
    /// nested one level down (`ui/features/accessibility/accessibility_checker`).
    fn js_bundle(&self, name: &str) -> Outcome {
        if let Some(p) = self.probe(&format!("ui/features/{name}")) {
            return Outcome::Resolved(vec![p], Confidence::Inferred);
        }
        let mut nested: Vec<&str> = self
            .by_stem
            .get(name)
            .into_iter()
            .flatten()
            .copied()
            .filter(|p| p.starts_with("ui/features/") && p.matches('/').count() == 4 && stem_of(p) == name && p.rsplit('/').next().is_some_and(|f| f.starts_with("index.")))
            .collect();
        nested.sort_unstable();
        match nested.as_slice() {
            [one] => Outcome::Resolved(vec![one.to_string()], Confidence::Inferred),
            [] => Outcome::Unresolved,
            many => Outcome::Resolved(many.iter().take(3).map(|p| p.to_string()).collect(), Confidence::Heuristic),
        }
    }

    pub fn resolve(&self, m: &MountRef, registry: &Registry) -> Outcome {
        if m.name.is_empty() {
            return if m.helper == "vite_client_tag" { Outcome::NoTarget } else { Outcome::Dynamic };
        }
        match m.kind {
            MountKind::Inertia => {
                for root in &self.pages_roots {
                    if let Some(p) = self.probe(&join_dir(root, &m.name)) {
                        return Outcome::Resolved(vec![p], Confidence::Inferred);
                    }
                }
                Outcome::Unresolved
            }
            MountKind::ReactComponent => {
                if let Some(targets) = registry.get(&m.name) {
                    let conf = targets.iter().map(|(_, c)| *c).min().unwrap_or(Confidence::Inferred);
                    return Outcome::Resolved(targets.iter().map(|(p, _)| p.clone()).collect(), conf.min(Confidence::Inferred));
                }
                let path = m.name.replace('.', "/");
                for root in ["app/javascript/components", "app/frontend/components", &join_dir(&self.config.packs.0, "components")] {
                    if let Some(p) = self.probe(&join_dir(root, &path)) {
                        return Outcome::Resolved(vec![p], Confidence::Inferred);
                    }
                }
                self.by_basename(&path)
            }
            MountKind::EntryTag => self.entry(&m.helper, &m.name),
            MountKind::JsBundle => self.js_bundle(&m.name),
            MountKind::PageBundle => Outcome::Unresolved,
        }
    }
}

/// `ReactOnRails.register({ Name, Other: Component })` registrations:
/// component name -> files, resolved through the file's imports.
pub type Registry = BTreeMap<String, Vec<(RelPath, Confidence)>>;

struct RegPatterns {
    register: Regex,
    default_import: Regex,
    named_import: Regex,
}

fn reg_patterns() -> &'static RegPatterns {
    static P: OnceLock<RegPatterns> = OnceLock::new();
    P.get_or_init(|| RegPatterns {
        register: Regex::new(r"ReactOnRails\.register\(\s*\{([^}]*)\}").expect("regex"),
        default_import: Regex::new(r#"import\s+(\w+)\s*(?:,\s*\{[^}]*\}\s*)?from\s*["']([^"']+)["']"#).expect("regex"),
        named_import: Regex::new(r#"import\s*(?:\w+\s*,\s*)?\{([^}]*)\}\s*from\s*["']([^"']+)["']"#).expect("regex"),
    })
}

/// Registered (name, import specifier) pairs in one JS file.
pub fn scan_registrations(text: &str) -> Vec<(String, String)> {
    if !text.contains("ReactOnRails.register") {
        return Vec::new();
    }
    let p = reg_patterns();
    let mut imports: HashMap<String, String> = HashMap::new();
    for c in p.default_import.captures_iter(text) {
        imports.insert(c[1].to_string(), c[2].to_string());
    }
    for c in p.named_import.captures_iter(text) {
        for item in c[1].split(',') {
            let local = item.rsplit(" as ").next().unwrap_or(item).trim();
            if !local.is_empty() {
                imports.insert(local.to_string(), c[2].to_string());
            }
        }
    }
    let mut out = Vec::new();
    for c in p.register.captures_iter(text) {
        for entry in c[1].split(',') {
            let entry = entry.trim();
            if entry.is_empty() || entry.starts_with("...") {
                continue;
            }
            let (name, ident) = entry.split_once(':').map_or((entry, entry), |(k, v)| (k.trim().trim_matches(['"', '\'']), v.trim()));
            if let Some(spec) = imports.get(ident) {
                out.push((name.to_string(), spec.clone()));
            }
        }
    }
    out
}
