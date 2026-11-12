//! An extractor that reads edges from a text file. Enabled by setting
//! `RIPPLE_EDGE_FILE` (absolute, or relative to the repo root). Used for
//! benchmarks with synthetic graphs and for end-to-end tests of the binary
//! that must not depend on the real language extractors; it can also inject
//! hand-written dependencies the extractors cannot see.
//!
//! Format: one edge per line, whitespace separated:
//! `FROM TO [KIND [CONFIDENCE [LINE [DETAIL...]]]]`, `#` starts a comment.
//! KIND defaults to `ruby_constant`, CONFIDENCE to `exact`. Edges whose
//! endpoints are not workspace files are dropped, as a real extractor would.

use anyhow::{Context, Result};
use ripple_core::{Confidence, Edge, EdgeKind, ExtractOutput, Extractor, Warning, Workspace};
use std::collections::HashSet;
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

/// Parses the edge-file format (see module docs).
pub fn parse_edges(text: &str) -> (Vec<Edge>, Vec<Warning>) {
    let mut edges = Vec::new();
    let mut warnings = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut f = line.split_whitespace();
        let (Some(from), Some(to)) = (f.next(), f.next()) else {
            warnings.push(Warning::new("edge_file_bad_line", format!("line {}: expected `FROM TO [KIND [CONFIDENCE [LINE]]]`", i + 1)));
            continue;
        };
        let kind = f.next().map(parse_enum::<EdgeKind>).unwrap_or(Some(EdgeKind::RubyConstant));
        let conf = f.next().map(parse_enum::<Confidence>).unwrap_or(Some(Confidence::Exact));
        let (Some(kind), Some(conf)) = (kind, conf) else {
            warnings.push(Warning::new("edge_file_bad_line", format!("line {}: unknown kind or confidence", i + 1)));
            continue;
        };
        let mut e = Edge::new(from, to, kind, conf);
        if let Some(l) = f.next().and_then(|l| l.parse().ok()) {
            e = e.at(l);
        }
        let detail: Vec<&str> = f.collect();
        if !detail.is_empty() {
            e = e.detail(detail.join(" "));
        }
        edges.push(e);
    }
    (edges, warnings)
}

impl Extractor for EdgeFileExtractor {
    fn name(&self) -> &'static str {
        "edge_file"
    }

    fn build(&mut self, ws: &Workspace) -> Result<ExtractOutput> {
        let path = if self.path.is_absolute() { self.path.clone() } else { ws.root.join(&self.path) };
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let (edges, warnings) = parse_edges(&text);
        let files: HashSet<&str> = ws.files.iter().map(String::as_str).collect();
        let total = edges.len();
        let edges: Vec<Edge> = edges.into_iter().filter(|e| files.contains(e.from.as_str()) && files.contains(e.to.as_str())).collect();
        let mut out = ExtractOutput { warnings, ..Default::default() };
        out.stats.insert("edges_read".into(), total as u64);
        out.stats.insert("edges_kept".into(), edges.len() as u64);
        out.edges = edges;
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
}
