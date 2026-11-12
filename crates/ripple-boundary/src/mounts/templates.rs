//! Finds the places where Rails hands control to JavaScript, in ERB/HAML/Slim
//! templates and in Ruby controllers/helpers/components. Text-level scanning
//! is deliberate: the forms are all a helper name followed by literal
//! arguments, and the same scanner must work across three template languages.

use regex::Regex;
use serde::Serialize;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MountKind {
    /// `render inertia: "Pages/Name"`.
    Inertia,
    /// `react_component("Name")` / `data-react-class` / `data: { react_class: }`.
    ReactComponent,
    /// `javascript_pack_tag`, `vite_*_tag`, `javascript_include_tag`, importmap.
    EntryTag,
    /// Canvas `js_bundle :name` -> `ui/features/<name>`.
    JsBundle,
    /// GitLab `pages/<controller>/<action>/index.js` page bundles.
    PageBundle,
}

/// One mount found in a Ruby-side file. `name` is empty when the argument is
/// not a literal (`javascript_pack_tag page`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MountRef {
    pub kind: MountKind,
    /// The helper as written (`react_component`, `vite_typescript_tag`, ...).
    pub helper: String,
    pub name: String,
    pub line: u32,
}

/// Is `path` a file whose text may contain mounts?
pub fn is_candidate(path: &str) -> bool {
    let template = [".erb", ".haml", ".slim"].iter().any(|e| path.ends_with(e));
    let ruby = path.ends_with(".rb") && ["app/controllers/", "app/helpers/", "app/components/", "app/views/"].iter().any(|d| path.contains(d));
    (template || ruby) && !path.starts_with("spec/") && !path.starts_with("test/") && !path.contains("/spec/") && !path.contains("/test/")
}

const KEYWORDS: &[&str] = &["inertia", "react_component", "react-class", "react_class", "_tag", "js_bundle"];

