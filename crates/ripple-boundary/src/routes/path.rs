//! Rails route path normalisation (`ActionDispatch::Routing::Mapper.normalize_path`).

/// `Journey::Router::Utils.normalize_path` followed by Mapper's handling of
/// optional groups, minus the format suffix (which we never store):
///
/// * leading slash added, repeated slashes squeezed, trailing slash dropped;
/// * a slash before an optional group moves inside it: `/a/(*b)` -> `/a(/*b)`;
/// * a trailing `(.:format)` is removed.
pub fn normalize(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 1);
    out.push('/');
    for c in path.chars() {
        if c == '/' && out.ends_with('/') {
            continue;
        }
        out.push(c);
    }
    if out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    // `/(` and `/(/` -> `(/`, for any run of opening parens.
    let mut moved = String::with_capacity(out.len());
    let bytes: Vec<char> = out.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '/' && bytes.get(i + 1) == Some(&'(') {
            let mut j = i + 1;
            while bytes.get(j) == Some(&'(') {
                moved.push('(');
                j += 1;
            }
            moved.push('/');
            if bytes.get(j) == Some(&'/') {
                j += 1;
            }
            i = j;
            continue;
        }
        moved.push(bytes[i]);
        i += 1;
    }
    // A path made only of optional groups stays as `(/:locale)`; matching
    // treats the empty expansion as `/`.
    while let Some(stripped) = moved.strip_suffix("(.:format)") {
        moved = stripped.to_string();
    }
    if moved.len() > 1 && moved.ends_with('/') {
        moved.pop();
    }
    if moved.is_empty() {
        moved.push('/');
    }
    moved
}

/// Joins a scope path and a child path the way `merge_path_scope` does.
pub fn join(parent: &str, child: &str) -> String {
    normalize(&format!("{parent}/{child}"))
}

/// `Mapper.normalize_name`: a path-ish name prefix becomes an identifier.
pub fn normalize_name(name: &str) -> String {
    let p = normalize(name);
    p.trim_start_matches('/').replace('/', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_like_rails() {
        assert_eq!(normalize(""), "/");
        assert_eq!(normalize("/"), "/");
        assert_eq!(normalize("//api//v1/"), "/api/v1");
        assert_eq!(normalize("/media_proxy/:id/(*any)"), "/media_proxy/:id(/*any)");
        assert_eq!(normalize("/oauth/register(.:format)"), "/oauth/register");
        assert_eq!(normalize("health_check(/:checks)"), "/health_check(/:checks)");
        assert_eq!(normalize("(/*full_path)"), "(/*full_path)");
        assert_eq!(join("/api", "/oembed"), "/api/oembed");
        assert_eq!(normalize_name("sessions/security_key_options"), "sessions_security_key_options");
    }
}
