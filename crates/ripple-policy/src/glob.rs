//! Glob handling shared by every pattern list in `ripple.yml`.
//!
//! Patterns are matched against repo-relative paths with gitignore-like
//! conveniences:
//! * `*` and `?` never cross `/`; `**` does.
//! * A pattern without a `/` matches at any depth (`*.log` == `**/*.log`).
//!   A leading `/` anchors it to the root instead (`/Gemfile`).
//! * A pattern also matches everything below it when it names a directory
//!   (`app/models/billing` matches `app/models/billing/invoice.rb`).
//!   A trailing `/` restricts it to that (`tmp/`).

use crate::error::Issue;
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};

/// Expands one user pattern into the globs actually compiled.
pub(crate) fn expand(pattern: &str) -> Result<Vec<String>, String> {
    let mut p = pattern.trim();
    if let Some(rest) = p.strip_prefix("./") {
        p = rest;
    }
    let anchored = p.starts_with('/');
    let p = p.trim_start_matches('/');
    let dir_only = p.ends_with('/');
    let p = p.trim_end_matches('/');
    if p.is_empty() {
        return Err("empty pattern".into());
    }
    let base = if !anchored && !p.contains('/') { format!("**/{p}") } else { p.to_string() };
    Ok(if dir_only {
        vec![format!("{base}/**")]
    } else if base.ends_with("/**") || base == "**" {
        vec![base]
    } else {
        vec![format!("{base}/**"), base]
    })
}

fn compile_one(glob: &str) -> Result<Glob, String> {
    GlobBuilder::new(glob).literal_separator(true).backslash_escape(true).build().map_err(|e| e.kind().to_string())
}

/// A compiled pattern list. `owner[i]` maps compiled glob `i` back to the
/// index of the user pattern it came from.
#[derive(Debug, Clone)]
pub(crate) struct Patterns {
    set: GlobSet,
    owner: Vec<usize>,
}

impl Default for Patterns {
    fn default() -> Self {
        Self { set: GlobSet::empty(), owner: Vec::new() }
    }
}

impl Patterns {
    /// Compiles `patterns`; `tag(i)` gives the owner id recorded for pattern `i`.
    /// Errors are reported against `key[i]`.
    pub(crate) fn build<'a>(
        patterns: impl IntoIterator<Item = (&'a str, usize, String)>,
        issues: &mut Vec<Issue>,
    ) -> Self {
        let mut builder = GlobSetBuilder::new();
        let mut owner = Vec::new();
        for (pattern, tag, key) in patterns {
            match expand(pattern) {
                Err(e) => issues.push(Issue::new(key, format!("invalid glob `{pattern}`: {e}"))),
                Ok(globs) => {
                    for g in globs {
                        match compile_one(&g) {
                            Ok(glob) => {
                                builder.add(glob);
                                owner.push(tag);
                            }
                            Err(e) => {
                                issues.push(Issue::new(key.clone(), format!("invalid glob `{pattern}`: {e}")));
                                break;
                            }
                        }
                    }
                }
            }
        }
        match builder.build() {
            Ok(set) => Self { set, owner },
            Err(e) => {
                issues.push(Issue::new("<globs>", e.to_string()));
                Self::default()
            }
        }
    }

    /// Simple list: every pattern's owner is its own index; keys are `key[i]`.
    pub(crate) fn list(key: &str, patterns: &[String], issues: &mut Vec<Issue>) -> Self {
        Self::build(patterns.iter().enumerate().map(|(i, p)| (p.as_str(), i, format!("{key}[{i}]"))), issues)
    }

    pub(crate) fn is_match(&self, path: &str) -> bool {
        !self.owner.is_empty() && self.set.is_match(path)
    }

    /// Owner ids of the patterns matching `path`, sorted and deduplicated.
    pub(crate) fn owners_matching(&self, path: &str, scratch: &mut Vec<usize>) -> Vec<usize> {
        if self.owner.is_empty() {
            return Vec::new();
        }
        self.set.matches_into(path, scratch);
        let mut out: Vec<usize> = scratch.iter().map(|&i| self.owner[i]).collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.owner.is_empty()
    }
}

/// A user-supplied list of globs (`ripple rank --path`), with the same
/// syntax as `ripple.yml`.
#[derive(Debug, Clone, Default)]
pub struct PathFilter(Patterns);

impl PathFilter {
    pub fn new(patterns: &[String]) -> Result<Self, String> {
        let mut issues = Vec::new();
        let p = Patterns::list("path", patterns, &mut issues);
        match issues.first() {
            Some(i) => Err(i.message.clone()),
            None => Ok(Self(p)),
        }
    }
    /// True for every path when the list is empty.
    pub fn is_match(&self, path: &str) -> bool {
        self.0.is_empty() || self.0.is_match(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, path: &str) -> bool {
        let mut issues = Vec::new();
        let p = Patterns::list("t", &[pattern.to_string()], &mut issues);
        assert!(issues.is_empty(), "{issues:?}");
        p.is_match(path)
    }

    #[test]
    fn gitignore_like_semantics() {
        assert!(m("app/models/billing/**", "app/models/billing/invoice.rb"));
        assert!(m("app/models/billing", "app/models/billing/x/invoice.rb"));
        assert!(!m("app/models/billing", "app/models/billing_old.rb"));
        assert!(m("*.log", "a/b/c.log"));
        assert!(m("Gemfile", "Gemfile"));
        assert!(m("Gemfile", "engines/x/Gemfile"));
        assert!(!m("/Gemfile", "engines/x/Gemfile"));
        assert!(m("/Gemfile", "Gemfile"));
        assert!(!m("app/*.rb", "app/models/user.rb"));
        assert!(m("app/**/*.rb", "app/models/user.rb"));
        assert!(m("**/*_spec.rb", "user_spec.rb"));
        assert!(m("tmp/", "tmp/cache/x"));
        assert!(m("./config/routes.rb", "config/routes.rb"));
        assert!(m("app/{models,services}/pay*.rb", "app/services/payment.rb"));
    }

    #[test]
    fn bad_globs_are_reported_with_key() {
        let mut issues = Vec::new();
        Patterns::list("tests", &["ok/**".into(), "bad/[x".into(), "".into()], &mut issues);
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].key, "tests[1]");
        assert!(issues[0].message.contains("bad/[x"), "{}", issues[0].message);
        assert_eq!(issues[1].key, "tests[2]");
    }
}
