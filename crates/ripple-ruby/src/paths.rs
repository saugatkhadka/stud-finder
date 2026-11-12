//! Path conventions: which files we read, Zeitwerk autoload roots, and the
//! Rails directory layout (controllers, mailers, components, views).

/// The template languages whose embedded Ruby we scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateLang {
    Erb,
    Haml,
    Slim,
    /// `.jbuilder` / `.builder`: the whole file is Ruby.
    Ruby,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// Indexed by Rubydex.
    Ruby,
    Template(TemplateLang),
}

pub fn classify(path: &str) -> Option<SourceKind> {
    let name = file_name(path);
    if name == "Rakefile" {
        return Some(SourceKind::Ruby);
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e)?;
    match ext {
        "rb" | "rake" | "ru" | "gemspec" => Some(SourceKind::Ruby),
        "erb" => Some(SourceKind::Template(TemplateLang::Erb)),
        "haml" => Some(SourceKind::Template(TemplateLang::Haml)),
        "slim" => Some(SourceKind::Template(TemplateLang::Slim)),
        "jbuilder" | "builder" => Some(SourceKind::Template(TemplateLang::Ruby)),
        _ => None,
    }
}

/// A Ruby file in a test tree: a `spec/` or `test/` directory at the root or
/// below (`ee/spec/features/x_spec.rb`).
pub fn is_test_path(path: &str) -> bool {
    path.ends_with(".rb") && path.split('/').rev().skip(1).any(|seg| seg == "spec" || seg == "test")
}

#[cfg(test)]
#[test]
fn test_paths() {
    assert!(is_test_path("spec/requests/a_spec.rb"));
    assert!(is_test_path("ee/spec/features/a_spec.rb"));
    assert!(is_test_path("test/integration/a_test.rb"));
    assert!(!is_test_path("app/models/spec.rb"));
    assert!(!is_test_path("app/models/test_run.rb"));
    assert!(!is_test_path("spec/javascripts/a.js"));
}

pub fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, n)| n)
}

pub fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

/// File name up to the first dot: `show.html.haml` -> `show`.
pub fn stem(path: &str) -> &str {
    let name = file_name(path);
    name.split_once('.').map_or(name, |(s, _)| s)
}

/// Case- and underscore-insensitive form of a constant path, used to compare
/// constant names against file paths without knowing the app's acronym
/// inflections (`Gitlab::HTTPConnection` and `gitlab/http_connection.rb`
/// both become `gitlab::httpconnection`).
pub fn normalize_const(fqn: &str) -> String {
    let fqn = fqn.strip_prefix("::").unwrap_or(fqn);
    fqn.chars()
        .filter(|c| *c != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Every constant name (normalized) that Zeitwerk could expect `path` to
/// define, one per autoload root containing it. Roots: any `**/app/<dir>/`,
/// `**/app/<dir>/concerns/`, any `**/lib/`, plus `extra_roots`.
pub fn zeitwerk_keys(path: &str, extra_roots: &[String]) -> Vec<String> {
    let Some(no_ext) = path.strip_suffix(".rb") else {
        return Vec::new();
    };
    let segs: Vec<&str> = no_ext.split('/').collect();
    let mut keys = Vec::new();
    let mut push = |rest: &[&str]| {
        if !rest.is_empty() {
            keys.push(normalize_const(&rest.join("::")));
        }
    };
    for i in 0..segs.len() {
        match segs[i] {
            "app" if i + 2 < segs.len() => {
                push(&segs[i + 2..]);
                if segs[i + 2] == "concerns" {
                    push(&segs[i + 3..]);
                }
            }
            "lib" if i + 1 < segs.len() => push(&segs[i + 1..]),
            _ => {}
        }
    }
    for root in extra_roots {
        let root = root.trim_end_matches('/');
        if let Some(rest) = no_ext.strip_prefix(root).and_then(|r| r.strip_prefix('/')) {
            push(&rest.split('/').collect::<Vec<_>>());
        }
    }
    keys.sort();
    keys.dedup();
    keys
}

/// `foo/app/<kind>/rest` -> `rest` for the first `app/<kind>/` segment pair.
fn under_app_dir<'a>(path: &'a str, kind: &str) -> Option<&'a str> {
    // First `app/<kind>/` that starts the path or follows a `/`; no allocation.
    let mut from = 0;
    while let Some(i) = path[from..].find("app/").map(|i| i + from) {
        if i == 0 || path.as_bytes()[i - 1] == b'/' {
            let rest = &path[i + 4..];
            if let Some(r) = rest.strip_prefix(kind).and_then(|r| r.strip_prefix('/')) {
                return Some(r);
            }
        }
        from = i + 4;
    }
    None
}

