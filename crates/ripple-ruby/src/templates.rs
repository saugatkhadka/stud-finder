//! Pulls the Ruby out of ERB / HAML / Slim templates (pragmatically, line by
//! line; no full template parser) and records the facts views need:
//! constants used, `render` calls, and receiverless method names.

use crate::lex::{self, Tok, Token};
use crate::paths::TemplateLang;
use crate::views::{Render, RenderKind};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TemplateFacts {
    /// Constant paths as written (top-level lookup), with line.
    pub consts: Vec<(String, u32)>,
    pub renders: Vec<Render>,
    /// Receiverless identifiers (candidate helper calls), first line each.
    pub idents: Vec<(String, u32)>,
}

pub fn collect(source: &str, lang: TemplateLang) -> TemplateFacts {
    let fragments = match lang {
        TemplateLang::Erb => erb(source),
        TemplateLang::Haml => haml(source),
        TemplateLang::Slim => slim(source),
        TemplateLang::Ruby => vec![(source.to_string(), 1)],
    };
    let mut facts = TemplateFacts::default();
    let mut seen_idents = std::collections::HashSet::new();
    for (code, line) in fragments {
        let toks = lex::tokenize(&code, line);
        renders(&toks, &mut facts.renders);
        for t in &toks {
            match &t.tok {
                Tok::Const(c) => facts.consts.push((c.clone(), t.line)),
                Tok::Ident {
                    name,
                    after_dot: false,
                } if !is_keyword(name) && seen_idents.insert(name.clone()) => {
                    facts.idents.push((name.clone(), t.line));
                }
                _ => {}
            }
        }
    }
    facts
}

fn is_keyword(w: &str) -> bool {
    matches!(
        w,
        "if" | "unless"
            | "else"
            | "elsif"
            | "end"
            | "do"
            | "while"
            | "until"
            | "case"
            | "when"
            | "then"
            | "and"
            | "or"
            | "not"
            | "nil"
            | "true"
            | "false"
            | "self"
            | "yield"
            | "return"
            | "in"
            | "for"
            | "begin"
            | "rescue"
            | "ensure"
            | "def"
            | "class"
            | "module"
            | "super"
            | "defined?"
            | "next"
            | "break"
            | "render"
            | "lambda"
            | "proc"
    )
}

/// `render "x"`, `render(partial: "x")`, `render layout: "x"`, `render template: "x"`,
/// hash-rocket forms, and jbuilder's `json.partial! "x"`.
fn renders(toks: &[Token], out: &mut Vec<Render>) {
    for (i, t) in toks.iter().enumerate() {
        let is_render = match &t.tok {
            Tok::Ident {
                name,
                after_dot: false,
            } => name == "render",
            Tok::Ident {
                name,
                after_dot: true,
            } => name == "partial!",
            _ => false,
        };
        if !is_render {
            continue;
        }
        let mut j = i + 1;
        if matches!(toks.get(j).map(|t| &t.tok), Some(Tok::Punct('('))) {
            j += 1;
        }
        if let Some(Tok::Str(name)) = toks.get(j).map(|t| &t.tok) {
            out.push(Render {
                kind: RenderKind::Partial,
                name: name.clone(),
                line: t.line,
            });
        }
        // Scan this call's top-level keyword arguments.
        let mut depth = 0i32;
        let mut k = j;
        while let Some(tok) = toks.get(k) {
            match &tok.tok {
                Tok::Punct('(' | '[' | '{') => depth += 1,
                Tok::Punct(')' | ']' | '}') => {
                    depth -= 1;
                    if depth < 0 {
                        break;
                    }
                }
                Tok::Ident { name, .. } if name == "do" || name == "render" => break,
                Tok::Label(key) | Tok::Sym(key) if depth == 0 => {
                    let mut v = k + 1;
                    if matches!(tok.tok, Tok::Sym(_)) {
                        if !matches!(toks.get(v).map(|t| &t.tok), Some(Tok::Punct('>'))) {
                            k += 1;
                            continue;
                        }
                        v += 1;
                    }
                    let kind = match key.as_str() {
                        "partial" => Some(RenderKind::Partial),
                        "layout" => Some(RenderKind::Partial),
                        "template" => Some(RenderKind::Template),
                        _ => None,
                    };
                    if let (Some(kind), Some(Tok::Str(name))) = (kind, toks.get(v).map(|t| &t.tok))
                    {
                        out.push(Render {
                            kind,
                            name: name.clone(),
                            line: tok.line,
                        });
                    }
                }
                _ => {}
            }
            if tok.line > t.line + 3 {
                break;
            }
            k += 1;
        }
    }
}

fn line_at(src: &str, byte: usize) -> u32 {
    1 + src.as_bytes()[..byte]
        .iter()
        .filter(|b| **b == b'\n')
        .count() as u32
}

