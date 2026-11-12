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
/// scratch (the reference [`Cache`] is checked against in tests).
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
        totals.add(e, o.as_deref(), 1);
        if let Some(m) = &e.mentions {
            table.push((path.clone(), m.clone()));
        }
        if let Some(o) = o {
            unmentioned.extend_from_slice(&o.unmentioned);
            if let Some(idx) = &o.scopes {
                scopes.insert(path.clone(), ranges(path, e, idx, names));
            }
        }
    }
    unmentioned.sort_unstable();
    unmentioned.dedup();
    let stats = totals.stats(files.len() as i64, unmentioned.len());
    Output { scopes, mentions: MentionTable { files: table, unmentioned: unmentioned.into() }, stats }
}

/// One file's share of [`compute`]'s output that depends on other files
/// (through the mention counts), cached on its entry by [`Cache`]. `None`
/// on the entry when all of it is empty.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct FileOut {
    denied: [u32; 5],
    /// `name_id`s of the unmentioned defs.
    unmentioned: Box<[u32]>,
    /// The ranges, in output order, as indices into `facts.actions` (scoped
    /// controller) or `methods.defs`. Non-empty.
    scopes: Option<Box<[u32]>>,
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

fn file_out(path: &str, e: &FileEntry, counts: &[u32], names: &Interner) -> Option<Box<FileOut>> {
    let controller = paths::is_scoped_controller(path);
    let mut out = FileOut::default();
    let mut unmentioned = Vec::new();
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
            } else if counts.get(d.name as usize).copied().unwrap_or(0) == 0 {
                unmentioned.push(name_id(name));
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
    out.unmentioned = unmentioned.into();
    out.scopes = scope_indices(path, e, &def_scope).filter(|s| !s.is_empty()).map(Into::into);
    (out != FileOut::default()).then(|| Box::new(out))
}

/// Sums behind the stats (a key is present exactly when the per-file loop
/// this replaced would have bumped it).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Totals {
    mention_files: i64,
    mention_pairs: i64,
    opaque: i64,
    defs: i64,
    ranges: i64,
    scope_files: i64,
    denied: [i64; 5],
}

impl Totals {
    fn add(&mut self, e: &FileEntry, o: Option<&FileOut>, sign: i64) {
        if let Some(m) = &e.mentions {
            self.mention_files += sign;
            self.mention_pairs += sign * m.pairs.len() as i64;
            if m.opaque {
                self.opaque += sign;
            }
        }
        self.defs += sign * e.methods.defs.len() as i64;
        let Some(o) = o else { return };
        if let Some(sc) = &o.scopes {
            self.scope_files += sign;
            self.ranges += sign * sc.len() as i64;
        }
        for (t, d) in self.denied.iter_mut().zip(o.denied) {
            *t += sign * i64::from(d);
        }
    }

