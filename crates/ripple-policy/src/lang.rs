use serde::{Deserialize, Serialize};

/// Coarse language of a file, used for `crosses_boundary` and reporting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Ruby,
    Js,
    /// Server-rendered templates (ERB, HAML, Slim, Jbuilder, ...).
    Template,
    #[default]
    Other,
}

/// Which side of the Rails/React boundary a language lives on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Server,
    Client,
}

const RUBY_NAMES: &[&str] = &["Gemfile", "Rakefile", "Guardfile", "Capfile", "Dangerfile", "config.ru"];
const RUBY_EXT: &[&str] = &["rb", "rake", "ru", "gemspec", "thor"];
const TEMPLATE_EXT: &[&str] = &["erb", "haml", "slim", "jbuilder", "builder", "rabl"];
const JS_EXT: &[&str] = &["js", "jsx", "ts", "tsx", "mjs", "cjs", "mts", "cts", "vue", "svelte"];

impl Language {
    pub fn of(path: &str) -> Self {
        let name = path.rsplit('/').next().unwrap_or(path);
        if RUBY_NAMES.contains(&name) {
            return Language::Ruby;
        }
        let Some((_, ext)) = name.rsplit_once('.') else { return Language::Other };
        let ext = ext.to_ascii_lowercase();
        let ext = ext.as_str();
        if RUBY_EXT.contains(&ext) {
            Language::Ruby
        } else if TEMPLATE_EXT.contains(&ext) {
            Language::Template
        } else if JS_EXT.contains(&ext) {
            Language::Js
        } else {
            Language::Other
        }
    }

    /// Ruby and templates are the server side; JS/TS the client side.
    pub fn side(self) -> Option<Side> {
        match self {
            Language::Ruby | Language::Template => Some(Side::Server),
            Language::Js => Some(Side::Client),
            Language::Other => None,
        }
    }

    pub fn is_source(self) -> bool {
        self != Language::Other
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Language::Ruby => "ruby",
            Language::Js => "js",
            Language::Template => "template",
            Language::Other => "other",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies() {
        assert_eq!(Language::of("app/models/user.rb"), Language::Ruby);
        assert_eq!(Language::of("Gemfile"), Language::Ruby);
        assert_eq!(Language::of("lib/tasks/a.rake"), Language::Ruby);
        assert_eq!(Language::of("app/views/a/show.html.erb"), Language::Template);
        assert_eq!(Language::of("app/views/a/show.json.jbuilder"), Language::Template);
        assert_eq!(Language::of("app/javascript/a.TSX"), Language::Js);
        assert_eq!(Language::of("app/javascript/a.vue"), Language::Js);
        assert_eq!(Language::of("config/locales/en.yml"), Language::Other);
        assert_eq!(Language::of("Makefile"), Language::Other);
        assert_eq!(Language::Template.side(), Some(Side::Server));
    }
}
