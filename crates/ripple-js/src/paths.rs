//! Lexical repo-relative path helpers (forward slashes, no leading `./`, `""` = repo root).

/// Joins `rel` onto `base` and resolves `.` / `..` lexically.
/// `None` if the result escapes the repo root.
pub fn join(base: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = if rel.starts_with('/') { Vec::new() } else { base.split('/').filter(|s| !s.is_empty()).collect() };
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

pub fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

pub fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, n)| n)
}

/// Extensions we parse as source.
pub const CODE_EXTENSIONS: [&str; 9] = ["js", "jsx", "ts", "tsx", "mjs", "cjs", "mts", "cts", "vue"];

pub fn extension(path: &str) -> &str {
    let name = file_name(path);
    name.rsplit_once('.').map_or("", |(_, e)| e)
}

pub fn is_code(path: &str) -> bool {
    CODE_EXTENSIONS.contains(&extension(path))
}

/// True for files whose change can alter how specifiers resolve.
pub fn is_config_name(path: &str) -> bool {
    let name = file_name(path);
    name == "package.json" || name == "pnpm-workspace.yaml" || (name.ends_with(".json") && (name.starts_with("tsconfig") || name.starts_with("jsconfig")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins() {
        assert_eq!(join("a/b", "../c").as_deref(), Some("a/c"));
        assert_eq!(join("a", "./x/./y").as_deref(), Some("a/x/y"));
        assert_eq!(join("", "../x"), None);
        assert_eq!(join("a/b", "/x").as_deref(), Some("x"));
    }
}
