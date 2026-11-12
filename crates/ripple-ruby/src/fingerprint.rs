//! The *outward* fingerprint of one analysed file: a hash of everything in
//! it that can change another file's edges or the Rubydex resolution of any
//! name. Two versions of a file with the same fingerprint (a "body-only"
//! edit) differ only in facts that feed the file's own edges, scopes,
//! mentions, metrics and warnings, so `update` can keep the Rubydex graph as
//! it is (stale offsets and ids for that file are never read) and recompute
//! only that file's edges.
//!
//! Hashed, all position-free:
//! - the source kind, whether the file was readable and whether Rubydex
//!   produced a local graph;
//! - `helper_defs` (other templates link to view helpers by these names);
//! - every Rubydex definition in document order, except the purely local
//!   ones below: its variant, name / string ids, flags, visibility, lexical
//!   nesting (as an index into this list), receiver, superclass and mixin
//!   reference names, alias target, constant-visibility target;
//! - every constant reference's name id, in document order;
//! - for both lists, the order Rubydex's resolver processes them in (it
//!   sorts by byte offset within a document), as a permutation.
//!
//! Purely local definitions (left out): instance methods, attribute
//! accessors and method aliases without a receiver whose lexical owner is a
//! `class`/`module` body, and instance variables inside such a method. They
//! only create method / instance-variable declarations owned by that class
//! (names ending in `()` or starting with `@`, so they never collide with a
//! constant), which nothing in ripple reads. Anything that can create a
//! singleton class (`def self.x`, `class << self`, class-level ivars) or
//! touch constants is kept. A definition that is the lexical nesting of a
//! kept one is kept too.
//!
//! Everything else in a file entry (constant reference *sites*, Rails facts,
//! template facts, method facts, warnings) only feeds that file's own
//! output, which is recomputed for every changed file anyway.
//!
//! [`of`] also returns a second hash, of the definitions part (everything
//! above but the constant references) and the *set* of distinct constant
//! reference names. Two versions with equal second hashes differ at most in
//! how many plain references of each name there are and where (superclasses
//! and mixins are part of the definitions); see the crate docs for why that
//! is enough to skip Rubydex too.

use crate::analyse::FileEntry;
use rubydex::indexing::local_graph::LocalGraph;
use rubydex::model::definitions::{Definition, Mixin, Receiver};
use rubydex::model::ids::{ConstantReferenceId, DefinitionId};
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::mem::discriminant;

/// (full fingerprint, definitions + reference name set).
pub fn of(lg: Option<&LocalGraph>, entry: &FileEntry) -> (u64, u64) {
    let mut h = DefaultHasher::new();
    entry.kind.hash(&mut h);
    entry.unreadable.hash(&mut h);
    entry.helper_defs.hash(&mut h);
    lg.is_some().hash(&mut h);
    let Some(lg) = lg else {
        let f = h.finish();
        return (f, f);
    };
    definitions(lg, &mut h);
    let mut names_h = h.clone();
    let mut names: Vec<u64> = lg.constant_references().values().map(|r| r.name_id().get()).collect();
    names.sort_unstable();
    names.dedup();
    names.hash(&mut names_h);
    references(lg, &mut h);
    (h.finish(), names_h.finish())
}

/// Is `def` local to its class body (see the module docs)? `nesting` is
/// the definition it is lexically nested in.
fn local(def: &Definition, defs: &HashMap<DefinitionId, &Definition>) -> bool {
    let in_class_body = |nesting: &Option<DefinitionId>| {
        nesting
            .and_then(|n| defs.get(&n))
            .is_some_and(|d| matches!(d, Definition::Class(_) | Definition::Module(_)))
    };
    match def {
        Definition::Method(m) => m.receiver().is_none() && in_class_body(m.lexical_nesting_id()),
        Definition::AttrAccessor(_) | Definition::AttrReader(_) | Definition::AttrWriter(_) => {
            in_class_body(def.lexical_nesting_id())
        }
        Definition::MethodAlias(a) => a.receiver().is_none() && in_class_body(a.lexical_nesting_id()),
        Definition::InstanceVariable(v) => v
            .lexical_nesting_id()
            .and_then(|n| defs.get(&n))
            .is_some_and(|m| matches!(m, Definition::Method(_)) && local(m, defs)),
        _ => false,
    }
}

