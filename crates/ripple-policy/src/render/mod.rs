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

/// Fewest plain non-source files in one directory that the text and
/// markdown renderers show as one summary row.
pub(crate) const MIN_COLLAPSED: usize = 3;

/// A row of the "changed" section: one file, or several plain non-source
/// files of one directory (locale JSON, fixtures, assets).
pub(crate) enum ChangedRow<'a> {
    One(&'a crate::report::ChangedEntry),
    Dir { dir: String, files: Vec<&'a crate::report::ChangedEntry> },
}

/// Groups the changed files for display. A file is collapsible when its
/// language is `other` and nothing about it needs its own line (not global,
/// not a test, not renamed). Directories with at least [`MIN_COLLAPSED`] such
/// files become one row, at the position of their first file; everything
/// else keeps its order. The JSON report is not affected.
pub(crate) fn changed_rows(changed: &[crate::report::ChangedEntry]) -> Vec<ChangedRow<'_>> {
    use std::collections::HashMap;
    let dir_of = |p: &str| p.rfind('/').map(|i| p[..=i].to_string()).unwrap_or_else(|| "./".to_string());
    let collapsible = |c: &crate::report::ChangedEntry| c.language == crate::Language::Other && !c.is_global && !c.is_test && c.old_path.is_none();
    let mut count: HashMap<String, usize> = HashMap::new();
    for c in changed.iter().filter(|c| collapsible(c)) {
        *count.entry(dir_of(&c.path)).or_default() += 1;
    }
    let mut rows: Vec<ChangedRow> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for c in changed {
        let dir = dir_of(&c.path);
        if collapsible(c) && count.get(&dir).copied().unwrap_or(0) >= MIN_COLLAPSED {
            if let Some(&i) = at.get(&dir) {
                if let ChangedRow::Dir { files, .. } = &mut rows[i] {
                    files.push(c);
                }
            } else {
                at.insert(dir.clone(), rows.len());
                rows.push(ChangedRow::Dir { dir, files: vec![c] });
            }
        } else {
            rows.push(ChangedRow::One(c));
        }
    }
    rows
}

/// Totals of a collapsed row: statuses other than modified ("3 added"),
/// summed line counts (`None` when no file has any), and the union of areas.
pub(crate) fn dir_totals(files: &[&crate::report::ChangedEntry]) -> (String, Option<(u32, u32)>, Vec<String>) {
    use crate::changed::FileStatus;
    let n = |s: FileStatus| files.iter().filter(|c| c.status == s).count();
    let statuses: Vec<String> = [(FileStatus::Added, "added"), (FileStatus::Deleted, "deleted")]
        .into_iter()
        .filter(|(s, _)| n(*s) > 0)
        .map(|(s, w)| format!("{} {w}", n(s)))
        .collect();
    let lines = files.iter().any(|c| c.added.is_some() || c.deleted.is_some()).then(|| {
        files.iter().fold((0u32, 0u32), |(a, d), c| (a.saturating_add(c.added.unwrap_or(0)), d.saturating_add(c.deleted.unwrap_or(0))))
    });
    let mut areas: Vec<String> = files.iter().flat_map(|c| c.areas.iter().cloned()).collect();
    areas.sort();
    areas.dedup();
    (statuses.join(", "), lines, areas)
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
