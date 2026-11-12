//! Import aliases declared outside tsconfig: Jest `moduleNameMapper`
//! (`jest.config*` or the `jest` key of `package.json`) and bundler
//! `resolve.alias` objects (`webpack.config*`, `config/webpack/*`,
//! `vite.config*`, `vitest.config*`).
//!
//! Best effort and static only: the config is never executed. A value is
//! read when it is a string, a template literal, `path.join`/`path.resolve`
//! of such parts, `__dirname` (or `dirname`, `import.meta.dirname`,
//! `fileURLToPath(new URL(.., import.meta.url))`), or a `const` bound to one
//! of those in the same file or in a file it `require`s / imports relatively.
//! Only prefix aliases whose target is a directory or file in the repo become
//! aliases: `'^~(/.*)$': '<rootDir>/app/assets/javascripts$1'`,
//! `'~': path.join(ROOT_PATH, 'app/assets/javascripts')`, `'@/': `${src}/``.
//! Exact-match keys (`jquery$`, `'^vue$'`), package remaps (`lodash:
//! 'lodash-es'`) and whole-prefix stubs (a mapper without `$1`) are skipped.
//! Entries whose value cannot be read are reported, one problem per file.
//!
//! An alias applies to importers under its *scope*: the nearest directory at
//! or above the config file that has a `package.json` (the repo root when
//! none does).

use crate::paths;
use crate::vfs::FileSet;
use regex::Regex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

/// One discovered alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alias {
    /// Importers under this directory (`""` = everywhere) see the alias.
    pub scope: String,
    /// Specifier prefix (`~`, `ee_else_ce`, `@`).
    pub key: String,
    /// Repo-relative directory or file the prefix stands for.
    pub target: String,
    /// The config file it came from.
    pub file: String,
    /// The key alone is also a specifier (`~` itself): bundler aliases yes,
    /// Jest `^key(/.*)$` mappers no.
    pub bare: bool,
}

#[derive(Debug, Default)]
pub struct Discovered {
    /// In config-file order, then declaration order (first wins per key).
    pub aliases: Vec<Alias>,
    /// Every file read (configs and the files they import constants from).
    pub deps: Vec<String>,
    pub problems: Vec<String>,
}

/// Is `path` a config file this module reads?
pub fn is_bundler_config(path: &str) -> bool {
    if path.contains("node_modules/") {
        return false;
    }
    let name = paths::file_name(path);
    let Some((stem, ext)) = name.rsplit_once('.') else { return false };
    if !matches!(ext, "js" | "cjs" | "mjs" | "ts" | "mts" | "cts") {
        return false;
    }
    let base = stem.split('.').next().unwrap_or(stem);
    (matches!(base, "jest" | "webpack" | "vite" | "vitest") && stem.starts_with(&format!("{base}.config")))
        || paths::parent(path).ends_with("config/webpack")
}

fn is_jest_config(path: &str) -> bool {
    paths::file_name(path).starts_with("jest.config")
}

/// Marks evaluated filesystem paths: `ROOT` + `/` + repo-relative path.
const ROOT: &str = "/\u{0}root";

fn to_rel(p: &str) -> Option<String> {
    let rest = p.strip_prefix(ROOT)?;
    paths::join("", rest.trim_start_matches('/'))
}

fn abs(rel: &str) -> String {
    if rel.is_empty() { ROOT.to_string() } else { format!("{ROOT}/{rel}") }
}

