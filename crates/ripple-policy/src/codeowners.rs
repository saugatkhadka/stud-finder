//! CODEOWNERS (GitHub / GitLab): who reviews which paths.
//!
//! Looked up at `.github/CODEOWNERS`, `CODEOWNERS`, `docs/CODEOWNERS`,
//! `.gitlab/CODEOWNERS` (first found). Each line is `pattern owner...`;
//! the last matching line wins. GitLab section headers (`[Section]`,
//! `^[Optional]`) and their default owners are skipped; a line without
//! owners clears ownership for its pattern, as on GitHub.

use crate::glob::Patterns;
use std::path::{Path, PathBuf};

pub const CODEOWNERS_LOCATIONS: [&str; 4] = [".github/CODEOWNERS", "CODEOWNERS", "docs/CODEOWNERS", ".gitlab/CODEOWNERS"];

#[derive(Debug, Clone)]
pub struct CodeOwners {
    /// Repo-relative path of the file read.
    pub path: PathBuf,
    patterns: Patterns,
    /// Owners per line (index = pattern id), in file order.
    owners: Vec<Vec<String>>,
    /// Lines that could not be compiled.
    pub skipped: usize,
}

impl CodeOwners {
    /// The first CODEOWNERS file under `root`, parsed; `None` when there is none.
    pub fn find(root: &Path) -> Option<Self> {
        CODEOWNERS_LOCATIONS.iter().find_map(|rel| {
            let src = std::fs::read_to_string(root.join(rel)).ok()?;
            Some(Self::parse(&src, PathBuf::from(rel)))
        })
    }

    pub fn parse(src: &str, path: PathBuf) -> Self {
        let mut lines: Vec<(String, Vec<String>)> = Vec::new();
        for line in src.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('[') || line.starts_with("^[") {
                continue;
            }
            let line = line.split(" #").next().unwrap_or(line);
            let mut parts = line.split_whitespace();
            let Some(pat) = parts.next() else { continue };
            let owners = parts.map(str::to_string).collect();
            lines.push((pat.replace("\\ ", " "), owners));
        }
        let mut skipped = 0;
        let mut owners = Vec::new();
        let mut keep = Vec::new();
        for (pat, o) in lines {
            if crate::glob::expand(&pat).is_err() {
                skipped += 1;
                continue;
            }
            keep.push((pat, owners.len()));
            owners.push(o);
        }
        let mut issues = Vec::new();
        let patterns = Patterns::build(keep.iter().map(|(p, i)| (p.as_str(), *i, String::new())), &mut issues);
        skipped += issues.len();
        Self { path, patterns, owners, skipped }
    }

    /// Owners of `path`: those of the last matching line.
    pub fn owners_of(&self, path: &str) -> &[String] {
        let mut scratch = Vec::new();
        match self.patterns.owners_matching(path, &mut scratch).into_iter().max() {
            Some(i) => &self.owners[i],
            None => &[],
        }
    }

    pub fn rules(&self) -> usize {
        self.owners.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_match_wins_and_sections_are_skipped() {
        let c = CodeOwners::parse(
            "# comment\n* @everyone\n[Billing] @billing-lead\n/app/models/billing/ @pay @fin\n*.js @web\n/docs/\n^[Optional]\napp/models/billing/legacy.rb @old # trailing\n",
            "CODEOWNERS".into(),
        );
        assert_eq!(c.owners_of("README.md"), ["@everyone"]);
        assert_eq!(c.owners_of("app/models/billing/invoice.rb"), ["@pay", "@fin"]);
        assert_eq!(c.owners_of("app/models/billing/legacy.rb"), ["@old"]);
        assert_eq!(c.owners_of("app/javascript/a.js"), ["@web"]);
        assert!(c.owners_of("docs/x.md").is_empty());
    }
}
