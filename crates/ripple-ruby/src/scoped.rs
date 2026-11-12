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

/// Scope ranges and the mention table for the current file set, from
/// scratch (builds, and the reference [`Cache`] is checked against).
pub fn compute(files: &BTreeMap<String, FileEntry>, names: &Interner) -> Output {
    let mut counts = vec![0u32; names.len()];
    for e in files.values() {
        for &(id, c) in &e.methods.counts {
            counts[id as usize] += c;
        }
    }
    let mut totals = Totals::default();
    let mut unmentioned: Vec<u32> = Vec::new();
    let mut scopes = BTreeMap::new();
    let mut table = Vec::new();
    for (path, e) in files {
        let o = file_out(path, e, &counts, names);
        totals.add(&o, 1);
        unmentioned.extend_from_slice(&o.unmentioned);
        if let Some(m) = o.mentions {
            table.push((path.clone(), m));
        }
        if let Some(sc) = o.scopes {
            scopes.insert(path.clone(), sc);
        }
    }
    unmentioned.sort_unstable();
    unmentioned.dedup();
    let stats = totals.stats(unmentioned.len());
    Output { scopes, mentions: MentionTable { files: table, unmentioned: unmentioned.into() }, stats }
}

/// One file's share of [`compute`]'s output.
#[derive(Debug, Default, Clone, PartialEq)]
struct FileOut {
    /// Global name ids of the file's defs (a def's scope depends on whether
    /// its name is mentioned anywhere).
    defs: Vec<u32>,
    denied: [u64; 5],
    /// `name_id`s of the unmentioned defs.
    unmentioned: Vec<u32>,
    mentions: Option<std::sync::Arc<ripple_core::FileMentions>>,
    /// Non-empty.
    scopes: Option<Vec<ScopeRange>>,
}

impl FileOut {
    fn is_empty(&self) -> bool {
        self.defs.is_empty() && self.mentions.is_none() && self.scopes.is_none()
    }
}

fn denied_index(d: Denied) -> usize {
    match d {
        Denied::Implicit => 0,
        Denied::Operator => 1,
        Denied::Setter => 2,
        Denied::Unmentioned => 3,
        Denied::ControllerNonAction => 4,
    }
}
const DENIED: [Denied; 5] = [Denied::Implicit, Denied::Operator, Denied::Setter, Denied::Unmentioned, Denied::ControllerNonAction];

fn file_out(path: &str, e: &FileEntry, counts: &[u32], names: &Interner) -> FileOut {
    let controller = paths::is_scoped_controller(path);
    let mut out = FileOut { mentions: e.mentions.clone(), ..Default::default() };
    let def_scope: Vec<bool> = e
        .methods
        .defs
        .iter()
        .map(|d| {
            out.defs.push(d.name);
            let name = names.get(d.name);
            let why = if controller {
                Some(Denied::ControllerNonAction)
            } else if let Some(w) = methods::denied_by_name(name) {
                Some(w)
            } else if counts.get(d.name as usize).copied().unwrap_or(0) == 0 {
                out.unmentioned.push(name_id(name));
                Some(Denied::Unmentioned)
            } else {
                None
            };
            if let Some(w) = why {
                out.denied[denied_index(w)] += 1;
            }
            why.is_none()
        })
        .collect();
    out.scopes = file_scopes(path, e, &def_scope, names).filter(|s| !s.is_empty());
    out
}

/// Sums behind the stats (a key is present exactly when `compute` used to
/// bump it).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Totals {
    files: i64,
    mention_files: i64,
    mention_pairs: i64,
    opaque: i64,
    defs: i64,
    ranges: i64,
    scope_files: i64,
    denied: [i64; 5],
}

impl Totals {
    fn add(&mut self, o: &FileOut, sign: i64) {
        self.files += sign;
        if let Some(m) = &o.mentions {
            self.mention_files += sign;
            self.mention_pairs += sign * m.pairs.len() as i64;
            if m.opaque {
                self.opaque += sign;
            }
        }
        self.defs += sign * o.defs.len() as i64;
        if let Some(sc) = &o.scopes {
            self.scope_files += sign;
            self.ranges += sign * sc.len() as i64;
        }
        for (t, d) in self.denied.iter_mut().zip(o.denied) {
            *t += sign * d as i64;
        }
    }

