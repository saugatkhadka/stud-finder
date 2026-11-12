//! Specifier resolution against the repo's file set.
//!
//! `oxc_resolver` does the file-level work (extensions, `index.*`, `.js` ->
//! `.ts`, directory `package.json` `main`) over a virtual file system. This
//! module adds what it cannot know without `node_modules`: tsconfig `paths`,
//! bundler aliases from `ripple.yml`, and workspace packages.
//!
//! Order for a non-relative specifier: configured aliases, tsconfig `paths`
//! (nearest config first, then its ancestors), workspace packages, `baseUrl`.
//! A specifier that one of the first three *claims* but nothing resolves is
//! `Unresolved`; one that nothing claims is an external npm package.

use crate::paths;
use crate::tsconfig::TsConfig;
use crate::vfs::{FileSet, SharedFileSet, VirtualFs};
use crate::workspaces::{Workspaces, export_targets};
use oxc_resolver::{ResolveOptions, ResolverGeneric};
use ripple_core::config::JsConfig;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// How a specifier was classified; drives the resolution-rate stats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Relative,
    /// Configured alias or tsconfig `paths`.
    Alias,
    Workspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Resolved { target: String, class: Class },
    /// A bare npm dependency (or URL / builtin): no edge.
    External,
    Unresolved { class: Class },
}

