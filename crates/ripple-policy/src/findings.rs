//! Findings derived from the change itself rather than from rules:
//! removed methods that are still mentioned, and reasons the impact may be
//! incomplete.

use crate::changed::FileStatus;
use crate::eval::{Changed, Ctx, EvalOptions};
use crate::lang::Language;
use crate::report::{AlsoMentioned, Incomplete, Mention, MentionHow, RemovedMethod, budget};
use ripple_core::NodeId;
use ripple_core::mentions::name_id;
use std::collections::BTreeSet;
use std::path::Path;

/// Files larger than this are not read for line numbers.
const MAX_READ: u64 = 2 << 20;

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The first line of `text` that mentions method `name` as a word and is
/// neither a comment nor its own `def`. Setters (`x=`) match `x`.
pub fn mention_line(text: &str, name: &str) -> Option<u32> {
    let word = name.strip_suffix('=').filter(|w| !w.is_empty() && w.chars().all(|c| is_ident(c) || c == '?' || c == '!')).unwrap_or(name);
    let ends_ident = word.chars().last().is_some_and(is_ident);
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_start();
        if t.starts_with('#') || crate::changed::def_name(t).is_some_and(|d| d == name) {
            continue;
        }
        let mut from = 0;
        while let Some(pos) = line[from..].find(word) {
            let at = from + pos;
            let before = line[..at].chars().next_back();
            let after = line[at + word.len()..].chars().next();
            let ok_before = !before.is_some_and(|c| is_ident(c) || c == '@' || c == '$');
            let ok_after = !after.is_some_and(|c| is_ident(c) || (ends_ident && (c == '?' || c == '!')));
            if ok_before && ok_after {
                return Some(i as u32 + 1);
            }
            from = at + word.len();
        }
    }
    None
}

