//! A port of ActiveSupport's default English inflections, which Rails uses to
//! derive controller names (`resource :profile` -> `profiles`), nested param
//! names (`resources :media` -> `:medium_id`) and route names.
//!
//! App-specific inflections (`config/initializers/inflections.rb`) are not read.

use regex::Regex;
use std::sync::OnceLock;

struct Rules {
    plurals: Vec<(Regex, &'static str)>,
    singulars: Vec<(Regex, &'static str)>,
}

const UNCOUNTABLE: &[&str] = &["equipment", "information", "rice", "money", "species", "series", "fish", "sheep", "jeans", "police"];
const IRREGULAR: &[(&str, &str)] = &[("person", "people"), ("man", "men"), ("child", "children"), ("sex", "sexes"), ("move", "moves"), ("zombie", "zombies")];

fn rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| {
        // Listed in ActiveSupport definition order; later rules take precedence,
        // so they are reversed below.
        let plural: &[(&str, &'static str)] = &[
            ("$", "s"),
            ("(?i)s$", "s"),
            ("(?i)^(ax|test)is$", "${1}es"),
            ("(?i)(octop|vir)us$", "${1}i"),
            ("(?i)(octop|vir)i$", "${1}i"),
            ("(?i)(alias|status)$", "${1}es"),
            ("(?i)(bu)s$", "${1}ses"),
            ("(?i)(buffal|tomat)o$", "${1}oes"),
            ("(?i)([ti])um$", "${1}a"),
            ("(?i)([ti])a$", "${1}a"),
            ("(?i)sis$", "ses"),
            ("(?i)(?:([^f])fe|([lr])f)$", "${1}${2}ves"),
            ("(?i)(hive)$", "${1}s"),
            ("(?i)([^aeiouy]|qu)y$", "${1}ies"),
            ("(?i)(x|ch|ss|sh)$", "${1}es"),
            ("(?i)(matr|vert|ind)(?:ix|ex)$", "${1}ices"),
            ("(?i)^(m|l)ouse$", "${1}ice"),
            ("(?i)^(m|l)ice$", "${1}ice"),
            ("(?i)^(ox)$", "${1}en"),
            ("(?i)^(oxen)$", "${1}"),
            ("(?i)(quiz)$", "${1}zes"),
        ];
        let singular: &[(&str, &'static str)] = &[
            ("(?i)s$", ""),
            ("(?i)(ss)$", "${1}"),
            ("(?i)(n)ews$", "${1}ews"),
            ("(?i)([ti])a$", "${1}um"),
            ("(?i)((a)naly|(b)a|(d)iagno|(p)arenthe|(p)rogno|(s)ynop|(t)he)(sis|ses)$", "${1}sis"),
            ("(?i)(^analy)(sis|ses)$", "${1}sis"),
            ("(?i)([^f])ves$", "${1}fe"),
            ("(?i)(hive)s$", "${1}"),
            ("(?i)(tive)s$", "${1}"),
            ("(?i)([lr])ves$", "${1}f"),
            ("(?i)([^aeiouy]|qu)ies$", "${1}y"),
            ("(?i)(s)eries$", "${1}eries"),
            ("(?i)(m)ovies$", "${1}ovie"),
            ("(?i)(x|ch|ss|sh)es$", "${1}"),
            ("(?i)^(m|l)ice$", "${1}ouse"),
            ("(?i)(bus)(es)?$", "${1}"),
            ("(?i)(o)es$", "${1}"),
            ("(?i)(shoe)s$", "${1}"),
            ("(?i)(cris|test)(is|es)$", "${1}is"),
            ("(?i)^(a)x[ie]s$", "${1}xis"),
            ("(?i)(octop|vir)(us|i)$", "${1}us"),
            ("(?i)(alias|status)(es)?$", "${1}"),
            ("(?i)^(ox)en", "${1}"),
            ("(?i)(vert|ind)ices$", "${1}ex"),
            ("(?i)(matr)ices$", "${1}ix"),
            ("(?i)(quiz)zes$", "${1}"),
            ("(?i)(database)s$", "${1}"),
        ];
        let compile = |list: &[(&str, &'static str)]| -> Vec<(Regex, &'static str)> {
            list.iter().rev().map(|(re, rep)| (Regex::new(re).expect("static inflection regex"), *rep)).collect()
        };
        Rules { plurals: compile(plural), singulars: compile(singular) }
    })
}

/// Applies the inflection to the last `_`-separated word, as ActiveSupport does
/// for compound words (`custom_emoji` -> `custom_emojis`).
fn inflect(word: &str, to_plural: bool) -> String {
    if word.is_empty() {
        return String::new();
    }
    let (head, last) = match word.rfind('_') {
        Some(i) => (&word[..=i], &word[i + 1..]),
        None => ("", word),
    };
    let lower = last.to_ascii_lowercase();
    if UNCOUNTABLE.contains(&lower.as_str()) {
        return word.to_string();
    }
    for (one, many) in IRREGULAR {
        let (from, to) = if to_plural { (one, many) } else { (many, one) };
        if lower == *from || lower == *to {
            return format!("{head}{to}");
        }
    }
    let list = if to_plural { &rules().plurals } else { &rules().singulars };
    for (re, rep) in list {
        if re.is_match(last) {
            return format!("{head}{}", re.replace(last, *rep));
        }
    }
    word.to_string()
}

pub fn pluralize(word: &str) -> String {
    inflect(word, true)
}

pub fn singularize(word: &str) -> String {
    inflect(word, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rails_examples() {
        for (s, p) in [
            ("status", "statuses"),
            ("medium", "media"),
            ("history", "histories"),
            ("inbox", "inboxes"),
            ("profile", "profiles"),
            ("person", "people"),
            ("custom_emoji", "custom_emojis"),
            ("custom_css", "custom_csses"),
            ("analysis", "analyses"),
            ("wife", "wives"),
            ("search", "searches"),
            ("quiz", "quizzes"),
            ("series", "series"),
        ] {
            assert_eq!(pluralize(s), p, "pluralize {s}");
            assert_eq!(singularize(p), s, "singularize {p}");
        }
        assert_eq!(singularize("reblogged_by"), "reblogged_by");
        assert_eq!(singularize("news"), "news");
    }
}
