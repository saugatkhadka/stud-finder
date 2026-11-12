//! Shows how Rubydex records constant references for one Ruby file: every
//! reference with its name, parent scope, byte span and source text.
//!
//! `cargo run -p ripple-ruby --example refs -- <file.rb>`

use rubydex::indexing::{IndexerBackend, LanguageId, build_local_graph};
use rubydex::model::name::ParentScope;

fn main() -> anyhow::Result<()> {
    let Some(path) = std::env::args().nth(1) else {
        anyhow::bail!("usage: refs <file.rb>")
    };
    let source = std::fs::read_to_string(&path)?;
    let lg = build_local_graph(
        path.clone().into(),
        &source,
        &LanguageId::Ruby,
        IndexerBackend::RubyIndexer,
    );
    let name_of = |id| {
        lg.names()
            .get(id)
            .and_then(|n| lg.strings().get(n.str()))
            .map(|s| s.as_str().to_string())
            .unwrap_or_default()
    };
    let mut refs: Vec<_> = lg.constant_references().values().collect();
    refs.sort_by_key(|r| (r.offset().start(), r.offset().end()));
    for r in refs {
        let (s, e) = (r.offset().start() as usize, r.offset().end() as usize);
        let parent = match lg.names().get(r.name_id()).map(|n| n.parent_scope()) {
            Some(ParentScope::Some(p)) => format!("parent={}", name_of(p)),
            Some(ParentScope::TopLevel) => "parent=::".into(),
            Some(ParentScope::Attached(p)) => format!("attached={}", name_of(p)),
            _ => String::new(),
        };
        let after: String = source[e..].chars().take(3).collect();
        println!(
            "{s:>6}..{e:<6} {:<30} {:<24} text={:?} next={:?}",
            name_of(r.name_id()),
            parent,
            &source[s..e],
            after
        );
    }
    Ok(())
}