pub(crate) fn read(root: Option<&Path>, file: &str) -> Option<String> {
    let path = root?.join(file);
    if std::fs::metadata(&path).ok()?.len() > MAX_READ {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Texts of files read for line numbers, shared by every finding of one
/// evaluation. `prefetch` reads a batch in parallel (on GitLab, reading a
/// few hundred candidate files one after another took ~10 ms).
#[derive(Default)]
pub(crate) struct Texts {
    map: std::collections::HashMap<String, Option<String>>,
}

impl Texts {
    pub(crate) fn get(&mut self, root: Option<&Path>, file: &str) -> Option<&str> {
        if !self.map.contains_key(file) {
            let t = read(root, file);
            self.map.insert(file.to_string(), t);
        }
        self.map[file].as_deref()
    }

    pub(crate) fn prefetch(&mut self, root: Option<&Path>, files: &[&str]) {
        let Some(root) = root else { return };
        let mut todo: Vec<&str> = files.iter().copied().filter(|f| !self.map.contains_key(*f)).collect();
        todo.sort_unstable();
        todo.dedup();
        if todo.len() < 8 {
            for f in todo {
                self.map.insert(f.to_string(), read(Some(root), f));
            }
            return;
        }
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8);
        let chunk = todo.len().div_ceil(threads);
        let read_back: Vec<(String, Option<String>)> = std::thread::scope(|s| {
            let handles: Vec<_> = todo.chunks(chunk).map(|part| s.spawn(move || part.iter().map(|f| (f.to_string(), read(Some(root), f))).collect::<Vec<_>>())).collect();
            handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
        });
        self.map.extend(read_back);
    }
}

/// For a set of method-name ids: the files defining and the files
/// mentioning each, in node order. One pass over the workspace (split
/// across threads) serves every by-name finding of an evaluation.
#[derive(Default)]
pub(crate) struct Sweep {
    ids: Vec<u32>,
    defs: Vec<Vec<NodeId>>,
    mentions: Vec<Vec<NodeId>>,
}

impl Sweep {
    pub(crate) fn new(g: &ripple_core::Graph, mut ids: Vec<u32>) -> Self {
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            return Self::default();
        }
        let n = g.node_count();
        let scan = |from: usize, to: usize| {
            let mut defs = vec![Vec::new(); ids.len()];
            let mut mentions = vec![Vec::new(); ids.len()];
            for node in from as NodeId..to as NodeId {
                let Some(m) = g.mentions(node) else { continue };
                for d in m.defs.iter() {
                    // `ids` is small and sorted: a binary search beats hashing.
                    if let Ok(k) = ids.binary_search(d) {
                        defs[k].push(node);
                    }
                }
                // Both lists are sorted: each search starts where the last ended.
                let p = &m.pairs;
                let (Some(first), Some(last)) = (p.first(), p.last()) else { continue };
                let mut lo = 0;
                for (k, &id) in ids.iter().enumerate() {
                    if id < first.0 {
                        continue;
                    }
                    if id > last.0 {
                        break;
                    }
                    lo += p[lo..].partition_point(|&(x, _)| x < id);
                    if lo < p.len() && p[lo].0 == id {
                        mentions[k].push(node);
                    }
                }
            }
            (defs, mentions)
        };
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(16);
        let chunk = n.div_ceil(threads).max(4096);
        let parts: Vec<(Vec<Vec<NodeId>>, Vec<Vec<NodeId>>)> = if n <= chunk {
            vec![scan(0, n)]
        } else {
            std::thread::scope(|s| {
                let handles: Vec<_> = (0..n).step_by(chunk).map(|from| s.spawn(move || scan(from, (from + chunk).min(n)))).collect();
                handles.into_iter().map(|h| h.join().expect("sweep thread")).collect()
            })
        };
        let mut defs = vec![Vec::new(); ids.len()];
        let mut mentions = vec![Vec::new(); ids.len()];
        for (d, m) in parts {
            for (k, v) in d.into_iter().enumerate() {
                defs[k].extend(v);
            }
            for (k, v) in m.into_iter().enumerate() {
                mentions[k].extend(v);
            }
        }
        Self { ids, defs, mentions }
    }

    fn slot(&self, id: u32) -> Option<usize> {
        self.ids.binary_search(&id).ok()
    }
    /// Files defining `id` (empty when it was not swept).
    pub(crate) fn defining(&self, id: u32) -> &[NodeId] {
        self.slot(id).map_or(&[], |k| &self.defs[k])
    }
    /// Files mentioning `id`.
    pub(crate) fn mentioning(&self, id: u32) -> &[NodeId] {
        self.slot(id).map_or(&[], |k| &self.mentions[k])
    }
}

/// Name ids the by-name findings will ask a [`Sweep`] about.
pub(crate) fn sweep_ids(ctx: &Ctx, of: &[NamesOf]) -> Vec<u32> {
    let g = ctx.graph;
    let mut ids: Vec<u32> = of.iter().filter(|o| g.mentions(o.node).is_some()).flat_map(|o| o.names.iter().map(|n| name_id(n))).collect();
    for c in &ctx.changed {
        if let Some(ch) = &c.file.change {
            ids.extend(ch.removed_methods.iter().map(|n| name_id(n)));
        }
    }
    ids
}

/// Is `node` worth checking for a Ruby method name (not JS/TS)?
fn rubyish(ctx: &Ctx, node: NodeId) -> bool {
    ctx.class(node).language != Language::Js
}