    fn stats(&self, files: i64, unmentioned_names: usize) -> BTreeMap<String, u64> {
        let mut stats = BTreeMap::new();
        if self.mention_files > 0 {
            stats.insert("ruby_mention_files".into(), self.mention_files as u64);
            stats.insert("ruby_mention_pairs".into(), self.mention_pairs as u64);
        }
        if self.opaque > 0 {
            stats.insert("ruby_mention_opaque_files".into(), self.opaque as u64);
        }
        if files > 0 {
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

/// [`compute`] kept up to date by file. The workspace-wide mention counts
/// are patched as entries come and go ([`Cache::add`] / [`Cache::retract`],
/// which also takes back the entry's cached [`FileOut`]), and
/// [`Cache::refresh`] recomputes the [`FileOut`] of the files that changed
/// plus of the files defining a name whose count crossed zero (the only
/// cross-file input), then assembles the output from the entries.
#[derive(Default)]
pub struct Cache {
    counts: Vec<u32>,
    dirty: std::collections::BTreeSet<String>,
    /// Names whose count went to or from zero since the last refresh.
    flipped: std::collections::HashSet<u32>,
    totals: Totals,
    /// `name_id` -> unmentioned defs with it.
    unmentioned: BTreeMap<u32, u32>,
    unmentioned_list: Option<std::sync::Arc<[u32]>>,
}

impl Cache {
    /// A new entry for `path` (its `scoped` slot not computed yet).
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
        self.dirty.insert(path.to_string());
    }

    /// `e` (previously added) leaves `path`.
    pub fn retract(&mut self, path: &str, e: &FileEntry) {
        for &(id, c) in &e.methods.counts {
            let n = &mut self.counts[id as usize];
            *n -= c;
            if *n == 0 && c > 0 {
                self.flipped.insert(id);
            }
        }
        if e.scoped_done {
            self.contribute(e, e.scoped.as_deref(), -1);
        }
        self.dirty.insert(path.to_string());
    }

    fn contribute(&mut self, e: &FileEntry, o: Option<&FileOut>, sign: i64) {
        self.totals.add(e, o, sign);
        for &u in o.map_or(&[][..], |o| &o.unmentioned[..]) {
            let n = self.unmentioned.entry(u).or_default();
            if sign > 0 {
                *n += 1;
                if *n == 1 {
                    self.unmentioned_list = None;
                }
            } else {
                *n -= 1;
                if *n == 0 {
                    self.unmentioned.remove(&u);
                    self.unmentioned_list = None;
                }
            }
        }
    }

    pub fn refresh(&mut self, files: &mut BTreeMap<String, FileEntry>, names: &Interner) -> Output {
        if !self.flipped.is_empty() {
            for (p, e) in files.iter() {
                if e.methods.defs.iter().any(|d| self.flipped.contains(&d.name)) {
                    self.dirty.insert(p.clone());
                }
            }
            self.flipped.clear();
        }
        for p in std::mem::take(&mut self.dirty) {
            let Some(e) = files.get_mut(&p) else { continue };
            if std::mem::take(&mut e.scoped_done) {
                let old = e.scoped.take();
                self.contribute(e, old.as_deref(), -1);
            }
            e.scoped = file_out(&p, e, &self.counts, names);
            e.scoped_done = true;
            self.contribute(e, e.scoped.as_deref(), 1);
        }
        let unmentioned = self.unmentioned_list.get_or_insert_with(|| self.unmentioned.keys().copied().collect()).clone();
        let stats = self.totals.stats(files.len() as i64, unmentioned.len());
        let mut scopes = BTreeMap::new();
        let mut table = Vec::new();
        for (p, e) in files.iter() {
            if let Some(m) = &e.mentions {
                table.push((p.clone(), m.clone()));
            }
            if let Some(idx) = e.scoped.as_ref().and_then(|o| o.scopes.as_ref()) {
                scopes.insert(p.clone(), ranges(p, e, idx, names));
            }
        }
        Output { scopes, mentions: MentionTable { files: table, unmentioned }, stats }
    }
}

/// Which actions (scoped controller) or method defs of a file get a scope
/// range, in output order; `None` for files without scopes.
fn scope_indices(path: &str, entry: &FileEntry, def_scope: &[bool]) -> Option<Vec<u32>> {
    if paths::is_scoped_controller(path) {
        return Some((0..entry.facts.actions.len() as u32).collect());
    }
    if entry.kind != Some(SourceKind::Ruby) {
        return None;
    }
    let defs = &entry.methods.defs;
    let mut per_name: HashMap<u32, u32> = HashMap::new();
    for d in defs {
        *per_name.entry(d.name).or_default() += 1;
    }
    let mut out: Vec<u32> = Vec::new();
    for (i, d) in defs.iter().enumerate() {
        let overlaps = (i > 0 && defs[i - 1].end >= d.start) || (i + 1 < defs.len() && defs[i + 1].start <= d.end);
        if def_scope[i] && per_name[&d.name] == 1 && d.clean && !overlaps {
            out.push(i as u32);
        }
    }
    out.sort_by_key(|&i| defs[i as usize].start);
    Some(out)
}

fn ranges(path: &str, entry: &FileEntry, idx: &[u32], names: &Interner) -> Vec<ScopeRange> {
    if paths::is_scoped_controller(path) {
        return idx
            .iter()
            .map(|&i| {
                let a = &entry.facts.actions[i as usize];
                ScopeRange { name: a.name.clone(), start: a.start, end: a.end }
            })
            .collect();
    }
    idx.iter()
        .map(|&i| {
            let d = &entry.methods.defs[i as usize];
            ScopeRange { name: names.get(d.name).to_string(), start: d.start, end: d.end }
        })
        .collect()
}
