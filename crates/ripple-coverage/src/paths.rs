//! Mapping report paths (absolute, relative to a CI checkout, relative to a
//! JS package) onto repo-relative paths. See the crate docs for the rule.

use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// `/`-separated, `.` and empty components dropped, `..` resolved, no
/// leading or trailing `/`. Returns whether the input was absolute
/// (`/...`, `C:\...`, `file:///...`).
pub(crate) fn normalize(raw: &str) -> (bool, String) {
    let s = raw.trim();
    let s = s.strip_prefix("file://").unwrap_or(s);
    let b = s.as_bytes();
    let abs = s.starts_with('/') || s.starts_with('\\') || (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic());
    let simple = !s.contains('\\') && !s.contains("//") && !s.contains("./") && !s.ends_with('/') && !s.ends_with("/.") && s != "." && s != "..";
    if simple {
        return (abs, s.strip_prefix('/').unwrap_or(s).to_string());
    }
    let mut parts: Vec<&str> = Vec::new();
    for c in s.split(['/', '\\']) {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    (abs, parts.join("/"))
}

fn join(base: &str, rel: &str) -> String {
    if base.is_empty() {
        rel.to_string()
    } else if rel.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{rel}")
    }
}

/// `p` without the leading components `prefix` (component-wise).
fn strip_components<'a>(p: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix.is_empty() {
        return Some(p);
    }
    p.strip_prefix(prefix)?.strip_prefix('/')
}

/// Number of equal trailing `/`-components.
fn common_suffix(a: &str, b: &str) -> usize {
    a.rsplit('/').zip(b.rsplit('/')).take_while(|(x, y)| x == y).count()
}

/// Byte index where the last `n` components of `p` start (0 = all of it).
fn tail_start(p: &str, n: usize) -> usize {
    if n == 0 {
        return p.len();
    }
    let mut seen = 0;
    for (i, ch) in p.bytes().enumerate().rev() {
        if ch == b'/' {
            seen += 1;
            if seen == n {
                return i + 1;
            }
        }
    }
    0
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Mapped(String),
    Unmapped,
    /// Several repo files fit equally well.
    Ambiguous,
}

enum Suffix<'k> {
    None,
    Unique(&'k str, usize),
    Ambiguous,
}

pub(crate) struct Mapper<'k> {
    known: HashSet<&'k str>,
    /// File name -> known files with it. Built on first use (only relative
    /// report paths need it).
    by_name: OnceCell<HashMap<&'k str, Vec<&'k str>>>,
    /// Normalized repo root (as given, and canonicalized), without leading `/`.
    roots: Vec<String>,
}

impl<'k> Mapper<'k> {
    pub(crate) fn new(root: &Path, known_files: impl IntoIterator<Item = &'k str>) -> Self {
        let known: HashSet<&'k str> = known_files.into_iter().filter(|f| !f.is_empty()).collect();
        let mut roots = vec![normalize(&root.to_string_lossy()).1];
        if let Ok(c) = root.canonicalize() {
            let c = normalize(&c.to_string_lossy()).1;
            if !roots.contains(&c) {
                roots.push(c);
            }
        }
        roots.retain(|r| !r.is_empty());
        Self { known, by_name: OnceCell::new(), roots }
    }

    fn by_name(&self) -> &HashMap<&'k str, Vec<&'k str>> {
        self.by_name.get_or_init(|| {
            let mut m: HashMap<&'k str, Vec<&'k str>> = HashMap::with_capacity(self.known.len());
            for &f in &self.known {
                m.entry(f.rsplit('/').next().unwrap_or(f)).or_default().push(f);
            }
            m
        })
    }

    /// No known files were given: accept any path under the root.
    fn trusting(&self) -> bool {
        self.known.is_empty()
    }

    fn is_known(&self, p: &str) -> bool {
        !p.is_empty() && (self.trusting() || self.known.contains(p))
    }

