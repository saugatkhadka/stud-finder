//! Report renderers. All are pure functions of the [`crate::Report`]
//! (GitHub output additionally writes to the files Actions provides).

pub mod github;
pub mod json;
pub mod markdown;
pub mod text;

use crate::report::{Hop, ImpactPath};
use ripple_core::{Confidence, EdgeKind};

pub(crate) fn kind_str(k: EdgeKind) -> &'static str {
    match k {
        EdgeKind::RubyConstant => "ruby_constant",
        EdgeKind::RubyRequire => "ruby_require",
        EdgeKind::RailsInferred => "rails_inferred",
        EdgeKind::RailsView => "rails_view",
        EdgeKind::RailsRoute => "rails_route",
        EdgeKind::JsImport => "js_import",
        EdgeKind::JsDynamicImport => "js_dynamic_import",
        EdgeKind::ApiCall => "api_call",
        EdgeKind::ViewMount => "view_mount",
        EdgeKind::GraphQl => "graph_ql",
        EdgeKind::Asset => "asset",
    }
}

pub(crate) fn conf_str(c: Confidence) -> &'static str {
    match c {
        Confidence::Exact => "exact",
        Confidence::Inferred => "inferred",
        Confidence::Heuristic => "heuristic",
    }
}

/// "kind `detail#to_scope` line N, confidence" for one hop.
pub(crate) fn hop_reason(h: &Hop) -> String {
    let mut parts = Vec::new();
    if let Some(k) = h.kind {
        parts.push(kind_str(k).to_string());
    }
    if h.inherit {
        parts.push("inherits".to_string());
    }
    if let Some(d) = detail_with_scope(h.detail.as_deref(), h.to_scope.as_deref()) {
        parts.push(format!("`{}`", d.replace('`', "'")));
    }
    if let Some(l) = h.line {
        parts.push(format!("line {l}"));
    }
    if let Some(c) = h.confidence
        && c != Confidence::Exact {
            parts.push(conf_str(c).to_string());
        }
    parts.join(" ")
}

/// `a.rb` --(ruby_constant `A` line 3)--> `b.rb` ... rendered with `fmt_file`.
pub(crate) fn path_line(p: &ImpactPath, fmt_file: impl Fn(&str) -> String, arrow: &str) -> String {
    let mut out = String::new();
    for (i, h) in p.path.iter().enumerate() {
        match &h.from_scope {
            Some(scope) if i + 1 < p.path.len() => out.push_str(&fmt_file(&format!("{}#{scope}", h.file))),
            _ => out.push_str(&fmt_file(&h.file)),
        }
        if i + 1 < p.path.len() {
            let r = hop_reason(h);
            if r.is_empty() {
                out.push_str(&format!(" {arrow} "));
            } else {
                out.push_str(&format!(" {arrow} ({r}) "));
            }
        }
    }
    out
}

/// An edge's detail together with the method it uses on the dependency:
/// `Purchase` + `refund!` -> `Purchase#refund!`. Left alone when the detail
/// already names the method (`#refund! (by name)`); a detail that is not a
/// bare class name (a constant, a route) gets ` -> #refund!` instead, so it
/// does not read as a method of that constant.
pub(crate) fn detail_with_scope(detail: Option<&str>, to_scope: Option<&str>) -> Option<String> {
    match (detail, to_scope) {
        (Some(d), Some(t)) => {
            let names_it = d.split(|c: char| !(c.is_alphanumeric() || "_!?=".contains(c))).any(|w| w == t);
            let last = d.rsplit("::").next().unwrap_or(d);
            let class_like = last.chars().next().is_some_and(char::is_uppercase)
                && last.chars().any(char::is_lowercase)
                && last.chars().all(|c| c.is_alphanumeric() || c == '_');
            Some(if names_it {
                d.to_string()
            } else if class_like {
                format!("{d}#{t}")
            } else {
                format!("{d} -> #{t}")
            })
        }
        (Some(d), None) => Some(d.to_string()),
        (None, Some(t)) => Some(format!("#{t}")),
        (None, None) => None,
    }
}

/// 1362 -> "1,362".
pub(crate) fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// One line on how the analysis treated changed files.
pub(crate) fn mode_line(r: &crate::report::Report) -> String {
    if r.query.file_level {
        "file-level analysis (every changed file treated as wholly changed; method and action scopes ignored)".to_string()
    } else {
        let narrowed = r.changed.iter().filter(|c| c.scopes.is_some()).count();
        format!(
            "method-level analysis ({} of {} changed file{} narrowed to the methods the diff touches; dependencies followed per method / controller action)",
            narrowed,
            r.changed.len(),
            crate::eval::plural(r.changed.len())
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn detail_and_scope() {
        let f = |d: Option<&str>, t: Option<&str>| super::detail_with_scope(d, t);
        assert_eq!(f(Some("Purchase"), Some("refund!")).as_deref(), Some("Purchase#refund!"));
        assert_eq!(f(Some("Billing::Invoice"), Some("pay")).as_deref(), Some("Billing::Invoice#pay"));
        assert_eq!(f(Some("#refund! (by name)"), Some("refund!")).as_deref(), Some("#refund! (by name)"));
        assert_eq!(f(Some("Purchase::NON_GIFT_STATES"), Some("ship")).as_deref(), Some("Purchase::NON_GIFT_STATES -> #ship"));
        assert_eq!(f(Some("GET /p/:id → p#show"), Some("show")).as_deref(), Some("GET /p/:id → p#show"));
        assert_eq!(f(Some("POST /x"), Some("create")).as_deref(), Some("POST /x -> #create"));
        assert_eq!(f(None, Some("show")).as_deref(), Some("#show"));
        assert_eq!(f(Some("A"), None).as_deref(), Some("A"));
        assert_eq!(f(None, None), None);
    }

    #[test]
    fn helpers() {
        assert_eq!(super::thousands(0), "0");
        assert_eq!(super::thousands(999), "999");
        assert_eq!(super::thousands(1362), "1,362");
        assert_eq!(super::thousands(1234567), "1,234,567");
    }
}
