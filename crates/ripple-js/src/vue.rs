//! Tolerant `<script>` / `<script setup>` block extraction for `.vue` single-file
//! components. This is not an HTML parser: it scans for `<script` tags outside
//! HTML comments and takes everything up to the next `</script>`.

use oxc_span::SourceType;

pub struct ScriptBlock<'s> {
    pub content: &'s str,
    /// Number of lines before the block's first character (add to oxc's 1-based line).
    pub line_offset: u32,
    pub source_type: SourceType,
    /// `<script src="./x.ts">`: the content lives in another file.
    pub src: Option<String>,
}

pub fn script_blocks(text: &str) -> Vec<ScriptBlock<'_>> {
    let bytes = text.as_bytes();
    let mut blocks = Vec::new();
    let mut i = 0;
    while let Some(rel) = text[i..].find('<') {
        let at = i + rel;
        let rest = &text[at..];
        if rest.starts_with("<!--") {
            i = rest.find("-->").map_or(text.len(), |e| at + e + 3);
            continue;
        }
        let is_script = bytes.len() > at + 7
            && bytes[at..at + 7].eq_ignore_ascii_case(b"<script")
            && matches!(bytes[at + 7], b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/');
        if !is_script {
            i = at + 1;
            continue;
        }
        let Some(tag_end) = tag_end(rest) else { break };
        let attrs = &rest[7..tag_end];
        let content_start = at + tag_end + 1;
        let self_closing = attrs.trim_end().ends_with('/');
        let close = if self_closing { None } else { find_ci(&text[content_start..], "</script") };
        let content_end = close.map_or(content_start, |c| content_start + c);
        let lang = attr(attrs, "lang");
        let source_type = match lang.as_deref() {
            Some("ts") => SourceType::ts(),
            Some("tsx") => SourceType::tsx(),
            Some("jsx") => SourceType::jsx(),
            _ => SourceType::mjs(),
        };
        blocks.push(ScriptBlock {
            content: &text[content_start..content_end],
            line_offset: text[..content_start].bytes().filter(|&b| b == b'\n').count() as u32,
            source_type,
            src: attr(attrs, "src"),
        });
        i = close.map_or(content_end, |c| content_start + c + 8);
    }
    blocks
}

/// Index of the `>` closing the opening tag, skipping quoted attribute values.
fn tag_end(tag: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in tag.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '>') => return Some(i),
            _ => {}
        }
    }
    None
}

fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    hay.as_bytes().windows(needle.len()).position(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

fn attr(attrs: &str, name: &str) -> Option<String> {
    let mut rest = attrs;
    while let Some(i) = rest.find(name) {
        let before_ok = i == 0 || rest.as_bytes()[i - 1].is_ascii_whitespace();
        let after = rest[i + name.len()..].trim_start();
        if before_ok && let Some(v) = after.strip_prefix('=') {
            let v = v.trim_start();
            let (quote, body) = match v.chars().next()? {
                q @ ('"' | '\'') => (Some(q), &v[1..]),
                _ => (None, v),
            };
            let end = match quote {
                Some(q) => body.find(q)?,
                None => body.find(char::is_whitespace).unwrap_or(body.len()),
            };
            return Some(body[..end].to_string());
        }
        rest = &rest[i + name.len()..];
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_both_blocks_and_lines() {
        let sfc = "<template><div/></template>\n<script>\nimport a from './a';\n</script>\n<script setup lang=\"ts\">\nimport b from './b';\n</script>\n";
        let blocks = script_blocks(sfc);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].line_offset, 1);
        assert_eq!(blocks[1].line_offset, 4);
        assert!(blocks[1].source_type.is_typescript());
    }

    #[test]
    fn skips_comments_and_reads_src() {
        let sfc = "<!-- <script>nope()</script> -->\n<script src=\"./x.ts\"></script>";
        let blocks = script_blocks(sfc);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].src.as_deref(), Some("./x.ts"));
    }
}