/// Lexical normalisation of `/`-separated parts (`..`, `.`, empty).
fn normalize(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if out.len() > 1 || out.first().is_some_and(|s| *s != "\u{0}root") {
                    out.pop();
                }
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

/// Blanks `//` and `/* */` comments outside string literals (keeps offsets).
fn strip_comments(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0;
    let mut quote: Option<u8> = None;
    while i < b.len() {
        let c = b[i];
        if let Some(q) = quote {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == q || (c == b'\n' && q != b'`') {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' | b'`' => quote = Some(c),
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    out[i] = b' ';
                    i += 1;
                }
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    if b[i] != b'\n' {
                        out[i] = b' ';
                    }
                    i += 1;
                }
                for _ in 0..2 {
                    if i < b.len() {
                        out[i] = b' ';
                        i += 1;
                    }
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    String::from_utf8(out).unwrap_or_default()
}

/// Splits `s` at top-level occurrences of `sep` (outside brackets and strings).
fn split_top(s: &str, sep: u8) -> Vec<&str> {
    let b = s.as_bytes();
    let (mut depth, mut quote, mut start) = (0i32, None::<u8>, 0);
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if let Some(q) = quote {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
        } else {
            match c {
                b'\'' | b'"' | b'`' => quote = Some(c),
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => depth -= 1,
                c if c == sep && depth == 0 => {
                    out.push(&s[start..i]);
                    start = i + 1;
                }
                _ => {}
            }
        }
        i += 1;
    }
    out.push(&s[start..]);
    out
}

/// The balanced `{ ... }` body starting at `open` (index of `{`), without braces.
fn braced(s: &str, open: usize) -> Option<&str> {
    let b = s.as_bytes();
    let (mut depth, mut quote) = (0i32, None::<u8>);
    let mut i = open;
    while i < b.len() {
        let c = b[i];
        if let Some(q) = quote {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
        } else {
            match c {
                b'\'' | b'"' | b'`' => quote = Some(c),
                b'{' | b'[' | b'(' => depth += 1,
                b'}' | b']' | b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&s[open + 1..i]);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

#[derive(Debug, Clone, PartialEq)]
enum Val {
    Str(String),
}

struct Env<'a> {
    /// Config directory as an evaluated path.
    dir: String,
    vars: &'a HashMap<String, String>,
}

fn unquote(e: &str) -> Option<String> {
    let q = e.chars().next()?;
    if !matches!(q, '\'' | '"') || e.len() < 2 || !e.ends_with(q) {
        return None;
    }
    let inner = &e[1..e.len() - 1];
    (!inner.contains(q)).then(|| inner.replace("\\\\", "\\"))
}

fn call_args<'s>(e: &'s str, names: &[&str]) -> Option<Vec<&'s str>> {
    for n in names {
        if let Some(rest) = e.strip_prefix(n) {
            let rest = rest.trim_start();
            if rest.starts_with('(') && rest.ends_with(')') {
                return Some(split_top(&rest[1..rest.len() - 1], b',').into_iter().map(str::trim).filter(|a| !a.is_empty()).collect());
            }
        }
    }
    None
}

fn eval(e: &str, env: &Env) -> Option<Val> {
    let e = e.trim().trim_end_matches(';').trim();
    let e = e.strip_suffix(" as const").unwrap_or(e).trim();
    if let Some(s) = unquote(e) {
        return Some(Val::Str(s));
    }
    if matches!(e, "__dirname" | "dirname" | "import.meta.dirname" | "path.dirname(fileURLToPath(import.meta.url))" | "dirname(fileURLToPath(import.meta.url))") {
        return Some(Val::Str(env.dir.clone()));
    }
    if e.len() >= 2 && e.starts_with('`') && e.ends_with('`') {
        let inner = &e[1..e.len() - 1];
        let mut out = String::new();
        let mut rest = inner;
        while let Some(i) = rest.find("${") {
            out.push_str(&rest[..i]);
            let end = rest[i..].find('}')? + i;
            let Val::Str(v) = eval(&rest[i + 2..end], env)?;
            out.push_str(&v);
            rest = &rest[end + 1..];
        }
        out.push_str(rest);
        return Some(Val::Str(out));
    }
    if let Some(args) = call_args(e, &["path.join", "path.posix.join", "join"]) {
        let parts: Vec<String> = args.iter().map(|a| eval(a, env).map(|Val::Str(s)| s)).collect::<Option<_>>()?;
        return Some(Val::Str(normalize(&parts.join("/"))));
    }
    if let Some(args) = call_args(e, &["path.resolve", "path.posix.resolve", "resolve"]) {
        let mut acc = ROOT.to_string();
        for a in args {
            let Val::Str(s) = eval(a, env)?;
            acc = if s.starts_with('/') { s } else { format!("{acc}/{s}") };
        }
        return Some(Val::Str(normalize(&acc)));
    }
    if let Some(args) = call_args(e, &["fileURLToPath"]) {
        let url = args.first()?.strip_prefix("new URL(")?.strip_suffix(')')?;
        let parts = split_top(url, b',');
        if parts.len() == 2 && parts[1].trim() == "import.meta.url" {
            let rel = unquote(parts[0].trim())?;
            return Some(Val::Str(normalize(&format!("{}/{rel}", env.dir))));
        }
        return None;
    }
    if e.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$') {
        return env.vars.get(e).map(|v| Val::Str(v.clone()));
    }
    None
}

