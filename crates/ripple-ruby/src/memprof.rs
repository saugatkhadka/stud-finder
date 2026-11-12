//! Memory attribution for `ripple-memprof` (feature `mem-profile` only).
//!
//! Tears the extractor's state down piece by piece and reports how many live
//! heap bytes each piece freed, as measured by the caller's counting
//! allocator. Rubydex's maps cannot be taken out individually (only
//! `declarations_mut` exists), so their share is estimated from counts,
//! capacities and payload sizes; the whole graph is measured exactly.

use crate::RubyExtractor;
use rubydex::model::declaration::Declaration;
use rubydex::model::definitions::Definition;
use rubydex::model::name::NameRef;
use std::collections::HashMap;
use std::mem::size_of;

fn table<K, V, S>(m: &HashMap<K, V, S>) -> usize {
    let cap = m.capacity();
    if cap == 0 {
        return 0;
    }
    let buckets = (cap * 8 / 7).next_power_of_two();
    buckets * (size_of::<(K, V)>() + 1)
}

fn def_payload(d: &Definition) -> usize {
    match d {
        Definition::Class(b) => size_of_val(&**b),
        Definition::SingletonClass(b) => size_of_val(&**b),
        Definition::Module(b) => size_of_val(&**b),
        Definition::Constant(b) => size_of_val(&**b),
        Definition::ConstantAlias(b) => size_of_val(&**b),
        Definition::ConstantVisibility(b) => size_of_val(&**b),
        Definition::MethodVisibility(b) => size_of_val(&**b),
        Definition::Method(b) => size_of_val(&**b),
        Definition::AttrAccessor(b) => size_of_val(&**b),
        Definition::AttrReader(b) => size_of_val(&**b),
        Definition::AttrWriter(b) => size_of_val(&**b),
        Definition::GlobalVariable(b) => size_of_val(&**b),
        Definition::InstanceVariable(b) => size_of_val(&**b),
        Definition::ClassVariable(b) => size_of_val(&**b),
        Definition::MethodAlias(b) => size_of_val(&**b),
        Definition::GlobalVariableAlias(b) => size_of_val(&**b),
    }
}