/// For every method a changed file no longer defines, the files that still
/// mention its name: the file itself, its direct dependents (and those of
/// its old path), then any other file by name. Returns the entries (within
/// the budget) and the number of entries before the budget.
pub(crate) fn removed_methods(ctx: &Ctx, opts: &EvalOptions, sweep: &Sweep, texts: &mut Texts) -> (Vec<RemovedMethod>, usize) {
    let root = opts.root.as_deref();
    let g = ctx.graph;
    let per_name = if opts.all { usize::MAX } else { opts.limit.unwrap_or(budget::MENTIONS) };
    // One query per (changed file, removed name).
    struct Query<'c> {
        changed: &'c crate::eval::Changed,
        name: &'c String,
        id: u32,
        /// The changed file's nodes (new and old path).
        nodes: Vec<NodeId>,
        /// Their direct dependents (shared by the file's names).
        direct: std::rc::Rc<BTreeSet<NodeId>>,
        others: Vec<NodeId>,
        other_definitions: usize,
    }
    let mut queries: Vec<Query> = Vec::new();
    for c in &ctx.changed {
        let Some(change) = &c.file.change else { continue };
        if change.removed_methods.is_empty() {
            continue;
        }
        let nodes: Vec<NodeId> = c.node.into_iter().chain(c.file.old_path.as_deref().and_then(|o| g.id(o))).collect();
        let mut direct: BTreeSet<NodeId> = BTreeSet::new();
        for &n in &nodes {
            for (d, e) in g.dependents(n) {
                if g.edge(e).2.confidence >= ctx.eff.min_confidence && !nodes.contains(&d) {
                    direct.insert(d);
                }
            }
        }
        let direct = std::rc::Rc::new(direct);
        for name in &change.removed_methods {
            queries.push(Query { changed: c, name, id: name_id(name), nodes: nodes.clone(), direct: direct.clone(), others: Vec::new(), other_definitions: 0 });
        }
    }
    if queries.is_empty() {
        return (Vec::new(), 0);
    }
    for q in &mut queries {
        q.other_definitions = sweep.defining(q.id).iter().filter(|n| !q.nodes.contains(n)).count();
        q.others = sweep.mentioning(q.id).iter().copied().filter(|n| !q.nodes.contains(n) && !q.direct.contains(n)).collect();
        if opts.all || q.other_definitions < budget::COMMON_NAME_DEFINITIONS {
            // Nearest first: same directory tree, then path.
            let file = &q.changed.file.path;
            q.others.sort_by_cached_key(|&n| (std::cmp::Reverse(shared_dirs(g.path(n), file)), g.path(n).to_string()));
        }
    }
    // Read every file a line number will be looked up in, in parallel.
    let mut want: Vec<&str> = Vec::new();
    for q in &queries {
        let c = q.changed;
        if c.file.status != FileStatus::Deleted {
            want.push(&c.file.path);
        }
        let id = [q.id];
        want.extend(q.direct.iter().filter(|&&d| rubyish(ctx, d) && !g.mentions(d).is_some_and(|m| !m.opaque && !m.mentions_any(&id))).map(|&d| g.path(d)));
        if opts.all || q.other_definitions < budget::COMMON_NAME_DEFINITIONS {
            want.extend(q.others.iter().take(per_name.saturating_add(5)).map(|&n| g.path(n)));
        }
    }
    texts.prefetch(root, &want);
    let mut out = Vec::new();
    for q in queries {
        let c = q.changed;
        let name = q.name;
        let id = [q.id];
        let mut mentions: Vec<Mention> = Vec::new();
        let mut total = 0;
        // The file itself (a private helper's callers live here).
        if c.file.status != FileStatus::Deleted {
            let own_text = texts.get(root, &c.file.path);
            let line = own_text.and_then(|t| mention_line(t, name));
            let by_facts = c.node.and_then(|n| g.mentions(n)).map(|m| m.mentions_any(&id));
            if line.is_some() || (own_text.is_none() && by_facts == Some(true)) {
                total += 1;
                mentions.push(Mention { file: c.file.path.clone(), line, how: MentionHow::SameFile });
            }
        }
        // Direct dependents: the facts decide when known, else the text.
        for &d in q.direct.iter() {
            if !rubyish(ctx, d) {
                continue;
            }
            let path = g.path(d);
            let facts = g.mentions(d);
            // Facts that say "no" settle it without reading the file.
            if facts.is_some_and(|m| !m.opaque && !m.mentions_any(&id)) {
                continue;
            }
            let line = texts.get(root, path).and_then(|t| mention_line(t, name));
            let hit = match facts {
                Some(m) => m.opaque && line.is_some() || m.mentions_any(&id),
                None => line.is_some(),
            };
            if hit {
                total += 1;
                if mentions.len() < per_name {
                    mentions.push(Mention { file: path.to_string(), line, how: MentionHow::Dependent });
                }
            }
        }
        // Anywhere else, by name (method facts only; no edge links them).
        let others = q.others;
        total += others.len();
        // Another class defining the name makes by-name matches doubtful:
        // past a few definitions they are counted, not listed.
        let other_definitions = q.other_definitions;
        let common = !opts.all && other_definitions >= budget::COMMON_NAME_DEFINITIONS;
        if !common {
            for n in others {
                if mentions.len() >= per_name {
                    break;
                }
                let path = g.path(n);
                let line = texts.get(root, path).and_then(|t| mention_line(t, name));
                // Read but not found as a word (a hash collision, or only
                // in a comment): not a mention after all.
                if root.is_some() && line.is_none() {
                    total -= 1;
                    continue;
                }
                mentions.push(Mention { file: path.to_string(), line, how: MentionHow::ByName });
            }
        }
        if total > 0 {
            out.push(RemovedMethod { file: c.file.path.clone(), method: name.clone(), mentions, mentions_total: total, other_definitions });
        }
    }
    out.sort_by(|a, b| (&a.file, &a.method).cmp(&(&b.file, &b.method)));
    let total = out.len();
    out.truncate(opts.cap_removed());
    (out, total)
}