fn patterns() -> &'static (Regex, Regex, Regex, Regex, Regex) {
    static P: OnceLock<(Regex, Regex, Regex, Regex, Regex)> = OnceLock::new();
    P.get_or_init(|| {
        (
            // const NAME = expr  (one line)
            Regex::new(r"(?m)^[ \t]*(?:export[ \t]+)?(?:const|let|var)[ \t]+([A-Za-z_$][\w$]*)[ \t]*(?::[^=\n]+)?=[ \t]*([^\n]+?)[ \t]*;?[ \t]*$").expect("regex"),
            // const { A, B: C } = require('./x')  /  import { A } from './x'
            Regex::new(r#"(?:(?:const|let|var)\s*\{([^}]*)\}\s*=\s*require\(\s*['"](\.[^'"]+)['"]\s*\)|import\s*\{([^}]*)\}\s*from\s*['"](\.[^'"]+)['"])"#).expect("regex"),
            // alias: {  /  const alias = {  /  Object.assign(alias, {
            Regex::new(r"(?:\balias\s*[:=]\s*|\bObject\.assign\(\s*alias\s*,\s*)[\{\[]").expect("regex"),
            // '^prefix(/.*)$': value   (jest moduleNameMapper)
            Regex::new(r#"(['"])(\^[^'"\n]+\$)['"]\s*:\s*([^,\n]+)"#).expect("regex"),
            // jest mapper key forms
            Regex::new(r"^\^([\w@~$.\-]+)(?:\(/\.\*\)|/\(\.\*\))\$$").expect("regex"),
        )
    })
}

/// `const` bindings of a file, evaluated in order; imports from relative
/// files are followed one level (their own bindings only).
fn bindings(root: &Path, rel: &str, files: &FileSet, follow: bool, deps: &mut Vec<String>) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    let Ok(src) = std::fs::read_to_string(root.join(rel)) else { return vars };
    let src = strip_comments(&src);
    let dir = abs(paths::parent(rel));
    let (decl, import, ..) = patterns();
    if follow {
        for c in import.captures_iter(&src) {
            let (names, spec) = match (c.get(1), c.get(2), c.get(3), c.get(4)) {
                (Some(n), Some(s), _, _) | (_, _, Some(n), Some(s)) => (n.as_str(), s.as_str()),
                _ => continue,
            };
            let Some(base) = paths::join(paths::parent(rel), spec) else { continue };
            let Some(file) = ["", ".js", ".cjs", ".mjs", ".ts", "/index.js"].iter().map(|x| format!("{base}{x}")).find(|f| files.contains(f)) else { continue };
            deps.push(file.clone());
            let theirs = bindings(root, &file, files, false, deps);
            for n in names.split(',') {
                let (from, to) = match n.split_once(':').or_else(|| n.split_once(" as ")) {
                    Some((a, b)) => (a.trim(), b.trim()),
                    None => (n.trim(), n.trim()),
                };
                if let Some(v) = theirs.get(from) {
                    vars.insert(to.to_string(), v.clone());
                }
            }
        }
    }
    for c in decl.captures_iter(&src) {
        let env = Env { dir: dir.clone(), vars: &vars };
        if let Some(Val::Str(v)) = eval(&c[2], &env) {
            vars.insert(c[1].to_string(), v);
        }
    }
    vars
}

