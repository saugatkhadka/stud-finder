//! Shadowing repair for incremental updates.
//!
//! Rubydex 0.4.3 invalidates by declaration: when a file changes, names that
//! pointed at that file's declarations are re-resolved. It does not notice
//! that a *new* declaration can shadow a name that already resolved
//! elsewhere. Restoring `app/lib/translation_service/translation.rb` in
//! mastodon creates `TranslationService::Translation`, but `Translation`
//! inside `class TranslationService::DeepL < TranslationService` stays
//! resolved to the top-level `::Translation` model, while a fresh build
//! resolves it to the new class (through the ancestor `TranslationService`).
//!
//! After an update we therefore find declarations that did not exist before,
//! and every resolved name with the same simple name for which the new
//! declaration's owner comes *earlier* in Ruby's lookup order (lexical
//! scopes, then ancestors of the innermost scope, then `Object`) than the
//! owner of its current target. The documents referencing those names are
//! deleted and re-added, so Rubydex resolves them from scratch.
//!
//! Re-consuming an unchanged document is a no-op in Rubydex (it compares
//! content hashes), and a name shared with a document that is not re-added
//! survives deletion, hence delete-all-then-re-add and the caller's check
//! that every shadowed name is actually gone.

use rubydex::model::declaration::{Ancestor, Declaration, Namespace};
use rubydex::model::graph::Graph;
use rubydex::model::ids::{DeclarationId, NameId, UriId};
use rubydex::model::name::{NameRef, ParentScope};
use std::collections::{BTreeSet, HashMap, HashSet};

/// Constant-like declarations (namespaces, constants, aliases) defined by `uris`.
pub fn constant_decls<'a>(
    graph: &Graph,
    uris: impl Iterator<Item = &'a str>,
) -> HashSet<DeclarationId> {
    let mut out = HashSet::new();
    for uri in uris {
        let Some(doc) = graph.documents().get(&UriId::from(uri)) else {
            continue;
        };
        for def in doc.definitions() {
            if let Some(decl_id) = graph.definition_id_to_declaration_id(*def)
                && matches!(
                    graph.declarations().get(decl_id),
                    Some(
                        Declaration::Namespace(_)
                            | Declaration::Constant(_)
                            | Declaration::ConstantAlias(_)
                    )
                )
            {
                out.insert(*decl_id);
            }
        }
    }
    out
}

/// Resolved names that `new_decls` may shadow.
pub fn shadowed(graph: &Graph, new_decls: &HashSet<DeclarationId>) -> HashSet<NameId> {
    // simple name -> owners of new declarations with that name
    let mut by_name: HashMap<String, Vec<(DeclarationId, DeclarationId)>> = HashMap::new();
    for id in new_decls {
        if let Some(decl) = graph.declarations().get(id) {
            by_name
                .entry(decl.unqualified_name())
                .or_default()
                .push((*id, *decl.owner_id()));
        }
    }
    let mut names = HashSet::new();
    if by_name.is_empty() {
        return names;
    }
    for (name_id, name) in graph.names() {
        let NameRef::Resolved(resolved) = name else {
            continue;
        };
        let Some(s) = graph.strings().get(name.str()) else {
            continue;
        };
        let Some(candidates) = by_name.get(s.as_str()) else {
            continue;
        };
        let target = *resolved.declaration_id();
        if candidates.iter().any(|(d, _)| *d == target) {
            continue;
        }
        let Some(current_owner) = graph.declarations().get(&target).map(|d| *d.owner_id()) else {
            continue;
        };
        let order = lookup_order(graph, name);
        let pos = |id: &DeclarationId| order.iter().position(|o| o == id);
        let shadows = candidates
            .iter()
            .any(|(_, owner)| match (pos(owner), pos(&current_owner)) {
                (Some(new), Some(cur)) => new < cur,
                (Some(_), None) => true,
                _ => false,
            });
        if shadows {
            names.insert(*name_id);
        }
    }
    names
}