struct Patterns {
    inertia: Regex,
    react: Regex,
    react_class: Regex,
    entry: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        inertia: Regex::new(r#"(?:\binertia(?:_template)?\s*:|:inertia\s*=>|["']inertia["']\s*=>)\s*["']([^"'#{}]+)["']"#).expect("regex"),
        react: Regex::new(r#"\b(react_component(?:_hash)?)\b\s*\(?\s*(?:["']([^"'#{}]+)["']|:(\w+))"#).expect("regex"),
        react_class: Regex::new(r#"(?:\bdata-react-class\s*=\s*|["']?data-react-class["']?\s*=>\s*|\breact_class["']?\s*(?::|=>)\s*)["']([^"'#{}]+)["']"#).expect("regex"),
        entry: Regex::new(r"\b(javascript_pack_tag|javascript_packs_with_chunks_tag|javascript_include_tag|vite_javascript_tag|vite_typescript_tag|vite_client_tag|javascript_importmap_tags|deferred_js_bundle|js_bundle)\b").expect("regex"),
    })
}

/// Byte offset -> 1-based line.
struct Lines(Vec<usize>);

impl Lines {
    fn new(text: &str) -> Self {
        Lines(text.match_indices('\n').map(|(i, _)| i).collect())
    }
    fn line(&self, offset: usize) -> u32 {
        (self.0.partition_point(|&nl| nl < offset) + 1) as u32
    }
}

/// True when the match sits in a comment: Ruby `# ...`, HAML `-#`, ERB `<%#`.
/// (A HAML line starting with `#` is an element id, not a comment.)
fn in_comment(text: &str, offset: usize, ruby: bool) -> bool {
    let start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    let prefix = text[start..offset].trim_start();
    if ruby { prefix.starts_with('#') } else { prefix.starts_with("-#") || prefix.contains("<%#") }
}

/// Leading literal arguments: `"a", 'b', :c` up to the first non-literal or
/// hash argument (`:plugin => x`, `defer: true`).
fn literal_args(rest: &str) -> Vec<String> {
    let mut out = Vec::new();
    let s = rest.trim_start();
    let mut s = s.strip_prefix('(').unwrap_or(s);
    loop {
        s = s.trim_start_matches([' ', '\t']);
        let (arg, after) = if let Some(q) = s.strip_prefix('"').or_else(|| s.strip_prefix('\'')) {
            let quote = s.as_bytes()[0] as char;
            let Some(end) = q.find(quote) else { break };
            let lit = &q[..end];
            if lit.contains("#{") {
                break;
            }
            (lit.to_string(), &q[end + 1..])
        } else if let Some(sym) = s.strip_prefix(':') {
            let end = sym.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(sym.len());
            if end == 0 {
                break;
            }
            (sym[..end].to_string(), &sym[end..])
        } else {
            break;
        };
        let after_trim = after.trim_start();
        if after_trim.starts_with("=>") || after_trim.starts_with(':') && !after_trim.starts_with("::") {
            break; // a hash key, not a positional argument
        }
        out.push(arg);
        match after_trim.strip_prefix(',') {
            Some(next) => s = next,
            None => break,
        }
    }
    out
}

pub fn scan(text: &str, path: &str) -> Vec<MountRef> {
    if !KEYWORDS.iter().any(|k| text.contains(k)) {
        return Vec::new();
    }
    let p = patterns();
    let lines = Lines::new(text);
    let ruby = path.ends_with(".rb");
    let mut out = Vec::new();
    let mut push = |kind, helper: &str, name: String, offset: usize| {
        if in_comment(text, offset, ruby) {
            return;
        }
        out.push(MountRef { kind, helper: helper.to_string(), name, line: lines.line(offset) });
    };
    for c in p.inertia.captures_iter(text) {
        let (Some(m), Some(name)) = (c.get(0), c.get(1)) else { continue };
        push(MountKind::Inertia, "inertia", name.as_str().to_string(), m.start());
    }
    for c in p.react.captures_iter(text) {
        // `react_component "Nav"` or a symbol (`react_component :video`, Mastodon).
        let (Some(m), Some(helper), Some(name)) = (c.get(0), c.get(1), c.get(2).or(c.get(3))) else { continue };
        push(MountKind::ReactComponent, helper.as_str(), name.as_str().to_string(), m.start());
    }
    for c in p.react_class.captures_iter(text) {
        let (Some(m), Some(name)) = (c.get(0), c.get(1)) else { continue };
        push(MountKind::ReactComponent, "data-react-class", name.as_str().to_string(), m.start());
    }
    for m in p.entry.find_iter(text) {
        let helper = m.as_str();
        // `def js_bundle(*args)` is the helper's definition, not a use.
        if text[..m.start()].trim_end().ends_with("def") {
            continue;
        }
        let kind = if helper.ends_with("js_bundle") { MountKind::JsBundle } else { MountKind::EntryTag };
        let args = literal_args(&text[m.end()..]);
        match helper {
            // No entry argument: the Vite client and importmap's default entry.
            "vite_client_tag" => push(kind, helper, String::new(), m.start()),
            "javascript_importmap_tags" => push(kind, helper, args.into_iter().next().unwrap_or_else(|| "application".into()), m.start()),
            _ if args.is_empty() => push(kind, helper, String::new(), m.start()),
            _ => {
                for a in args {
                    push(kind, helper, a, m.start());
                }
            }
        }
    }
    out.sort_by(|a, b| (a.line, a.kind, &a.name).cmp(&(b.line, b.kind, &b.name)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_args_stop_at_hashes() {
        assert_eq!(literal_args(" :gradebook, :plugin => :x"), ["gradebook"]);
        assert_eq!(literal_args("('a.ts', 'b', crossorigin: 'anonymous')"), ["a.ts", "b"]);
        assert_eq!(literal_args(" page"), Vec::<String>::new());
        assert_eq!(literal_args("(is_master ? :a : :b)"), Vec::<String>::new());
        assert_eq!(literal_args(" \"x#{y}\""), Vec::<String>::new());
    }

    #[test]
    fn scans_forms() {
        let erb = "<%= react_component \"Nav\", props: {} %><%= react_component :media_gallery %>\n<%= javascript_pack_tag 'a', 'b' %>\n<%# react_component 'Hidden' %>\n";
        let m = scan(erb, "app/views/x.html.erb");
        let names: Vec<_> = m.iter().map(|m| (m.kind, m.name.as_str(), m.line)).collect();
        assert_eq!(names, [(MountKind::ReactComponent, "Nav", 1), (MountKind::ReactComponent, "media_gallery", 1), (MountKind::EntryTag, "a", 2), (MountKind::EntryTag, "b", 2)]);
        let rb = "  # js_bundle :doc\n  def js_bundle(*args)\n  render inertia: \"Users/Show\", props: {}\n  js_bundle :gradebook\n";
        let m = scan(rb, "app/controllers/x_controller.rb");
        let names: Vec<_> = m.iter().map(|m| (m.kind, m.name.as_str(), m.line)).collect();
        assert_eq!(names, [(MountKind::Inertia, "Users/Show", 3), (MountKind::JsBundle, "gradebook", 4)]);
    }
}
