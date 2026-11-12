//! Rails view lookup: which template files a controller/mailer/component owns,
//! and where `render "x"` / `render partial:` / `layout "x"` land.
//!
//! Lookup is approximated statically:
//! - partial names without a slash are looked up relative to the *template's*
//!   directory (Rails uses the rendering controller's prefix, which is the
//!   same dir for a controller's own views), then in `application/`;
//! - every `**/app/views/` root is searched (GitLab's `ee/app/views` is
//!   prepended to the view path), and all format/handler variants of the
//!   first directory level that has a match are returned.

use crate::paths;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RenderKind {
    /// `render "x"` / `render partial: "x"` in a view; `render partial:` in a controller.
    Partial,
    /// `render template: "x"`; `render "x"` in a controller.
    Template,
    /// `layout "x"` / `render layout: "x"` in a controller.
    Layout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Render {
    pub kind: RenderKind,
    pub name: String,
    pub line: u32,
}

/// Template files grouped by directory.
#[derive(Debug, Default)]
pub struct ViewIndex<'a> {
    /// Distinct `.../app/views/` roots, sorted (`app/views/` before `ee/app/views/`).
    roots: Vec<&'a str>,
    /// directory (no trailing slash) -> (stem, path), sorted by path.
    dirs: HashMap<&'a str, Vec<(&'a str, &'a str)>>,
}

impl<'a> ViewIndex<'a> {
    pub fn new(templates: impl Iterator<Item = &'a str>) -> Self {
        let mut idx = ViewIndex::default();
        for path in templates {
            if let Some((root, _)) = paths::split_view_path(path)
                && !idx.roots.contains(&root)
            {
                idx.roots.push(root);
            }
            idx.dirs
                .entry(paths::dir_of(path))
                .or_default()
                .push((paths::stem(path), path));
        }
        idx.roots.sort_unstable();
        for v in idx.dirs.values_mut() {
            v.sort_unstable_by(|a, b| a.1.cmp(b.1));
        }
        idx
    }

    /// Templates in an actual directory whose stem matches.
    fn in_dir<'s>(
        &'s self,
        dir: &str,
        stem: Option<&'s str>,
    ) -> impl Iterator<Item = &'a str> + 's {
        self.dirs
            .get(dir)
            .into_iter()
            .flatten()
            .filter(move |(s, _)| stem.is_none_or(|st| st == *s))
            .map(|(_, p)| *p)
    }

    /// Templates at `rel_dir` (relative to a view root) across all view roots.
    fn under_roots(&self, rel_dir: &str, stem: Option<&str>) -> Vec<&'a str> {
        let rel_dir = rel_dir.trim_matches('/');
        let mut out = Vec::new();
        for root in &self.roots {
            let dir = if rel_dir.is_empty() {
                root.trim_end_matches('/').to_string()
            } else {
                format!("{root}{rel_dir}")
            };
            out.extend(self.in_dir(&dir, stem));
        }
        out
    }

    /// All templates (incl. partials) directly in a controller/mailer view prefix.
    pub fn templates_for_prefix(&self, prefix: &str) -> Vec<&'a str> {
        self.under_roots(prefix, None)
    }

    pub fn layout(&self, name: &str) -> Vec<&'a str> {
        let (dir, stem) = split_name(name);
        let dir = if dir.is_empty() {
            "layouts".to_string()
        } else {
            format!("layouts/{dir}")
        };
        self.under_roots(&dir, Some(stem))
    }

    /// Partial lookup from a view in `current` (dir relative to the view root).
    pub fn partial(&self, current: &str, name: &str) -> Vec<&'a str> {
        self.lookup(current, name, true)
    }

    pub fn template(&self, current: &str, name: &str) -> Vec<&'a str> {
        self.lookup(current, name, false)
    }

    fn lookup(&self, current: &str, name: &str, partial: bool) -> Vec<&'a str> {
        let name = name.trim_start_matches('/');
        let (dir, stem) = split_name(name);
        let stem = if partial {
            format!("_{stem}")
        } else {
            stem.to_string()
        };
        if !dir.is_empty() {
            return self.under_roots(dir, Some(&stem));
        }
        for level in [current, "application"] {
            let found = self.under_roots(level, Some(&stem));
            if !found.is_empty() {
                return found;
            }
        }
        Vec::new()
    }

    /// View-component sidecar templates: `foo_component.html.erb` next to
    /// `foo_component.rb`, and anything in `foo_component/`.
    pub fn component_templates(&self, rb_path: &str) -> Vec<&'a str> {
        let dir = paths::dir_of(rb_path);
        let stem = paths::stem(rb_path);
        let sidecar = if dir.is_empty() {
            stem.to_string()
        } else {
            format!("{dir}/{stem}")
        };
        self.in_dir(dir, Some(stem))
            .chain(self.in_dir(&sidecar, None))
            .collect()
    }
}

/// `"admin/users/form.html"` -> (`"admin/users"`, `"form"`).
fn split_name(name: &str) -> (&str, &str) {
    let (dir, file) = name.rsplit_once('/').unwrap_or(("", name));
    (dir, file.split_once('.').map_or(file, |(s, _)| s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_lookup() {
        let files = [
            "app/views/accounts/show.html.haml",
            "app/views/accounts/_card.html.haml",
            "app/views/accounts/_card.json.jbuilder",
            "app/views/application/_flash.html.haml",
            "app/views/shared/_nav.html.erb",
            "ee/app/views/shared/_nav.html.haml",
            "app/views/layouts/admin.html.haml",
        ];
        let idx = ViewIndex::new(files.into_iter());
        assert_eq!(
            idx.partial("accounts", "card"),
            vec![
                "app/views/accounts/_card.html.haml",
                "app/views/accounts/_card.json.jbuilder"
            ]
        );
        assert_eq!(
            idx.partial("accounts", "flash"),
            vec!["app/views/application/_flash.html.haml"]
        );
        assert_eq!(
            idx.partial("accounts", "shared/nav"),
            vec![
                "app/views/shared/_nav.html.erb",
                "ee/app/views/shared/_nav.html.haml"
            ]
        );
        assert_eq!(idx.partial("accounts", "missing"), Vec::<&str>::new());
        assert_eq!(
            idx.layout("admin"),
            vec!["app/views/layouts/admin.html.haml"]
        );
        assert_eq!(
            idx.template("x", "accounts/show"),
            vec!["app/views/accounts/show.html.haml"]
        );
    }
}