/// `(label, bytes)`: `measured.*` rows are exact frees; `est.*` rows are
/// estimates inside the Rubydex graph; `count.*` rows are counts.
pub fn breakdown(x: &mut RubyExtractor, live: &dyn Fn() -> isize) -> Vec<(String, isize)> {
    let mut out: Vec<(String, isize)> = Vec::new();
    let Some(mut st) = x.state.take() else { return out };
    let g = &st.graph;
    let mut counts: HashMap<&'static str, (usize, usize)> = HashMap::new();
    let mut comments = 0usize;
    let mut def_payloads = 0usize;
    for d in g.definitions().values() {
        let e = counts.entry(d.kind()).or_default();
        e.0 += 1;
        e.1 += def_payload(d);
        def_payloads += def_payload(d);
        for c in d.comments() {
            comments += c.string().len();
        }
    }
    let mut kinds: Vec<_> = counts.into_iter().collect();
    kinds.sort_by_key(|(_, (_, b))| std::cmp::Reverse(*b));
    for (k, (n, b)) in kinds {
        out.push((format!("count.rubydex.definitions.{k}"), n as isize));
        out.push((format!("est.rubydex.definitions.{k}.payload"), b as isize));
    }
    let names_payload: usize = g
        .names()
        .values()
        .map(|n| match n {
            NameRef::Unresolved(b) => size_of_val(&**b),
            NameRef::Resolved(b) => size_of_val(&**b),
        })
        .sum();
    let strings_payload: usize = g.strings().values().map(|s| s.as_str().len()).sum();
    let docs_vecs: usize = g
        .documents()
        .values()
        .map(|d| d.uri().len() + 8 * (d.definitions().len() + d.method_references().len() + d.constant_references().len()))
        .sum();
    let lines: usize = g.documents().values().map(|d| d.line_index().line_col(d.line_index().len()).line as usize).sum();
    out.push(("count.rubydex.document_lines".into(), lines as isize));
    out.push(("est.rubydex.documents.line_index(4B/line)".into(), (lines * 4) as isize));
    let deps_vecs: usize = g.name_dependents().values().map(|v| v.capacity() * 16).sum();
    let decl_n = g.declarations().len();
    for (label, n) in [
        ("declarations", g.declarations().len()),
        ("definitions", g.definitions().len()),
        ("documents", g.documents().len()),
        ("strings", g.strings().len()),
        ("names", g.names().len()),
        ("constant_references", g.constant_references().len()),
        ("method_references", g.method_references().len()),
        ("name_dependents", g.name_dependents().len()),
    ] {
        out.push((format!("count.rubydex.{label}"), n as isize));
    }
    out.push(("est.rubydex.definitions.table".into(), table(g.definitions()) as isize));
    out.push(("est.rubydex.definitions.boxes".into(), def_payloads as isize));
    out.push(("est.rubydex.definitions.comment_text".into(), (comments) as isize));
    out.push(("est.rubydex.method_references.table".into(), table(g.method_references()) as isize));
    out.push(("est.rubydex.constant_references.table".into(), table(g.constant_references()) as isize));
    out.push(("est.rubydex.names.table+boxes".into(), (table(g.names()) + names_payload) as isize));
    out.push(("est.rubydex.strings.table+text".into(), (table(g.strings()) + strings_payload) as isize));
    out.push(("est.rubydex.documents.table+uri+idvecs(excl. line_index)".into(), (table(g.documents()) + docs_vecs) as isize));
    out.push(("est.rubydex.name_dependents.table+vecs".into(), (table(g.name_dependents()) + deps_vecs) as isize));
    let _ = decl_n;
    let _ = size_of::<Declaration>();

    out.push(("count.ruby.files".into(), st.files.len() as isize));
    out.push(("count.ruby.size_of_FileEntry".into(), size_of::<crate::analyse::FileEntry>() as isize));
    out.push(("count.ruby.refs.len".into(), st.files.values().map(|e| e.refs.len()).sum::<usize>() as isize));
    out.push(("count.ruby.refs.capacity".into(), st.files.values().map(|e| e.refs.capacity()).sum::<usize>() as isize));
    out.push(("count.ruby.size_of_RefSite".into(), size_of::<crate::analyse::RefSite>() as isize));
    out.push(("count.ruby.method_names".into(), st.names.len() as isize));
    let slack = |f: &dyn Fn(&crate::analyse::FileEntry) -> (usize, usize, usize)| -> isize {
        st.files.values().map(|e| {
            let (len, cap, size) = f(e);
            (cap - len) * size
        }).sum::<usize>() as isize
    };
    out.push(("est.ruby.slack.refs".into(), slack(&|e| (e.refs.len(), e.refs.capacity(), 24))));
    out.push(("est.ruby.slack.methods.defs".into(), slack(&|e| (e.methods.defs.len(), e.methods.defs.capacity(), size_of::<crate::methods::Def>()))));
    out.push(("est.ruby.slack.methods.counts".into(), slack(&|e| (e.methods.counts.len(), e.methods.counts.capacity(), 8))));
    out.push(("est.ruby.slack.methods.own".into(), slack(&|e| (e.methods.own.len(), e.methods.own.capacity(), 4))));
    out.push(("est.ruby.slack.methods.inherit_ranges".into(), slack(&|e| (e.methods.inherit_ranges.len(), e.methods.inherit_ranges.capacity(), 8))));
    out.push(("est.ruby.slack.template.idents".into(), slack(&|e| (e.template.idents.len(), e.template.idents.capacity(), 32))));
    out.push(("est.ruby.slack.template.mentions".into(), slack(&|e| (e.template.mentions.len(), e.template.mentions.capacity(), 32))));

    let mut step = |label: &str, f: &mut dyn FnMut()| {
        let before = live();
        f();
        out.push((format!("measured.{label}"), before - live()));
    };
    step("ruby.files.refs", &mut || st.files.values_mut().for_each(|e| e.refs = Vec::new()));
    step("ruby.files.rails_facts", &mut || st.files.values_mut().for_each(|e| e.facts = Default::default()));
    step("ruby.files.template_facts", &mut || st.files.values_mut().for_each(|e| e.template = Default::default()));
    step("ruby.files.method_facts", &mut || st.files.values_mut().for_each(|e| e.methods = Default::default()));
    step("ruby.files.mentions(arc)", &mut || st.files.values_mut().for_each(|e| e.mentions = None));
    step("ruby.files.zeitwerk+warnings", &mut || {
        st.files.values_mut().for_each(|e| {
            e.zeitwerk_keys = Vec::new();
            e.warnings = Vec::new();
        })
    });
    step("ruby.files.map(keys+nodes)", &mut || st.files = Default::default());
    step("ruby.names_interner+inconsistent", &mut || {
        st.names = Default::default();
        st.inconsistent = Default::default();
    });
    step("rubydex.declarations(exact)", &mut || st.graph.declarations_mut().clear());
    step("rubydex.declarations.table", &mut || st.graph.declarations_mut().shrink_to_fit());
    step("rubydex.rest_of_graph(exact)", &mut || st.graph = rubydex::model::graph::Graph::default());
    out
}
