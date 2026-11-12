//! An extractor that reads edges from a text file. Enabled by setting
//! `RIPPLE_EDGE_FILE` (absolute, or relative to the repo root). Used for
//! benchmarks with synthetic graphs and for end-to-end tests of the binary
//! that must not depend on the real language extractors; it can also inject
//! hand-written dependencies the extractors cannot see.
//!
//! Format: one entry per line, whitespace separated; `#` at the start of a
//! line or after whitespace starts a comment.
//!
//! * `FROM TO [KIND [CONFIDENCE [LINE [DETAIL...]]]]`: an edge. KIND defaults
//!   to `ruby_constant`, CONFIDENCE to `exact`. Either endpoint may name a
//!   scope: `app/services/refund.rb#run app/models/purchase.rb#refund!` sets
//!   `from_scope` / `to_scope` (`#@body` = the class-level part).
//! * `@inherit FROM TO [KIND ...]`: an inheritance edge (scopes ignored).
//! * `@scope FILE NAME START END`: a scope's line range (1-based, inclusive),
//!   reported as [`ripple_core::Facts::scopes`] so diffs map onto methods.
//!
//! Edges and scopes whose files are not workspace files are dropped, as a
//! real extractor would.

use anyhow::{Context, Result};
use ripple_core::model::ScopeRange;
use ripple_core::{Confidence, Edge, EdgeKind, ExtractOutput, Extractor, Facts, RelPath, Warning, Workspace};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

pub const ENV: &str = "RIPPLE_EDGE_FILE";

pub struct EdgeFileExtractor {
    path: PathBuf,
}

impl EdgeFileExtractor {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn from_env() -> Option<Self> {
        std::env::var_os(ENV).filter(|v| !v.is_empty()).map(Self::new)
    }
}

fn parse_enum<T: serde::de::DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

/// Drops a trailing comment: `#` at the start or after whitespace.
fn strip_comment(line: &str) -> &str {
    let b = line.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        if c == b'#' && (i == 0 || b[i - 1].is_ascii_whitespace()) {
            return &line[..i];
        }
    }
    line
}

/// `path#scope` -> (`path`, Some(`scope`)).
fn endpoint(s: &str) -> (&str, Option<&str>) {
    match s.split_once('#') {
        Some((p, sc)) if !sc.is_empty() => (p, Some(sc)),
        Some((p, _)) => (p, None),
        None => (s, None),
    }
}

/// Parses the edge-file format (see module docs).
pub fn parse_edges(text: &str) -> (Vec<Edge>, Vec<Warning>) {
    let (edges, _, warnings) = parse(text);
    (edges, warnings)
}

/// Edges, scope ranges (per file, sorted by start) and warnings.
pub fn parse(text: &str) -> (Vec<Edge>, BTreeMap<RelPath, Vec<ScopeRange>>, Vec<Warning>) {
    let mut edges = Vec::new();
    let mut scopes: BTreeMap<RelPath, Vec<ScopeRange>> = BTreeMap::new();
    let mut warnings = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        let mut f = line.split_whitespace();
        let mut inherit = false;
        match f.clone().next() {
            Some("@scope") => {
                let parts: Vec<&str> = f.skip(1).collect();
                let parsed = match parts.as_slice() {
                    [file, name, a, b] => a.parse::<u32>().ok().zip(b.parse::<u32>().ok()).filter(|(a, b)| a <= b).map(|(a, b)| (file, name, a, b)),
                    _ => None,
                };
                match parsed {
                    Some((file, name, start, end)) => scopes.entry(file.to_string()).or_default().push(ScopeRange { name: name.to_string(), start, end }),
                    None => warnings.push(Warning::new("edge_file_bad_line", format!("line {}: expected `@scope FILE NAME START END`", i + 1))),
                }
                continue;
            }
            Some("@inherit") => {
                f.next();
                inherit = true;
            }
            _ => {}
        }
        let (Some(from), Some(to)) = (f.next(), f.next()) else {
            warnings.push(Warning::new("edge_file_bad_line", format!("line {}: expected `FROM TO [KIND [CONFIDENCE [LINE]]]`", i + 1)));
            continue;
        };
        let ((from, from_scope), (to, to_scope)) = (endpoint(from), endpoint(to));
        let kind = f.next().map(parse_enum::<EdgeKind>).unwrap_or(Some(EdgeKind::RubyConstant));
        let conf = f.next().map(parse_enum::<Confidence>).unwrap_or(Some(Confidence::Exact));
        let (Some(kind), Some(conf)) = (kind, conf) else {
            warnings.push(Warning::new("edge_file_bad_line", format!("line {}: unknown kind or confidence", i + 1)));
            continue;
        };
        let mut e = Edge::new(from, to, kind, conf);
        if inherit {
            e = e.inherit();
        } else {
            e.from_scope = from_scope.map(str::to_string);
            e.to_scope = to_scope.map(str::to_string);
        }
        if let Some(l) = f.next().and_then(|l| l.parse().ok()) {
            e = e.at(l);
        }
        let detail: Vec<&str> = f.collect();
        if !detail.is_empty() {
            e = e.detail(detail.join(" "));
        }
        edges.push(e);
    }
    for v in scopes.values_mut() {
        v.sort_by_key(|r| (r.start, r.end));
    }
    (edges, scopes, warnings)
}

