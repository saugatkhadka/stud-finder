use std::fmt;
use std::path::PathBuf;

/// One problem found while validating a config. `key` is a dotted path into
/// the YAML document (`rules[2].when.touches[0]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub key: String,
    pub line: Option<usize>,
    pub message: String,
}

impl Issue {
    pub fn new(key: impl Into<String>, message: impl Into<String>) -> Self {
        Self { key: key.into(), line: None, message: message.into() }
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "{} (line {line}): {}", self.key, self.message),
            None => write!(f, "{}: {}", self.key, self.message),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("cannot read {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The YAML is malformed or does not match the schema (unknown key, wrong type).
    #[error("{origin}: {}", fmt_parse(key, *line, *column, message))]
    Parse { origin: String, key: Option<String>, line: Option<usize>, column: Option<usize>, message: String },
    /// The YAML parsed but is inconsistent (unknown area, bad glob, duplicate rule id, ...).
    #[error("{origin}: invalid configuration:\n{}", issues.iter().map(|i| format!("  - {i}")).collect::<Vec<_>>().join("\n"))]
    Invalid { origin: String, issues: Vec<Issue> },
}

fn fmt_parse(key: &Option<String>, line: Option<usize>, column: Option<usize>, message: &str) -> String {
    let mut out = String::new();
    if let Some(line) = line {
        out.push_str(&format!("line {line}"));
        if let Some(col) = column {
            out.push_str(&format!(", column {col}"));
        }
        out.push_str(": ");
    }
    if let Some(key) = key.as_deref().filter(|k| !k.is_empty() && *k != ".") {
        out.push_str(&format!("at `{key}`: "));
    }
    out.push_str(message);
    out
}