    fn known_ref(&self, p: &str) -> Option<&'k str> {
        self.known.get(p).copied()
    }

    /// Best suffix match. One path must be entirely a component-wise suffix
    /// of the other: `/ci/app/x.rb` fits `app/x.rb` but not `ee/app/x.rb`.
    /// The longest match wins; equally long ones are ambiguous.
    fn suffix(&self, norm: &str, abs: bool) -> Suffix<'k> {
        // A relative report path that is the tail of longer repo paths
        // (`src/x.js` of `packages/web/src/x.js`): those are longer than any
        // repo path that is a tail of the report path.
        if !abs {
            let name = norm.rsplit('/').next().unwrap_or(norm);
            let report_len = norm.split('/').count();
            let mut found = None;
            for &c in self.by_name().get(name).map(Vec::as_slice).unwrap_or(&[]) {
                if c.len() > norm.len() && common_suffix(norm, c) == report_len {
                    if found.is_some() {
                        return Suffix::Ambiguous;
                    }
                    found = Some(c);
                }
            }
            if let Some(c) = found {
                return Suffix::Unique(c, report_len);
            }
        }
        // The longest repo path that is a tail of the report path: drop
        // leading components until one is known (unique by construction).
        let mut i = 0;
        loop {
            let tail = &norm[i..];
            if let Some(k) = self.known_ref(tail) {
                return Suffix::Unique(k, tail.split('/').count());
            }
            match tail.find('/') {
                Some(j) => i += j + 1,
                None => return Suffix::None,
            }
        }
    }

    /// Map the entries of one report. `base` is the repo-relative directory
    /// the report belongs to (the parent of its `coverage/` directory), tried
    /// first for relative paths.
    pub(crate) fn resolve(&self, raws: &[&str], base: &str) -> Vec<Outcome> {
        enum Pending<'k> {
            Done(Outcome),
            Later(String, Suffix<'k>),
        }
        let mut pending = Vec::with_capacity(raws.len());
        let mut votes: HashMap<(String, String), u32> = HashMap::new();
        // The rewrite of the previous unique match, tried first: one lookup
        // per entry for the common case of one foreign checkout root.
        let mut last: Option<(String, String)> = None;
        for raw in raws {
            let (abs, norm) = normalize(raw);
            if abs {
                if let Some(rel) = self.roots.iter().find_map(|r| strip_components(&norm, r)) {
                    let o = if self.is_known(rel) { Outcome::Mapped(rel.to_string()) } else { Outcome::Unmapped };
                    pending.push(Pending::Done(o));
                    continue;
                }
            } else {
                let joined = join(base, &norm);
                if self.is_known(&joined) {
                    pending.push(Pending::Done(Outcome::Mapped(joined)));
                    continue;
                }
                if self.is_known(&norm) {
                    pending.push(Pending::Done(Outcome::Mapped(norm)));
                    continue;
                }
            }
            if self.trusting() || norm.is_empty() {
                pending.push(Pending::Done(Outcome::Unmapped));
                continue;
            }
            if let Some(ab) = &last
                && let Some(rest) = strip_components(&norm, &ab.0)
                && let Some(k) = self.known_ref(&join(&ab.1, rest))
            {
                // Same as `suffix` would find unless a longer match exists,
                // which the final pass (rule first) would override anyway.
                *votes.entry(ab.clone()).or_default() += 1;
                let n = rest.split('/').count();
                pending.push(Pending::Later(norm, Suffix::Unique(k, n)));
                continue;
            }
            let s = self.suffix(&norm, abs);
            if let Suffix::Unique(c, n) = s {
                // Learn "strip A, prepend B" from unique matches: `A` is the
                // other checkout's root, `B` a package directory.
                let a = norm[..tail_start(&norm, n)].trim_end_matches('/');
                let b = c[..tail_start(c, n)].trim_end_matches('/');
                let ab = (a.to_string(), b.to_string());
                *votes.entry(ab.clone()).or_default() += 1;
                last = Some(ab);
            }
            pending.push(Pending::Later(norm, s));
        }
        // The most frequent rewrite; ties go to the longest stripped prefix.
        let rule = votes.into_iter().max_by(|x, y| x.1.cmp(&y.1).then(x.0.0.len().cmp(&y.0.0.len())).then(y.0.cmp(&x.0))).map(|(k, _)| k);
        pending
            .into_iter()
            .map(|p| match p {
                Pending::Done(o) => o,
                Pending::Later(norm, s) => {
                    if let Some((a, b)) = &rule
                        && let Some(rest) = strip_components(&norm, a)
                    {
                        let cand = join(b, rest);
                        if self.is_known(&cand) {
                            return Outcome::Mapped(cand);
                        }
                        if !a.is_empty() {
                            // Under the other checkout's root but not here:
                            // a file deleted since, not a near-namesake.
                            return Outcome::Unmapped;
                        }
                    }
                    match s {
                        Suffix::Unique(c, _) => Outcome::Mapped(c.to_string()),
                        Suffix::Ambiguous => Outcome::Ambiguous,
                        Suffix::None => Outcome::Unmapped,
                    }
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_forms() {
        assert_eq!(normalize("/a/b/c.rb"), (true, "a/b/c.rb".into()));
        assert_eq!(normalize("./a//b/../c.rb"), (false, "a/c.rb".into()));
        assert_eq!(normalize("C:\\ci\\app\\x.rb"), (true, "C:/ci/app/x.rb".into()));
        assert_eq!(normalize("file:///w/app/x.rb"), (true, "w/app/x.rb".into()));
        assert_eq!(normalize("src/x.js"), (false, "src/x.js".into()));
    }

    #[test]
    fn tail() {
        assert_eq!(&"a/b/c"[tail_start("a/b/c", 2)..], "b/c");
        assert_eq!(&"a/b/c"[tail_start("a/b/c", 3)..], "a/b/c");
        assert_eq!(common_suffix("x/app/m.rb", "app/m.rb"), 2);
    }
}
