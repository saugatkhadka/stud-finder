//! Method-level facts (`ruby.method_level`): which methods of a file get a
//! scope range (`Facts.scopes`, so a diff maps onto changed methods), and the
//! workspace's [`MentionTable`] for the query-time mention filter (see
//! [`ripple_core::Graph::impact_scoped`]). Edges stay file-level, apart from
//! controller-action scopes.
//!
//! A method gets no scope (a change inside it is a change to the whole file)
//! when it is invoked implicitly: a fixed list of hooks and conversions,
//! setters, operators ([`methods::denied_by_name`]), and any method whose
//! name is mentioned nowhere in the workspace outside its own definition
//! (called by convention: GraphQL, Pundit, ...). In a controller only
//! actions are scopes.
//!
//! "Mentioned nowhere" makes one file's scopes depend on other files, so this
//! is recomputed on every build/update from the cached per-file facts. Name
//! ids come from a workspace-wide, append-only [`Interner`]; they never reach
//! the output.

use crate::analyse::FileEntry;
use crate::methods::{self, Denied, MethodFacts};
use crate::paths::{self, SourceKind};
use ripple_core::mentions::{MentionTable, name_id};
use ripple_core::model::ScopeRange;
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Default)]
pub struct Interner {
    index: HashMap<String, u32>,
    names: Vec<String>,
}

impl Interner {
    pub fn get(&self, id: u32) -> &str {
        &self.names[id as usize]
    }
    pub fn len(&self) -> usize {
        self.names.len()
    }

    fn intern(&mut self, s: String) -> u32 {
        if let Some(&i) = self.index.get(&s) {
            return i;
        }
        let i = self.names.len() as u32;
        self.names.push(s.clone());
        self.index.insert(s, i);
        i
    }

    /// Replaces the facts' local name ids with workspace-wide ones.
    pub fn globalize(&mut self, m: &mut MethodFacts) {
        if m.names.is_empty() {
            return;
        }
        let ids: Vec<u32> = std::mem::take(&mut m.names).into_iter().map(|n| self.intern(n)).collect();
        for d in &mut m.defs {
            d.name = ids[d.name as usize];
        }
        for c in &mut m.counts {
            c.0 = ids[c.0 as usize];
        }
        for o in &mut m.own {
            *o = ids[*o as usize];
        }
        m.own.sort_unstable();
        m.counts.sort_unstable();
    }
}

pub struct Output {
    pub scopes: BTreeMap<String, Vec<ScopeRange>>,
    pub mentions: MentionTable,
    pub stats: BTreeMap<String, u64>,
}

/// Scope ranges and the mention table for the current file set.
pub fn compute(files: &BTreeMap<String, FileEntry>, names: &Interner) -> Output {
    let mut counts = vec![0u32; names.len()];
    for e in files.values() {
        for &(id, c) in &e.methods.counts {
            counts[id as usize] += c;
        }
    }
    let mut denied: BTreeMap<Denied, u64> = BTreeMap::new();
    let mut unmentioned: Vec<u32> = Vec::new();
    let mut scopes = BTreeMap::new();
    let mut table = Vec::new();
    let mut stats: BTreeMap<String, u64> = BTreeMap::new();
    let mut stat = |k: &str, v: u64| *stats.entry(k.to_string()).or_default() += v;
    for (path, e) in files {
        if let Some(m) = &e.mentions {
            table.push((path.clone(), m.clone()));
            stat("ruby_mention_files", 1);
            stat("ruby_mention_pairs", m.pairs.len() as u64);
            if m.opaque {
                stat("ruby_mention_opaque_files", 1);
            }
        }
        let controller = paths::is_scoped_controller(path);
        let def_scope: Vec<bool> = e
            .methods
            .defs
            .iter()
            .map(|d| {
                let name = names.get(d.name);
                let why = if controller {
                    Some(Denied::ControllerNonAction)
                } else if let Some(w) = methods::denied_by_name(name) {
                    Some(w)
                } else if counts[d.name as usize] == 0 {
                    unmentioned.push(name_id(name));
                    Some(Denied::Unmentioned)
                } else {
                    None
                };
                if let Some(w) = why {
                    *denied.entry(w).or_default() += 1;
                }
                why.is_none()
            })
            .collect();
        stat("ruby_method_defs", e.methods.defs.len() as u64);
        if let Some(sc) = file_scopes(path, e, &def_scope, names).filter(|s| !s.is_empty()) {
            stat("ruby_method_scope_ranges", sc.len() as u64);
            scopes.insert(path.clone(), sc);
        }
    }
    for (why, n) in &denied {
        stats.insert(format!("ruby_method_denied_{}", why.key()), *n);
    }
    unmentioned.sort_unstable();
    unmentioned.dedup();
    stats.insert("ruby_method_unmentioned_names".into(), unmentioned.len() as u64);
    Output { scopes, mentions: MentionTable { files: table, unmentioned: unmentioned.into() }, stats }
}

/// `Facts.scopes` for one file: controller actions, or the methods that have
/// a scope, are defined once in the file and own every line of their range.
fn file_scopes(path: &str, entry: &FileEntry, def_scope: &[bool], names: &Interner) -> Option<Vec<ScopeRange>> {
    if paths::is_scoped_controller(path) {
        return Some(entry.facts.actions.iter().map(|a| ScopeRange { name: a.name.clone(), start: a.start, end: a.end }).collect());
    }
    if entry.kind != Some(SourceKind::Ruby) {
        return None;
    }
    let defs = &entry.methods.defs;
    let mut per_name: HashMap<u32, u32> = HashMap::new();
    for d in defs {
        *per_name.entry(d.name).or_default() += 1;
    }
    let mut out: Vec<ScopeRange> = Vec::new();
    for (i, d) in defs.iter().enumerate() {
        let overlaps = (i > 0 && defs[i - 1].end >= d.start) || (i + 1 < defs.len() && defs[i + 1].start <= d.end);
        if def_scope[i] && per_name[&d.name] == 1 && d.clean && !overlaps {
            out.push(ScopeRange { name: names.get(d.name).to_string(), start: d.start, end: d.end });
        }
    }
    out.sort_by_key(|r| r.start);
    Some(out)
}