/// Names of `names` that `text` defines as singleton methods (`def self.x`).
pub(crate) fn singleton_names(text: &str, names: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("def self.") {
            let end = rest.find(|c: char| !(is_ident(c) || c == '?' || c == '!' || c == '=')).unwrap_or(rest.len());
            let n = &rest[..end];
            if names.iter().any(|x| x == n) && !out.iter().any(|x| x == n) {
                out.push(n.to_string());
            }
        }
    }
    out
}

/// Up to `max` lines of `text` calling `name` on one of `consts`
/// (`Link.fetch`, `::Link.fetch`, `Foo::Link.fetch`).
pub(crate) fn constant_call_lines(text: &str, consts: &[String], name: &str, max: usize) -> Vec<u32> {
    let mut out = Vec::new();
    let pats: Vec<String> = consts.iter().map(|c| format!("{c}.{name}")).collect();
    for (i, line) in text.lines().enumerate() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        for p in &pats {
            let mut from = 0;
            while let Some(pos) = line[from..].find(p.as_str()) {
                let at = from + pos;
                let before = line[..at].chars().next_back();
                let after = line[at + p.len()..].chars().next();
                if !before.is_some_and(|c| is_ident(c) || c == '@') && !after.is_some_and(|c| is_ident(c) || c == '?' || c == '!') {
                    if out.len() < max {
                        out.push(i as u32 + 1);
                    }
                    break;
                }
                from = at + p.len();
            }
            if out.last() == Some(&(i as u32 + 1)) {
                break;
            }
        }
    }
    out
}

/// The constants an edge detail names, outermost-qualified and last
/// segment: `Charge::Disputable#x` gives `Charge::Disputable`, `Disputable`.
pub(crate) fn detail_constants(detail: &str) -> Vec<String> {
    let c = detail.split(['#', ' ']).next().unwrap_or("").trim_start_matches("::");
    if c.is_empty() || !c.starts_with(|ch: char| ch.is_ascii_uppercase()) {
        return Vec::new();
    }
    let mut v = vec![c.to_string()];
    if let Some((_, last)) = c.rsplit_once("::") {
        v.push(last.to_string());
    }
    v
}

/// `app/controllers/admin/users_controller.rb` gives `admin/users`.
pub(crate) fn controller_name(path: &str) -> Option<&str> {
    path.strip_prefix("app/controllers/")?.strip_suffix("_controller.rb")
}