fn definitions<'g>(lg: &'g LocalGraph, h: &mut DefaultHasher) {
    let all = lg.definitions();
    let order = lg.document().definitions();
    let by_id: HashMap<DefinitionId, &'g Definition> = order.iter().filter_map(|id| all.get(id).map(|d| (*id, d))).collect();
    let mut keep: Vec<bool> = order.iter().map(|id| by_id.get(id).is_some_and(|d| !local(d, &by_id))).collect();
    // A kept definition's nesting is kept (methods can nest definitions).
    let pos: HashMap<DefinitionId, usize> = order.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    for i in (0..order.len()).rev() {
        if keep[i]
            && let Some(n) = by_id.get(&order[i]).and_then(|d| *d.lexical_nesting_id())
            && let Some(&j) = pos.get(&n)
        {
            keep[j] = true;
        }
    }
    let kept: Vec<usize> = (0..order.len()).filter(|&i| keep[i] && by_id.contains_key(&order[i])).collect();
    let index: HashMap<DefinitionId, usize> = kept.iter().enumerate().map(|(k, &i)| (order[i], k)).collect();
    let refs = lg.constant_references();
    let ref_name = |id: &ConstantReferenceId| refs.get(id).map(|r| r.name_id().get());
    let nest = |n: &Option<DefinitionId>| n.map(|n| index.get(&n).copied());
    kept.len().hash(h);
    for &i in &kept {
        let d = by_id[&order[i]];
        discriminant(d).hash(h);
        d.name_id().map(|n| n.get()).hash(h);
        nest(d.lexical_nesting_id()).hash(h);
        match d {
            Definition::Class(c) => {
                c.flags().bits().hash(h);
                c.superclass_ref().map(ref_name).hash(h);
                mixins(c.mixins(), &ref_name, h);
            }
            Definition::Module(m) => {
                m.flags().bits().hash(h);
                mixins(m.mixins(), &ref_name, h);
            }
            Definition::SingletonClass(s) => {
                s.flags().bits().hash(h);
                mixins(s.mixins(), &ref_name, h);
            }
            Definition::Constant(c) => c.flags().bits().hash(h),
            Definition::ConstantAlias(a) => a.target_name_id().get().hash(h),
            Definition::ConstantVisibility(v) => {
                v.receiver().map(|n| n.get()).hash(h);
                v.target().get().hash(h);
                discriminant(v.visibility()).hash(h);
            }
            Definition::MethodVisibility(v) => {
                v.str_id().get().hash(h);
                discriminant(v.visibility()).hash(h);
            }
            Definition::Method(m) => {
                m.str_id().get().hash(h);
                discriminant(m.visibility()).hash(h);
                receiver(m.receiver(), &index, h);
            }
            Definition::AttrAccessor(a) => (a.str_id().get(), discriminant(a.visibility())).hash(h),
            Definition::AttrReader(a) => (a.str_id().get(), discriminant(a.visibility())).hash(h),
            Definition::AttrWriter(a) => (a.str_id().get(), discriminant(a.visibility())).hash(h),
            Definition::GlobalVariable(v) => v.str_id().get().hash(h),
            Definition::InstanceVariable(v) => v.str_id().get().hash(h),
            Definition::ClassVariable(v) => v.str_id().get().hash(h),
            Definition::MethodAlias(a) => {
                (a.new_name_str_id().get(), a.old_name_str_id().get()).hash(h);
                receiver(a.receiver(), &index, h);
            }
            Definition::GlobalVariableAlias(a) => (a.new_name_str_id().get(), a.old_name_str_id().get()).hash(h),
        }
    }
    // Resolver order within the document: by offset.
    let mut by_offset: Vec<usize> = (0..kept.len()).collect();
    by_offset.sort_by_key(|&k| by_id[&order[kept[k]]].offset().clone());
    by_offset.hash(h);
}

fn references(lg: &LocalGraph, h: &mut DefaultHasher) {
    let refs = lg.constant_references();
    let ref_order = lg.document().constant_references();
    ref_order.len().hash(h);
    let mut offsets = Vec::with_capacity(ref_order.len());
    for id in ref_order {
        match refs.get(id) {
            Some(r) => {
                r.name_id().get().hash(h);
                offsets.push(r.offset().clone());
            }
            None => u64::MAX.hash(h),
        }
    }
    let mut perm: Vec<usize> = (0..offsets.len()).collect();
    perm.sort_by_key(|&k| offsets[k].clone());
    perm.hash(h);
}

fn mixins(ms: &[Mixin], ref_name: &impl Fn(&ConstantReferenceId) -> Option<u64>, h: &mut DefaultHasher) {
    ms.len().hash(h);
    for m in ms {
        discriminant(m).hash(h);
        ref_name(m.constant_reference_id()).hash(h);
    }
}

fn receiver(r: &Option<Receiver>, index: &HashMap<DefinitionId, usize>, h: &mut DefaultHasher) {
    match r {
        None => 0u8.hash(h),
        Some(Receiver::SelfReceiver(d)) => (1u8, index.get(d).copied()).hash(h),
        Some(Receiver::ConstantReceiver(n)) => (2u8, n.get()).hash(h),
    }
}