pub struct ModuleResolver {
    root: PathBuf,
    pub files: SharedFileSet,
    oxc: ResolverGeneric<VirtualFs>,
    /// All loaded configs, keyed by directory; within a directory `tsconfig.json`
    /// precedes `jsconfig.json`.
    configs: HashMap<String, Vec<TsConfig>>,
    /// `(key, repo-relative dir)`, longest key first.
    aliases: Vec<(String, String)>,
    pub workspaces: Workspaces,
    /// Problems found while loading configuration, as `(code, message)`.
    pub problems: Vec<(&'static str, String)>,
}

fn oxc_options() -> ResolveOptions {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    ResolveOptions {
        extensions: s(&[".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".json", ".vue", ".d.ts"]),
        extension_alias: vec![
            (".js".into(), s(&[".ts", ".tsx", ".js"])),
            (".jsx".into(), s(&[".tsx", ".jsx"])),
            (".mjs".into(), s(&[".mts", ".mjs"])),
            (".cjs".into(), s(&[".cts", ".cjs"])),
        ],
        main_fields: s(&["module", "main", "types"]),
        condition_names: s(&["import", "module", "browser", "default", "types", "require"]),
        main_files: s(&["index"]),
        symlinks: false,
        builtin_modules: false,
        allow_package_exports_in_directory_resolve: true,
        ..ResolveOptions::default()
    }
}

impl ModuleResolver {
    pub fn new(root: PathBuf, files: SharedFileSet, js: &JsConfig) -> Self {
        let fs = VirtualFs::new(root.clone(), Arc::clone(&files));
        let mut me = Self {
            root,
            files,
            oxc: ResolverGeneric::new_with_file_system(fs, oxc_options()),
            configs: HashMap::new(),
            aliases: Vec::new(),
            workspaces: Workspaces::default(),
            problems: Vec::new(),
        };
        me.load_config(js);
        me
    }

    /// (Re)reads tsconfigs, workspaces and aliases and drops oxc's caches.
    /// Called on construction and whenever a config-ish file changed.
    pub fn load_config(&mut self, js: &JsConfig) {
        self.oxc.clear_cache();
        self.problems.clear();
        self.configs.clear();
        let files = self.files.read().unwrap_or_else(|e| e.into_inner());

        let config_files: Vec<String> = if js.tsconfig.is_empty() {
            files
                .iter()
                .filter(|f| matches!(paths::file_name(f), "tsconfig.json" | "jsconfig.json") && !f.contains("node_modules/"))
                .cloned()
                .collect()
        } else {
            js.tsconfig.iter().filter_map(|t| paths::join("", t)).collect()
        };
        for file in config_files {
            let cfg = TsConfig::load(&self.root, &file);
            for p in &cfg.problems {
                self.problems.push(("js_tsconfig_problem", p.clone()));
            }
            self.configs.entry(cfg.dir.clone()).or_default().push(cfg);
        }
        for list in self.configs.values_mut() {
            // tsconfig.json before jsconfig.json (alphabetical order gives jsconfig first).
            list.sort_by_key(|c| paths::file_name(&c.file).starts_with("jsconfig"));
        }

        self.aliases = js
            .aliases
            .iter()
            .filter_map(|(k, v)| {
                let dir = paths::join("", v)?;
                let key = k.trim_end_matches('/').to_string();
                if !files.is_dir(&dir) && !files.contains(&dir) {
                    self.problems.push(("js_alias_target_missing", format!("alias \"{k}\" points at \"{v}\", which is not in the repo; ignoring it")));
                    return None;
                }
                Some((key, dir))
            })
            .collect();
        self.aliases.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));
        drop(files);

        self.workspaces = Workspaces::discover(&self.root, &self.files.read().unwrap_or_else(|e| e.into_inner()));
        for p in &self.workspaces.problems {
            self.problems.push(("js_package_json_problem", p.clone()));
        }
    }

    /// Files whose change must trigger [`load_config`](Self::load_config).
    pub fn config_deps(&self) -> impl Iterator<Item = &String> {
        self.configs.values().flatten().flat_map(|c| c.deps.iter()).chain(self.workspaces.deps.iter())
    }

    pub fn config_count(&self) -> usize {
        self.configs.values().map(Vec::len).sum()
    }

    pub fn clear_cache(&self) {
        self.oxc.clear_cache();
    }

    pub fn files(&self) -> std::sync::RwLockReadGuard<'_, FileSet> {
        self.files.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn resolve(&self, importer: &str, raw: &str) -> Outcome {
        let spec = clean_specifier(raw);
        let importer_dir = paths::parent(importer);

        if spec.is_empty() {
            return Outcome::External;
        }
        if spec == "." || spec == ".." || spec.starts_with("./") || spec.starts_with("../") {
            return self.relative(importer_dir, spec);
        }
        if spec.starts_with('/') {
            // Web-root absolute (`/images/x.png`): relative to the repo root.
            let target = paths::join("", spec);
            return match target.and_then(|t| self.resolve_target(&t, spec.ends_with('/'))) {
                Some(target) => Outcome::Resolved { target, class: Class::Relative },
                None => Outcome::Unresolved { class: Class::Relative },
            };
        }
        if is_external_scheme(spec) {
            return Outcome::External;
        }

        let mut claimed: Option<Class> = None;

        // 1. Aliases from ripple.yml.
        for (key, dir) in &self.aliases {
            let rest = match spec.strip_prefix(key.as_str()) {
                Some("") => "",
                Some(r) if r.starts_with('/') => r,
                _ => continue,
            };
            claimed.get_or_insert(Class::Alias);
            if let Some(t) = paths::join(dir, rest.trim_start_matches('/')).and_then(|t| self.resolve_target(&t, false)) {
                return Outcome::Resolved { target: t, class: Class::Alias };
            }
        }

        // 2. tsconfig `paths`, then 3. `baseUrl`, nearest config first.
        let chain = self.config_chain(importer_dir);
        for cfg in &chain {
            if let Some((targets, claims)) = cfg.match_paths(spec) {
                if claims {
                    claimed.get_or_insert(Class::Alias);
                }
                if let Some(t) = targets.iter().find_map(|t| self.resolve_target(t, false)) {
                    return Outcome::Resolved { target: t, class: Class::Alias };
                }
            }
        }

        // 4. Workspace packages.
        if let Some((pkg, sub)) = self.workspaces.lookup(spec) {
            claimed.get_or_insert(Class::Workspace);
            if let Some(t) = self.resolve_workspace(pkg, sub) {
                return Outcome::Resolved { target: t, class: Class::Workspace };
            }
        }

        // 5. baseUrl: resolves if the file exists, but a miss is not a claim
        // (`react` with a baseUrl is still `react`).
        for cfg in &chain {
            if let Some(t) = cfg.base_url_candidate(spec).and_then(|t| self.resolve_target(&t, false)) {
                return Outcome::Resolved { target: t, class: Class::Alias };
            }
        }

        match claimed {
            // A tsconfig `paths` rule like `@instructure/*` -> `packages/*` also
            // matches npm packages that merely share the scope. TypeScript falls
            // back to node_modules there, so a declared dependency is external.
            Some(Class::Alias) if self.workspaces.is_declared_dependency(spec) => Outcome::External,
            Some(class) => Outcome::Unresolved { class },
            None => Outcome::External,
        }
    }

    fn relative(&self, importer_dir: &str, spec: &str) -> Outcome {
        match paths::join(importer_dir, spec).and_then(|t| self.resolve_target(&t, spec.ends_with('/') || spec == "." || spec == "..")) {
            Some(target) => Outcome::Resolved { target, class: Class::Relative },
            None => Outcome::Unresolved { class: Class::Relative },
        }
    }

    /// Configs applying to a file in `dir`, nearest directory first.
    fn config_chain(&self, dir: &str) -> Vec<&TsConfig> {
        let mut out = Vec::new();
        let mut cur = dir;
        loop {
            if let Some(list) = self.configs.get(cur) {
                out.extend(list.iter());
            }
            if cur.is_empty() {
                break;
            }
            cur = paths::parent(cur);
        }
        out
    }

    fn resolve_workspace(&self, pkg: &crate::workspaces::Package, sub: &str) -> Option<String> {
        let subpath = if sub.is_empty() { ".".to_string() } else { format!("./{sub}") };
        if let Some(exports) = &pkg.exports {
            for t in export_targets(exports, &subpath) {
                if let Some(found) = paths::join(&pkg.dir, &t).and_then(|p| self.resolve_target(&p, false)) {
                    return Some(found);
                }
            }
        }
        if sub.is_empty() {
            // `main` / `module` / `types` / `index.*` via oxc, then the conventional `src/index`.
            self.resolve_target(&pkg.dir, true).or_else(|| self.resolve_target(&format!("{}/src/index", pkg.dir), false))
        } else {
            let direct = paths::join(&pkg.dir, sub)?;
            self.resolve_target(&direct, false).or_else(|| self.resolve_target(&format!("{}/src/{sub}", pkg.dir), false))
        }
    }

    /// Resolves a repo-relative path to a file: exact, with extensions,
    /// `.js` -> `.ts`, or as a directory (`package.json` main, `index.*`).
    /// `as_dir` forces directory resolution (`./foo/`).
    fn resolve_target(&self, target: &str, as_dir: bool) -> Option<String> {
        let (dir, spec) = if as_dir || target.is_empty() {
            (target, ".".to_string())
        } else {
            let (d, n) = target.rsplit_once('/').unwrap_or(("", target));
            (d, format!("./{n}"))
        };
        let res = self.oxc.resolve(self.root.join(dir), &spec).ok()?;
        let rel = res.path().strip_prefix(&self.root).ok()?.to_str()?;
        Some(rel.to_string())
    }
}

/// Drops webpack loader prefixes (`style-loader!./x.css`), and `?query` / `#hash`
/// suffixes (`./icon.svg?react`).
fn clean_specifier(raw: &str) -> &str {
    let s = raw.rsplit('!').next().unwrap_or(raw);
    let s = s.split(['?', '#']).next().unwrap_or(s);
    s.trim()
}

fn is_external_scheme(spec: &str) -> bool {
    // `node:fs`, `https://…`, `data:…`, `virtual:…` — never repo files. (`@scope/pkg`
    // contains no colon.) Windows drive letters are not a concern: repo paths are relative.
    spec.contains(':')
}