/// Route lines of `text` naming `controller#action`, as `(line, action)`.
pub(crate) fn route_lines(text: &str, controller: &str) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        for q in ['"', '\'', ':'] {
            let pat = format!("{q}{controller}#");
            if let Some(pos) = line.find(&pat) {
                let rest = &line[pos + pat.len()..];
                let end = rest.find(|c: char| !is_ident(c)).unwrap_or(rest.len());
                if end > 0 {
                    out.push((i as u32 + 1, rest[..end].to_string()));
                }
                break;
            }
        }
    }
    out
}

/// What [`also_mentioned`] needs per changed file: its method names.
pub(crate) struct NamesOf {
    pub(crate) node: NodeId,
    pub(crate) file: String,
    pub(crate) names: Vec<String>,
    /// Those of `names` defined as `def self.x`.
    pub(crate) singleton: Vec<String>,
}

/// Number of leading path components `a` and `b` share.
fn shared_dirs(a: &str, b: &str) -> usize {
    a.split('/').zip(b.split('/')).take_while(|(x, y)| x == y).count()
}

/// For every changed method name, the workspace files that mention it but
/// have no edge to the changed file (`exclude`: the changed files and their
/// direct dependents). Listed nearest first, confirmed in the file text when
/// it can be read. Returns the entries and the total number of files.
pub(crate) fn also_mentioned(ctx: &Ctx, opts: &EvalOptions, of: &[NamesOf], exclude: &BTreeSet<NodeId>, sweep: &Sweep, texts: &mut Texts) -> (Vec<AlsoMentioned>, usize) {
    let g = ctx.graph;
    let root = opts.root.as_deref();
    let cap = if opts.all { usize::MAX } else { opts.limit.unwrap_or(budget::ALSO_MENTIONED) };
    // One entry per name: the exclusion set is shared, so a name changed
    // in two files has the same candidates.
    let mut queries: Vec<(&NamesOf, &String, u32)> =
        of.iter().filter(|o| g.mentions(o.node).is_some()).flat_map(|o| o.names.iter().map(move |n| (o, n, name_id(n)))).collect();
    let mut seen_names = std::collections::HashSet::new();
    queries.retain(|(_, n, _)| seen_names.insert(n.as_str()));
    let definitions: Vec<usize> = queries.iter().map(|(o, _, id)| sweep.defining(*id).iter().filter(|&&n| n != o.node).count()).collect();
    let mut cands: Vec<Vec<NodeId>> = queries.iter().map(|(_, _, id)| sweep.mentioning(*id).iter().copied().filter(|n| !exclude.contains(n)).collect()).collect();
    let list_caps: Vec<usize> = queries
        .iter()
        .enumerate()
        .map(|(q, (o, name, _))| if opts.all { cap } else if definitions[q] + 1 > budget::COMMON_NAME_DEFINITIONS || o.singleton.contains(name) { 0 } else { cap })
        .collect();
    for (q, (o, _, _)) in queries.iter().enumerate() {
        // Nearest first (only lists that are shown need an order).
        if list_caps[q] > 0 {
            cands[q].sort_by_cached_key(|&n| (std::cmp::Reverse(shared_dirs(g.path(n), &o.file)), g.path(n).to_string()));
        }
    }
    let want: Vec<&str> = cands.iter().zip(&list_caps).flat_map(|(c, &lc)| c.iter().take(lc.saturating_add(5)).map(|&n| g.path(n))).collect();
    texts.prefetch(root, &want);
    let mut out = Vec::new();
    let mut grand = 0;
    for (q, (o, name, _)) in queries.iter().enumerate() {
        let cands = std::mem::take(&mut cands[q]);
        let mut total = cands.len();
        let mut files = Vec::new();
        // A common name (several definitions) or a singleton method: the
        // mentions are mostly other methods, so only the count is given.
        let singleton = o.singleton.contains(name);
        let list_cap = list_caps[q];
        for n in cands {
            if files.len() >= list_cap {
                break;
            }
            let path = g.path(n);
            let line = texts.get(root, path).and_then(|t| mention_line(t, name));
            if root.is_some() && line.is_none() {
                // A hash collision, or only in a comment.
                total -= 1;
                continue;
            }
            files.push(Mention { file: path.to_string(), line, how: MentionHow::ByName });
        }
        grand += total;
        out.push(AlsoMentioned {
            name: (*name).clone(),
            of: o.file.clone(),
            definitions: definitions[q] + 1,
            total,
            singleton,
            files,
        });
    }
    (out, grand)
}