/// Scope of a config file: nearest ancestor (inclusive) with a `package.json`.
fn scope_of(rel: &str, files: &FileSet) -> String {
    let mut dir = paths::parent(rel);
    loop {
        let pj = if dir.is_empty() { "package.json".to_string() } else { format!("{dir}/package.json") };
        if files.contains(&pj) || dir.is_empty() {
            return dir.to_string();
        }
        dir = paths::parent(dir);
    }
}

fn exists(files: &FileSet, rel: &str) -> bool {
    !rel.is_empty() && !rel.split('/').any(|s| s == "node_modules") && (files.is_dir(rel) || files.contains(rel))
}

/// Jest `moduleNameMapper` entries of one config text (`<rootDir>` = `root_dir`).
fn jest_aliases(text: &str, env: &Env, root_dir: &str, files: &FileSet, out: &mut Vec<(String, String)>, unreadable: &mut Vec<String>) {
    let (.., entry, key_form) = patterns();
    for c in entry.captures_iter(text) {
        let key = &c[2];
        let Some(k) = key_form.captures(key) else { continue };
        let prefix = k[1].to_string();
        let slash_in_group = key.ends_with("(/.*)$");
        let value = c[3].trim();
        let Some(Val::Str(v)) = eval(value, env) else {
            unreadable.push(key.to_string());
            continue;
        };
        // A mapper without `$1` stubs the whole prefix with one file (`ee_component` -> empty component).
        let Some(stem) = v.strip_suffix("$1") else { continue };
        let stem = if slash_in_group { stem } else { stem.strip_suffix('/').unwrap_or(stem) };
        let Some(path) = stem.strip_prefix("<rootDir>") else { continue };
        let Some(rel) = to_rel(&normalize(&format!("{root_dir}/{path}"))) else { continue };
        if exists(files, &rel) {
            out.push((prefix, rel));
        }
    }
}