    fn stats(&self, unmentioned_names: usize) -> BTreeMap<String, u64> {
        let mut stats = BTreeMap::new();
        if self.mention_files > 0 {
            stats.insert("ruby_mention_files".into(), self.mention_files as u64);
            stats.insert("ruby_mention_pairs".into(), self.mention_pairs as u64);
        }
        if self.opaque > 0 {
            stats.insert("ruby_mention_opaque_files".into(), self.opaque as u64);
        }
        if self.files > 0 {
            stats.insert("ruby_method_defs".into(), self.defs as u64);
        }
        if self.scope_files > 0 {
            stats.insert("ruby_method_scope_ranges".into(), self.ranges as u64);
        }
        for (d, n) in DENIED.iter().zip(self.denied) {
            if n > 0 {
                stats.insert(format!("ruby_method_denied_{}", d.key()), n as u64);
            }
        }
        stats.insert("ruby_method_unmentioned_names".into(), unmentioned_names as u64);
        stats
    }
}

/// [`compute`] kept up to date by file: the workspace-wide mention counts
/// are patched as entries come and go ([`Cache::add`] / [`Cache::retract`]),
/// and [`Cache::refresh`] recomputes the files that changed plus the files
/// defining a name whose count crossed zero (the only cross-file input).
#[derive(Default)]
pub struct Cache {
    counts: Vec<u32>,
    files: BTreeMap<String, FileOut>,
    /// Number of entries (files without any output included).
    entries: i64,
    dirty: std::collections::BTreeSet<String>,
    /// Names whose count went to or from zero since the last refresh.
    flipped: std::collections::HashSet<u32>,
    totals: Totals,
    unmentioned: BTreeMap<u32, u32>,
    unmentioned_list: Option<std::sync::Arc<[u32]>>,
}

impl Cache {
    pub fn add(&mut self, path: &str, e: &FileEntry) {
        for &(id, c) in &e.methods.counts {
            let i = id as usize;
            if i >= self.counts.len() {
                self.counts.resize(i + 1, 0);
            }
            if self.counts[i] == 0 && c > 0 {
                self.flipped.insert(id);
            }
            self.counts[i] += c;
        }
        self.entries += 1;
        self.dirty.insert(path.to_string());
    }

    pub fn retract(&mut self, path: &str, e: &FileEntry) {
        for &(id, c) in &e.methods.counts {
            let n = &mut self.counts[id as usize];
            *n -= c;
            if *n == 0 && c > 0 {
                self.flipped.insert(id);
            }
        }
        self.entries -= 1;
        self.dirty.insert(path.to_string());
    }

    pub fn refresh(&mut self, files: &BTreeMap<String, FileEntry>, names: &Interner) -> Output {
        if !self.flipped.is_empty() {
            for (p, o) in &self.files {
                if o.defs.iter().any(|d| self.flipped.contains(d)) {
                    self.dirty.insert(p.clone());
                }
            }
            self.flipped.clear();
        }
        let mut list_changed = self.unmentioned_list.is_none();
        for p in std::mem::take(&mut self.dirty) {
            if let Some(old) = self.files.remove(&p) {
                self.totals.add(&old, -1);
                for u in &old.unmentioned {
                    let n = self.unmentioned.get_mut(u).expect("counted");
                    *n -= 1;
                    if *n == 0 {
                        self.unmentioned.remove(u);
                        list_changed = true;
                    }
                }
            }
            let Some(e) = files.get(&p) else { continue };
            let o = file_out(&p, e, &self.counts, names);
            for &u in &o.unmentioned {
                let n = self.unmentioned.entry(u).or_default();
                list_changed |= *n == 0;
                *n += 1;
            }
            self.totals.add(&o, 1);
            if !o.is_empty() {
                self.files.insert(p, o);
            }
        }
        if list_changed {
            self.unmentioned_list = Some(self.unmentioned.keys().copied().collect());
        }
        let unmentioned = self.unmentioned_list.clone().expect("set above");
        let mut totals = self.totals.clone();
        // Files without output only count towards `files`.
        totals.files = self.entries;
        let stats = totals.stats(unmentioned.len());
        let mut scopes = BTreeMap::new();
        let mut table = Vec::new();
        for (p, o) in &self.files {
            if let Some(m) = &o.mentions {
                table.push((p.clone(), m.clone()));
            }
            if let Some(sc) = &o.scopes {
                scopes.insert(p.clone(), sc.clone());
            }
        }
        Output { scopes, mentions: MentionTable { files: table, unmentioned }, stats }
    }
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
