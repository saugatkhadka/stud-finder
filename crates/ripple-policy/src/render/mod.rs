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

/// "kind `detail` line N, confidence" for one hop.
pub(crate) fn hop_reason(h: &Hop) -> String {
    let mut parts = Vec::new();
    if let Some(k) = h.kind {
        parts.push(kind_str(k).to_string());
    }
    if let Some(d) = &h.detail {
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
        out.push_str(&fmt_file(&h.file));
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