/// Entries of `alias: { ... }` objects and `{ find: 'x', replacement: y }` arrays.
fn bundler_aliases(text: &str, env: &Env, files: &FileSet, out: &mut Vec<(String, String)>, unreadable: &mut Vec<String>) {
    let (_, _, alias_re, ..) = patterns();
    for m in alias_re.find_iter(text) {
        let open = m.end() - 1;
        let Some(body) = braced(text, open) else { continue };
        let array = text.as_bytes()[open] == b'[';
        let entries: Vec<(String, &str)> = if array {
            split_top(body, b',')
                .into_iter()
                .filter_map(|item| {
                    let obj = item.trim().strip_prefix('{')?.strip_suffix('}')?;
                    let mut find = None;
                    let mut repl = None;
                    for kv in split_top(obj, b',') {
                        let parts = split_top(kv, b':');
                        if parts.len() < 2 {
                            continue;
                        }
                        let v = kv[parts[0].len() + 1..].trim();
                        match parts[0].trim() {
                            "find" => find = unquote(v),
                            "replacement" => repl = Some(v),
                            _ => {}
                        }
                    }
                    Some((find?, repl?))
                })
                .collect()
        } else {
            split_top(body, b',')
                .into_iter()
                .filter_map(|kv| {
                    let kv = kv.trim();
                    if kv.is_empty() || kv.starts_with("...") {
                        return None;
                    }
                    let parts = split_top(kv, b':');
                    if parts.len() < 2 {
                        return None;
                    }
                    let k = parts[0].trim();
                    let key = unquote(k).or_else(|| k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$').then(|| k.to_string()))?;
                    Some((key, kv[parts[0].len() + 1..].trim()))
                })
                .collect()
        };
        for (key, value) in entries {
            if key.ends_with('$') || key.is_empty() {
                continue; // exact-match alias: a package or file remap
            }
            let key = key.trim_end_matches('/').to_string();
            if key.is_empty() {
                continue;
            }
            match eval(value, env) {
                Some(Val::Str(v)) => {
                    // A bare package name (`lodash-es`) is not a repo path.
                    let Some(rel) = v.starts_with(ROOT).then(|| to_rel(&normalize(&v))).flatten() else { continue };
                    if exists(files, &rel) {
                        out.push((key, rel));
                    }
                }
                None if value == "false" || value.starts_with('/') => {}
                None => unreadable.push(key),
            }
        }
    }
}

pub fn discover(root: &Path, files: &FileSet) -> Discovered {
    let mut d = Discovered::default();
    let mut configs: Vec<&String> = files.iter().filter(|f| is_bundler_config(f)).collect();
    configs.sort();
    for rel in configs {
        let Ok(src) = std::fs::read_to_string(root.join(rel)) else { continue };
        d.deps.push(rel.clone());
        let text = strip_comments(&src);
        let vars = bindings(root, rel, files, true, &mut d.deps);
        let env = Env { dir: abs(paths::parent(rel)), vars: &vars };
        let mut found = Vec::new();
        let mut unreadable = Vec::new();
        if is_jest_config(rel) {
            jest_aliases(&text, &env, &env.dir, files, &mut found, &mut unreadable);
        } else {
            bundler_aliases(&text, &env, files, &mut found, &mut unreadable);
        }
        if !unreadable.is_empty() {
            unreadable.sort();
            unreadable.dedup();
            d.problems.push(format!("{rel}: {} alias entr{} not statically readable, ignored: {}", unreadable.len(), if unreadable.len() == 1 { "y" } else { "ies" }, unreadable.join(", ")));
        }
        let scope = scope_of(rel, files);
        let bare = !is_jest_config(rel);
        for (key, target) in found {
            d.aliases.push(Alias { scope: scope.clone(), key, target, file: rel.clone(), bare });
        }
    }
    // `package.json` `jest.moduleNameMapper`.
    let mut pkgs: Vec<&String> = files.iter().filter(|f| paths::file_name(f) == "package.json" && !f.contains("node_modules/")).collect();
    pkgs.sort();
    for rel in pkgs {
        let Ok(src) = std::fs::read_to_string(root.join(rel)) else { continue };
        if !src.contains("moduleNameMapper") {
            continue;
        }
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&src) else { continue };
        let Some(map) = json.get("jest").and_then(|j| j.get("moduleNameMapper")).and_then(|m| m.as_object()) else { continue };
        let text: String = map.iter().filter_map(|(k, v)| Some(format!("{}: {}\n", serde_json::to_string(k).ok()?, serde_json::to_string(v.as_str()?).ok()?))).collect();
        let vars = HashMap::new();
        let env = Env { dir: abs(paths::parent(rel)), vars: &vars };
        let mut found = Vec::new();
        jest_aliases(&text, &env, &env.dir, files, &mut found, &mut Vec::new());
        let scope = scope_of(rel, files);
        for (key, target) in found {
            d.aliases.push(Alias { scope: scope.clone(), key, target, file: rel.clone(), bare: false });
        }
    }
    d.deps.sort();
    d.deps.dedup();
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_names() {
        for p in ["jest.config.js", "jest.config.base.js", "config/webpack.config.js", "vite.config.mts", "vitest.config.ts", "config/webpack/common.js", "packages/a/webpack.config.cjs"] {
            assert!(is_bundler_config(p), "{p}");
        }
        for p in ["jest.js", "webpack.js", "app/webpack_helper.js", "node_modules/x/vite.config.js", "jest_resolver.js", "config/webpack.yml"] {
            assert!(!is_bundler_config(p), "{p}");
        }
    }

    #[test]
    fn evaluates_paths() {
        let mut vars = HashMap::new();
        vars.insert("ROOT_PATH".to_string(), abs(""));
        let env = Env { dir: abs("config"), vars: &vars };
        let get = |e: &str| eval(e, &env).map(|Val::Str(s)| to_rel(&normalize(&s)).unwrap_or(s));
        assert_eq!(get("path.join(ROOT_PATH, 'app/assets/javascripts')").as_deref(), Some("app/assets/javascripts"));
        assert_eq!(get("path.resolve(__dirname, '..', 'app')").as_deref(), Some("app"));
        assert_eq!(get("`${ROOT_PATH}/app/`").as_deref(), Some("app"));
        assert_eq!(get("fileURLToPath(new URL('../src', import.meta.url))").as_deref(), Some("src"));
        assert_eq!(get("'lodash-es'").as_deref(), Some("lodash-es"));
        assert_eq!(get("someFunction('x')"), None);
    }
}