/// `<% code %>`, `<%= code %>`, `<%- code -%>`; skips `<%# comments %>` and `<%%` literals.
fn erb(src: &str) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(off) = src[pos..].find("<%") {
        let start = pos + off + 2;
        let Some(end_off) = src[start..].find("%>") else {
            break;
        };
        let end = start + end_off;
        pos = end + 2;
        let body = &src[start..end];
        if body.starts_with('%') || body.starts_with('#') {
            continue;
        }
        let code = body.trim_start_matches(['=', '-']).trim_end_matches('-');
        out.push((code.to_string(), line_at(src, start)));
    }
    out
}

/// `#{...}` regions of plain text.
fn interpolations(text: &str, line: u32, out: &mut Vec<(String, u32)>) {
    let mut pos = 0;
    while let Some(off) = text[pos..].find("#{") {
        let start = pos + off + 2;
        let mut depth = 1;
        let mut end = None;
        for (i, b) in text.as_bytes()[start..].iter().enumerate() {
            match b {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(start + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else { break };
        out.push((text[start..end].to_string(), line));
        pos = end + 1;
    }
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Joins continuation lines (code ending in `,` or HAML's `|` multiline marker).
fn continued<'a>(
    first: &str,
    lines: &mut std::iter::Peekable<impl Iterator<Item = (usize, &'a str)>>,
) -> String {
    let mut code = first.trim_end().to_string();
    while code.ends_with(',') || code.ends_with(" |") {
        let Some((_, next)) = lines.next() else { break };
        if code.ends_with(" |") {
            code.truncate(code.len() - 1);
        }
        code.push('\n');
        code.push_str(next.trim_end());
    }
    code.trim_end_matches('|').to_string()
}

/// Takes a balanced `{...}`/`(...)`/`[...]` group at the start of `s`;
/// returns (inner, rest). Unbalanced groups consume the rest of the line.
fn balanced(s: &str) -> (&str, &str) {
    let open = s.as_bytes()[0];
    let close = match open {
        b'{' => b'}',
        b'(' => b')',
        _ => b']',
    };
    let mut depth = 0;
    for (i, b) in s.bytes().enumerate() {
        if b == open {
            depth += 1;
        } else if b == close {
            depth -= 1;
            if depth == 0 {
                return (&s[1..i], &s[i + 1..]);
            }
        }
    }
    (&s[1..], "")
}

fn haml(src: &str) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    let mut skip_deeper_than: Option<usize> = None;
    let mut lines = src.lines().enumerate().peekable();
    while let Some((idx, raw)) = lines.next() {
        let line_no = idx as u32 + 1;
        let t = raw.trim_start();
        if t.is_empty() {
            continue;
        }
        let indent = indent_of(raw);
        if let Some(limit) = skip_deeper_than {
            if indent > limit {
                continue;
            }
            skip_deeper_than = None;
        }
        if t.starts_with("-#")
            || (t.starts_with(':') && t[1..].starts_with(|c: char| c.is_ascii_alphabetic()))
        {
            skip_deeper_than = Some(indent);
            continue;
        }
        if t == "/" || t.starts_with("/[") {
            skip_deeper_than = Some(indent);
            continue;
        }
        if t.starts_with('/') {
            continue;
        }
        if let Some(code) = haml_code_marker(t) {
            out.push((continued(code, &mut lines), line_no));
            continue;
        }
        if t.starts_with("==") {
            interpolations(t, line_no, &mut out);
            continue;
        }
        let is_tag = (t.starts_with('%')
            || t.starts_with('.')
            || (t.starts_with('#') && !t.starts_with("#{")))
            && t[1..].starts_with(|c: char| {
                c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '{'
            });
        if !is_tag {
            interpolations(t, line_no, &mut out);
            continue;
        }
        // Tag head: %div.class#id
        let head_len = t
            .find(|c: char| {
                !(c.is_ascii_alphanumeric() || matches!(c, '%' | '.' | '#' | '-' | '_' | ':'))
            })
            .unwrap_or(t.len());
        let mut rest = &t[head_len..];
        let mut attr_code = String::new();
        while let Some(first) = rest.chars().next() {
            if matches!(first, '{' | '(' | '[') {
                let (inner, after) = balanced(rest);
                if after.is_empty() && !rest.ends_with(['}', ')', ']']) {
                    // Attribute hash spanning lines: take lines until braces balance.
                    let mut joined = rest.to_string();
                    for (_, next) in lines.by_ref() {
                        joined.push('\n');
                        joined.push_str(next);
                        let (_, a) = balanced(&joined);
                        if !a.is_empty() || joined.trim_end().ends_with(['}', ')']) {
                            break;
                        }
                    }
                    attr_code.push_str(&joined[1..]);
                    rest = "";
                    break;
                }
                attr_code.push_str(inner);
                attr_code.push('\n');
                rest = after;
            } else {
                break;
            }
        }
        if !attr_code.is_empty() {
            out.push((attr_code, line_no));
        }
        let rest = rest.trim_start_matches(['<', '>', '/']);
        if let Some(code) = haml_code_marker(rest) {
            out.push((continued(code, &mut lines), line_no));
        } else {
            interpolations(rest, line_no, &mut out);
        }
    }
    out
}

/// Code following `-`, `=`, `~`, `!=`, `&=` (but not `==`, which is interpolated text).
fn haml_code_marker(t: &str) -> Option<&str> {
    if t.starts_with("==") || t.starts_with("-#") {
        return None;
    }
    for m in ["!=", "&=", "=", "-", "~"] {
        if let Some(rest) = t.strip_prefix(m) {
            return Some(rest);
        }
    }
    None
}

fn slim(src: &str) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    let mut skip_deeper_than: Option<usize> = None;
    let mut text_deeper_than: Option<usize> = None;
    let mut lines = src.lines().enumerate().peekable();
    while let Some((idx, raw)) = lines.next() {
        let line_no = idx as u32 + 1;
        let t = raw.trim_start();
        if t.is_empty() {
            continue;
        }
        let indent = indent_of(raw);
        if let Some(limit) = skip_deeper_than {
            if indent > limit {
                continue;
            }
            skip_deeper_than = None;
        }
        if let Some(limit) = text_deeper_than {
            if indent > limit {
                interpolations(t, line_no, &mut out);
                continue;
            }
            text_deeper_than = None;
        }
        if t.starts_with('/') || t.starts_with("doctype") {
            skip_deeper_than = Some(indent);
            continue;
        }
        let first_word_end = t
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(t.len());
        if first_word_end > 0
            && t[first_word_end..].starts_with(':')
            && t[first_word_end..].trim() == ":"
        {
            // `javascript:` etc. filter block
            skip_deeper_than = Some(indent);
            continue;
        }
        if t.starts_with('|') || t.starts_with('\'') {
            interpolations(t, line_no, &mut out);
            text_deeper_than = Some(indent);
            continue;
        }
        if let Some(code) = ["==", "=", "-"].iter().find_map(|m| t.strip_prefix(m)) {
            out.push((continued(code, &mut lines), line_no));
            continue;
        }
        // Tag line: head, then `attr=value` pairs, then `= code` or text.
        let head_len = t
            .find(|c: char| {
                !(c.is_ascii_alphanumeric() || matches!(c, '.' | '#' | '-' | '_' | ':'))
            })
            .unwrap_or(t.len());
        let mut rest = t[head_len..].trim_start();
        loop {
            if rest.starts_with('=') {
                out.push((
                    continued(rest.trim_start_matches(['=', '\'', '<', '>']), &mut lines),
                    line_no,
                ));
                break;
            }
            let name_len = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '@')))
                .unwrap_or(rest.len());
            if name_len > 0 && rest[name_len..].starts_with('=') {
                let value = &rest[name_len + 1..];
                let value = value.strip_prefix('=').unwrap_or(value);
                if value.starts_with(['"', '\'']) {
                    let q = value.as_bytes()[0] as char;
                    let end = value[1..].find(q).map_or(value.len(), |e| e + 2);
                    interpolations(&value[..end], line_no, &mut out);
                    rest = value[end..].trim_start();
                } else if value.starts_with(['(', '[', '{']) {
                    let (inner, after) = balanced(value);
                    out.push((inner.to_string(), line_no));
                    rest = after.trim_start();
                } else {
                    let end = value.find(char::is_whitespace).unwrap_or(value.len());
                    out.push((value[..end].to_string(), line_no));
                    rest = value[end..].trim_start();
                }
                continue;
            }
            interpolations(rest, line_no, &mut out);
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn consts(src: &str, lang: TemplateLang) -> Vec<String> {
        collect(src, lang)
            .consts
            .into_iter()
            .map(|(c, _)| c)
            .collect()
    }

    #[test]
    fn erb_regions() {
        let src = "<h1>Account</h1>\n<%# Comment::Here %>\n<%= link_to Account.count %>\n<%% Literal %>\n<% if Foo::Bar.x? -%>";
        assert_eq!(consts(src, TemplateLang::Erb), vec!["Account", "Foo::Bar"]);
        let f = collect(
            "<%= render 'shared/x' %>\n<%= render partial: \"y\", locals: {a: 1} %>",
            TemplateLang::Erb,
        );
        assert_eq!(f.renders.len(), 2);
        assert_eq!(f.renders[1].line, 2);
    }

    #[test]
    fn haml_regions() {
        let src = "%h1 Status Title\n-# Hidden::Const\n  Nested::Hidden\n%p{ class: Klass.css }= Account.name\n.x #{User.count} people\n:javascript\n  Js::Thing\n= render 'a', b: 1\n- if Foo::Bar.ok?,\n  Baz\n";
        assert_eq!(
            consts(src, TemplateLang::Haml),
            vec!["Klass", "Account", "User", "Foo::Bar", "Baz"]
        );
        let f = collect(src, TemplateLang::Haml);
        assert_eq!(
            f.renders,
            vec![Render {
                kind: RenderKind::Partial,
                name: "a".into(),
                line: 8
            }]
        );
    }

    #[test]
    fn slim_regions() {
        let src = "h1 Plain Text\na href=Foo.path Link\n= Bar.x\n| text #{Baz.y}\np= Qux.z\n";
        assert_eq!(
            consts(src, TemplateLang::Slim),
            vec!["Foo", "Bar", "Baz", "Qux"]
        );
    }
}
