//! A small table-driven subset of ActiveSupport's inflector: enough to turn an
//! association name into a class name.
//!
//! Known limits (vs. ActiveSupport):
//! - singular rules are a curated subset of the English defaults (no `ox/oxen`,
//!   `matrix/matrices` style Latin beyond the common ones listed below);
//! - only `acronym`, `irregular` and `uncountable` are read from the app's
//!   `config/initializers/inflections.rb`, and only when written with literal
//!   strings; `plural`/`singular` regex rules there are ignored;
//! - acronyms are matched per underscore-separated word only (Rails also
//!   matches acronyms spanning words, e.g. `acronym "RESTful"`).

use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Default)]
pub struct Inflector {
    /// lowercase -> canonical spelling (`oauth` -> `OAuth`).
    acronyms: HashMap<String, String>,
    /// plural -> singular.
    irregular: HashMap<String, String>,
    uncountable: HashSet<String>,
}

/// (plural suffix, singular replacement), checked in order; first match wins.
const SINGULAR_RULES: &[(&str, &str)] = &[
    ("quizzes", "quiz"),
    ("matrices", "matrix"),
    ("vertices", "vertex"),
    ("indices", "index"),
    ("aliases", "alias"),
    ("statuses", "status"),
    ("octopi", "octopus"),
    ("viri", "virus"),
    ("axes", "axis"),
    ("crises", "crisis"),
    ("testes", "testis"),
    ("analyses", "analysis"),
    ("bases", "basis"),
    ("diagnoses", "diagnosis"),
    ("theses", "thesis"),
    ("shoes", "shoe"),
    ("buses", "bus"),
    ("mice", "mouse"),
    ("lice", "louse"),
    ("movies", "movie"),
    ("series", "series"),
    ("news", "news"),
    ("species", "species"),
    ("hives", "hive"),
    ("tives", "tive"),
    ("oes", "o"),
    ("xes", "x"),
    ("ches", "ch"),
    ("sses", "ss"),
    ("shes", "sh"),
    ("quies", "quy"),
    ("ies", "y"),
    ("lves", "lf"),
    ("rves", "rf"),
    ("ves", "fe"),
    ("ta", "tum"),
    ("ia", "ium"),
    ("ss", "ss"),
    ("sis", "sis"),
    ("status", "status"),
    ("alias", "alias"),
    ("bus", "bus"),
    ("virus", "virus"),
    ("octopus", "octopus"),
    ("s", ""),
];

const IRREGULAR: &[(&str, &str)] = &[
    ("people", "person"),
    ("men", "man"),
    ("women", "woman"),
    ("children", "child"),
    ("sexes", "sex"),
    ("moves", "move"),
    ("zombies", "zombie"),
    ("databases", "database"),
];

const UNCOUNTABLE: &[&str] = &[
    "equipment",
    "information",
    "rice",
    "money",
    "species",
    "series",
    "fish",
    "sheep",
    "jeans",
    "police",
];

impl Inflector {
    pub fn new() -> Self {
        Self {
            acronyms: HashMap::new(),
            irregular: IRREGULAR
                .iter()
                .map(|(p, s)| (p.to_string(), s.to_string()))
                .collect(),
            uncountable: UNCOUNTABLE.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Reads literal `inflect.acronym "X"`, `inflect.irregular "a", "b"` and
    /// `inflect.uncountable %w(a b)` / `"a"` declarations. Tolerant: anything it
    /// does not understand is ignored.
    pub fn with_app_rules(mut self, source: &str) -> Self {
        for line in source.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            if let Some(rest) = after_call(line, "acronym") {
                for word in quoted_words(rest) {
                    self.acronyms.insert(word.to_lowercase(), word);
                }
            } else if let Some(rest) = after_call(line, "irregular") {
                let words = quoted_words(rest);
                if let [singular, plural, ..] = words.as_slice() {
                    self.irregular
                        .insert(plural.to_lowercase(), singular.to_lowercase());
                }
            } else if let Some(rest) = after_call(line, "uncountable") {
                self.uncountable
                    .extend(quoted_words(rest).into_iter().map(|w| w.to_lowercase()));
            }
        }
        self
    }

    pub fn singularize(&self, word: &str) -> String {
        // Only the last underscore-separated word is inflected (`user_roles` -> `user_role`).
        let (head, last) = match word.rsplit_once('_') {
            Some((h, l)) => (Some(h), l),
            None => (None, word),
        };
        let lower = last.to_lowercase();
        let singular = if self.uncountable.contains(&lower) {
            last.to_string()
        } else if let Some(s) = self.irregular.get(&lower) {
            s.clone()
        } else {
            SINGULAR_RULES
                .iter()
                .find(|(suffix, _)| lower.ends_with(suffix))
                .map(|(suffix, rep)| format!("{}{}", &last[..last.len() - suffix.len()], rep))
                .unwrap_or_else(|| last.to_string())
        };
        match head {
            Some(h) => format!("{h}_{singular}"),
            None => singular,
        }
    }

    /// `admin/user_profile` -> `Admin::UserProfile`, honouring acronyms.
    pub fn camelize(&self, s: &str) -> String {
        s.split('/')
            .map(|seg| {
                seg.split('_')
                    .filter(|w| !w.is_empty())
                    .map(|w| match self.acronyms.get(&w.to_lowercase()) {
                        Some(a) => a.clone(),
                        None => {
                            let mut c = w.chars();
                            c.next()
                                .map(|f| f.to_uppercase().chain(c).collect())
                                .unwrap_or_default()
                        }
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("::")
    }
}

fn after_call<'a>(line: &'a str, method: &str) -> Option<&'a str> {
    let idx = line.find(&format!(".{method}"))?;
    Some(&line[idx + method.len() + 1..])
}

/// Words in `'a'`, `"a"`, or `%w(a b)` forms.
fn quoted_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(i) = s.find("%w") {
        let body = &s[i + 3..];
        let end = body.find([')', ']', '}']).unwrap_or(body.len());
        out.extend(body[..end].split_whitespace().map(String::from));
        return out;
    }
    let mut i = 0;
    while let Some(off) = s[i..].find(['"', '\'']) {
        let open = i + off;
        let quote = &s[open..open + 1];
        match s[open + 1..].find(quote) {
            Some(len) => {
                out.push(s[open + 1..open + 1 + len].to_string());
                i = open + 1 + len + 1;
            }
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn singular() {
        let i = Inflector::new();
        for (p, s) in [
            ("users", "user"),
            ("statuses", "status"),
            ("categories", "category"),
            ("addresses", "address"),
            ("people", "person"),
            ("media_attachments", "media_attachment"),
            ("boxes", "box"),
            ("matches", "match"),
            ("account_aliases", "account_alias"),
            ("status", "status"),
            ("wives", "wife"),
            ("shelves", "shelf"),
            ("series", "series"),
            ("news", "news"),
        ] {
            assert_eq!(i.singularize(p), s, "{p}");
        }
    }

    #[test]
    fn camel() {
        let i = Inflector::new().with_app_rules("ActiveSupport::Inflector.inflections(:en) do |inflect|\n  inflect.acronym 'OAuth'\n  inflect.irregular 'cactus', 'cacti'\nend\n");
        assert_eq!(i.camelize("oauth_application"), "OAuthApplication");
        assert_eq!(i.camelize("admin/user_profile"), "Admin::UserProfile");
        assert_eq!(i.singularize("cacti"), "cactus");
    }
}