/// Controller view prefix: `ee/app/controllers/admin/users_controller.rb` -> `admin/users`.
pub fn controller_view_prefix(path: &str) -> Option<&str> {
    let rest = under_app_dir(path, "controllers")?;
    if rest.starts_with("concerns/") {
        return None;
    }
    rest.strip_suffix("_controller.rb")
}

/// A controller whose edges get action scopes: under `**/app/controllers/`,
/// a `.rb` file, and not inside any `concerns/` directory.
pub fn is_scoped_controller(path: &str) -> bool {
    under_app_dir(path, "controllers")
        .is_some_and(|rest| path.ends_with(".rb") && !rest.split('/').any(|seg| seg == "concerns"))
}

/// Mailer view prefix: `app/mailers/user_mailer.rb` -> `user_mailer`.
pub fn mailer_view_prefix(path: &str) -> Option<&str> {
    let rest = under_app_dir(path, "mailers")?;
    if rest.starts_with("concerns/") {
        return None;
    }
    rest.strip_suffix(".rb")
}

/// View-component Ruby file: `app/components/foo/bar_component.rb`.
pub fn is_component(path: &str) -> bool {
    under_app_dir(path, "components").is_some() && path.ends_with(".rb")
}

/// Splits a template path into (view root incl. trailing `app/views/`, path under it).
pub fn split_view_path(path: &str) -> Option<(&str, &str)> {
    let rest = under_app_dir(path, "views")?;
    Some((&path[..path.len() - rest.len()], rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(
            zeitwerk_keys("app/models/concerns/foo_bar.rb", &[]),
            vec!["concerns::foobar", "foobar"]
        );
        assert_eq!(
            zeitwerk_keys("ee/app/models/ee/user.rb", &[]),
            vec!["ee::user"]
        );
        assert_eq!(
            zeitwerk_keys("lib/gitlab/http.rb", &[]),
            vec!["gitlab::http"]
        );
        assert_eq!(zeitwerk_keys("app/models/user.rb", &[]), vec!["user"]);
        assert_eq!(zeitwerk_keys("config/x.rb", &["config".into()]), vec!["x"]);
        assert_eq!(
            normalize_const("::Gitlab::HTTPConnection"),
            "gitlab::httpconnection"
        );
    }

    #[test]
    fn rails_dirs() {
        assert_eq!(
            controller_view_prefix("ee/app/controllers/admin/users_controller.rb"),
            Some("admin/users")
        );
        assert_eq!(
            controller_view_prefix("app/controllers/concerns/x_controller.rb"),
            None
        );
        assert_eq!(mailer_view_prefix("app/mailers/notify.rb"), Some("notify"));
        assert!(is_scoped_controller("ee/app/controllers/admin/users_controller.rb"));
        assert!(!is_scoped_controller("app/controllers/concerns/x.rb"));
        assert!(!is_scoped_controller("app/controllers/api/concerns/x.rb"));
        assert!(!is_scoped_controller("app/models/user.rb"));
        assert_eq!(
            split_view_path("ee/app/views/a/_b.html.haml"),
            Some(("ee/app/views/", "a/_b.html.haml"))
        );
        assert_eq!(stem("x/_b.html.haml"), "_b");
    }
}