impl Extractor for EdgeFileExtractor {
    fn name(&self) -> &'static str {
        "edge_file"
    }

    fn build(&mut self, ws: &Workspace) -> Result<ExtractOutput> {
        let path = if self.path.is_absolute() { self.path.clone() } else { ws.root.join(&self.path) };
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let (edges, mut scopes, warnings) = parse(&text);
        let files: HashSet<&str> = ws.files.iter().map(String::as_str).collect();
        let total = edges.len();
        let edges: Vec<Edge> = edges.into_iter().filter(|e| files.contains(e.from.as_str()) && files.contains(e.to.as_str())).collect();
        scopes.retain(|f, _| files.contains(f.as_str()));
        let mut out = ExtractOutput { warnings, ..Default::default() };
        out.stats.insert("edges_read".into(), total as u64);
        out.stats.insert("edges_kept".into(), edges.len() as u64);
        out.stats.insert("scopes".into(), scopes.values().map(Vec::len).sum::<usize>() as u64);
        out.edges = edges;
        out.facts = Facts { scopes, ..Default::default() };
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lines() {
        let (e, w) = parse_edges("a.rb b.rb\n# c\nc.ts d.rb api_call heuristic 7 POST /x\nbad\nx y nope\n");
        assert_eq!(e.len(), 2);
        assert_eq!(e[1].kind, EdgeKind::ApiCall);
        assert_eq!(e[1].confidence, Confidence::Heuristic);
        assert_eq!((e[1].line, e[1].detail.as_deref()), (7, Some("POST /x")));
        assert_eq!(w.len(), 2);
    }

    #[test]
    fn scopes_inherit_and_comments() {
        let text = "a.rb#run m.rb#refund! ruby_constant exact 12 Purchase # calls refund!\n\
                    @inherit sub.rb m.rb\n\
                    @scope m.rb charge! 9 11\n@scope m.rb refund! 4 7\n@scope m.rb bad 9\n\
                    c.rb m.rb#@body rails_inferred inferred 3 Users#show\n#whole line\n";
        let (e, s, w) = parse(text);
        assert_eq!(e.len(), 3);
        assert_eq!((e[0].from.as_str(), e[0].from_scope.as_deref(), e[0].to.as_str(), e[0].to_scope.as_deref()), ("a.rb", Some("run"), "m.rb", Some("refund!")));
        assert_eq!((e[0].line, e[0].detail.as_deref()), (12, Some("Purchase")));
        assert!(e[1].inherit && e[1].from_scope.is_none());
        assert_eq!((e[2].to_scope.as_deref(), e[2].detail.as_deref()), (Some("@body"), Some("Users#show")), "`#` inside a word is not a comment");
        let names: Vec<(&str, u32, u32)> = s["m.rb"].iter().map(|r| (r.name.as_str(), r.start, r.end)).collect();
        assert_eq!(names, [("refund!", 4, 7), ("charge!", 9, 11)]);
        assert_eq!(w.len(), 1);
    }
}
