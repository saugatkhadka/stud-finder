//! Drops the parts of a Rubydex [`LocalGraph`] ripple never reads before it
//! is merged into the long-lived graph.
//!
//! * **Method references** (every call site in the file). Rubydex 0.4.3's
//!   resolver and invalidation never look at them; they are only stored and,
//!   on deletion, un-ref-count their name strings. On GitLab they were 2.7M
//!   entries, ~185 MB of the retained graph (map + per-document id lists).
//! * **The document's line index**, used only to turn offsets into LSP
//!   line/columns. The document is recreated from a short stand-in text, so
//!   its line index is one entry; the stand-in is derived from the original
//!   content hash, so "same content" (Rubydex's skip check in
//!   `consume_document_changes`) still compares as before.
//! * **Diagnostics**, which ripple reads from the local graph before this.
//!
//! Definitions and constant references are re-added in their original order,
//! so the definition and reference maps are built by the same insertion
//! sequence and resolution sees the same work in the same order.

use rubydex::indexing::local_graph::LocalGraph;
use rubydex::model::document::Document;
use std::collections::hash_map::Entry;

pub fn without_unused_parts(lg: LocalGraph) -> LocalGraph {
    let (uri_id, document, mut definitions, mut strings, names, mut constant_references, method_references, _deps) =
        lg.into_parts();
    for r in method_references.values() {
        if let Entry::Occupied(mut e) = strings.entry(*r.str())
            && !e.get_mut().decrement_ref_count()
        {
            e.remove();
        }
    }
    drop(method_references);
    let stand_in = format!("{:016x}", document.content_hash());
    let mut out = LocalGraph::from_parts(uri_id, Document::new(document.uri().into(), &stand_in), strings, names);
    for id in document.definitions() {
        if let Some(d) = definitions.remove(id) {
            out.add_definition(d);
        }
    }
    for id in document.constant_references() {
        if let Some(r) = constant_references.remove(id) {
            out.add_constant_reference(r);
        }
    }
    debug_assert!(definitions.is_empty() && constant_references.is_empty());
    out
}