/// Reasons the impact of the changed files may be larger than reported
/// that the graph itself can tell (the query side adds unresolved
/// references and index staleness).
pub(crate) fn incomplete(ctx: &Ctx) -> Vec<Incomplete> {
    let mut out = Vec::new();
    let add = |out: &mut Vec<Incomplete>, reason: &str, c: &Changed, message: &str| {
        out.push(Incomplete { reason: reason.to_string(), file: Some(c.file.path.clone()), message: message.to_string() });
    };
    for c in &ctx.changed {
        if c.class.global {
            add(&mut out, "global_file", c, "matches `global_files`: the whole app may depend on it in ways no static graph sees");
        }
        match c.node {
            None if c.file.status == FileStatus::Deleted => {
                add(&mut out, "not_in_graph", c, "deleted and not in the index: the files that referenced it cannot be listed")
            }
            None if c.class.language.is_source() => add(&mut out, "not_in_graph", c, "not in the dependency graph: its impact is unknown, not empty"),
            None => {}
            Some(n) => {
                if let Some(m) = ctx.graph.mentions(n)
                    && (!m.self_dynamic.is_empty() || m.opaque)
                {
                    add(
                        &mut out,
                        "dynamic_dispatch",
                        c,
                        "dispatches by computed name (`send`, `method_missing`, `eval`): methods reached that way are not in the graph",
                    );
                }
            }
        }
    }
    out
}

impl EvalOptions {
    fn cap_removed(&self) -> usize {
        match (self.all, self.limit) {
            (true, None) => usize::MAX,
            (_, Some(l)) => l,
            (false, None) => budget::REMOVED,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mention_line;

    #[test]
    fn routes_singletons_and_constant_calls() {
        use super::*;
        let routes = "  get \"/d\" => \"dashboard#index\"\n  # get \"/x\" => \"dashboard#old\"\n  get \"/d/c\", to: 'dashboard#customers_count'\n  get \"/o\" => \"other#index\"\n";
        assert_eq!(route_lines(routes, "dashboard"), [(1, "index".to_string()), (3, "customers_count".to_string())]);
        assert_eq!(controller_name("app/controllers/admin/users_controller.rb"), Some("admin/users"));
        let link = "class Link\n  def self.fetch(x)\n  end\n  def fetch_all\n  end\nend\n";
        assert_eq!(singleton_names(link, &["fetch".into(), "fetch_all".into()]), ["fetch"]);
        assert_eq!(detail_constants("Charge::Disputable#x"), ["Charge::Disputable", "Disputable"]);
        let t = "a = Link.fetch(1)\nb = MyLink.fetch(2)\nc = Link.fetch_all\n# Link.fetch\nd = ::Link.fetch(3)\n";
        assert_eq!(constant_call_lines(t, &["Link".into()], "fetch", 10), [1, 5]);
    }

    #[test]
    fn mention_lines() {
        let t = "class A\n  # refund! is gone\n  def refund!\n  end\n  def x\n    p.refund!(1)\n  end\nend\n";
        assert_eq!(mention_line(t, "refund!"), Some(6), "skips the comment and its own def");
        assert_eq!(mention_line("a.refund_all\nb.refund\n", "refund"), Some(2), "whole words only");
        assert_eq!(mention_line("a.refund!\n", "refund"), None, "refund is not refund!");
        assert_eq!(mention_line("x = :total\n", "total"), Some(1), "symbols count");
        assert_eq!(mention_line("@total = 1\n", "total"), None, "ivars do not");
        assert_eq!(mention_line("a.total = 3\n", "total="), Some(1), "setters match their base name");
    }
}