/// Names of constant definitions whose declaration is not owned by the
/// definition's lexical owner (the resolved parent scope for `class A::B`,
/// the enclosing namespace for `class B`, `Object` at top level).
///
/// Two cases seen on real repos, both absent from a fresh build:
/// - mastodon: `TranslationService::Translation` was referenced (and
///   resolved, through `Object`, to `::Translation`) before the file defining
///   it was re-added; names are shared between references and definitions,
///   so the new `class TranslationService::Translation` joined `::Translation`.
/// - GitLab: after an unrelated file reopening `Gitlab::Ci` changed,
///   `class Rules::Rule` inside `module Entry` (file unchanged) was
///   re-resolved under a top-level placeholder `Rules`, creating
///   `Rules::Rule` instead of `Gitlab::Ci::Config::Entry::Rules::Rule`.
///
/// Scans every definition (a few ms on GitLab); callers diff against the set
/// from the previous run so that quirks a fresh build has too are left alone.
pub fn misattached(graph: &Graph) -> HashSet<NameId> {
    let object = DeclarationId::from("Object");
    let decls = graph.declarations();
    let mut out = HashSet::new();
    for def in graph.definitions().values() {
        let Some(name_id) = def.name_id() else {
            continue;
        };
        let Some(decl_id) = graph.name_id_to_declaration_id(*name_id) else {
            continue;
        };
        let Some(decl) = decls.get(decl_id) else {
            continue;
        };
        if !matches!(
            decl,
            Declaration::Namespace(Namespace::Class(_) | Namespace::Module(_))
                | Declaration::Constant(_)
                | Declaration::ConstantAlias(_)
        ) {
            continue;
        }
        let Some(name) = graph.names().get(name_id) else {
            continue;
        };
        let lexical_owner = match name.parent_scope() {
            ParentScope::Some(p) => graph.name_id_to_declaration_id(*p).copied(),
            ParentScope::TopLevel => Some(object),
            ParentScope::None => match name.nesting() {
                Some(n) => graph.name_id_to_declaration_id(*n).copied(),
                None => Some(object),
            },
            ParentScope::Attached(_) => None,
        };
        // Only compare against plain namespaces; aliases and placeholders are resolved differently.
        let Some(expected) = lexical_owner.filter(|o| {
            matches!(
                decls.get(o),
                Some(Declaration::Namespace(
                    Namespace::Class(_) | Namespace::Module(_)
                ))
            )
        }) else {
            continue;
        };
        if *decl.owner_id() != expected {
            out.insert(*name_id);
        }
    }
    out
}

/// Documents whose references or definitions use any of `names`.
pub fn documents_using(graph: &Graph, names: &HashSet<NameId>) -> BTreeSet<String> {
    let mut uris = HashSet::new();
    if names.is_empty() {
        return BTreeSet::new();
    }
    for r in graph.constant_references().values() {
        if names.contains(r.name_id()) {
            uris.insert(r.uri_id());
        }
    }
    for def in graph.definitions().values() {
        if def.name_id().is_some_and(|n| names.contains(n)) {
            uris.insert(*def.uri_id());
        }
    }
    uris.iter()
        .filter_map(|u| graph.documents().get(u))
        .map(|d| d.uri().to_string())
        .collect()
}

/// Declarations searched for `name`, in Ruby's order.
fn lookup_order(graph: &Graph, name: &NameRef) -> Vec<DeclarationId> {
    let resolved = |id: &NameId| graph.name_id_to_declaration_id(*id).copied();
    let ancestors = |decl: DeclarationId, out: &mut Vec<DeclarationId>| {
        if let Some(ns) = graph
            .declarations()
            .get(&decl)
            .and_then(|d| d.as_namespace())
        {
            out.extend(ns.ancestors().iter().filter_map(|a| match a {
                Ancestor::Complete(id) => Some(*id),
                Ancestor::Partial(_) => None,
            }));
        }
    };
    let mut out = Vec::new();
    match name.parent_scope() {
        ParentScope::Some(parent) => {
            if let Some(p) = resolved(parent) {
                out.push(p);
                ancestors(p, &mut out);
            }
        }
        ParentScope::TopLevel => out.push(DeclarationId::from("Object")),
        ParentScope::Attached(_) => {}
        ParentScope::None => {
            let mut innermost = None;
            let mut nesting = *name.nesting();
            let mut guard = 0;
            while let Some(n) = nesting {
                guard += 1;
                let Some(decl) = resolved(&n) else { break };
                innermost.get_or_insert(decl);
                out.push(decl);
                nesting = graph.names().get(&n).and_then(|nn| *nn.nesting());
                if guard > 64 {
                    break;
                }
            }
            if let Some(inner) = innermost {
                ancestors(inner, &mut out);
            }
            out.push(DeclarationId::from("Object"));
        }
    }
    out
}
