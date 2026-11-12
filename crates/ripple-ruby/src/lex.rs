//! A tolerant Ruby tokenizer for code fragments pulled out of templates.
//!
//! Template fragments are not valid Ruby on their own (`- if x` has no `end`,
//! `<% form_for do |f| %>` opens a block), so instead of parsing we tokenize
//! and only recognise what we need: constant paths, plain string literals,
//! receiverless identifiers, labels/symbols (for `render partial: "x"`), and
//! a little punctuation. Strings and comments never yield constants; `#{}`
//! interpolation inside double-quoted strings is tokenized as code.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tok {
    /// `Foo`, `Foo::Bar`, `::Foo::Bar` (leading `::` stripped). Never preceded by `.`.
    Const(String),
    /// Contents of a string literal without interpolation.
    Str(String),
    /// A lowercase identifier/method name. `after_dot`: it has an explicit receiver.
    Ident { name: String, after_dot: bool },
    /// `key:` in a hash / keyword argument.
    Label(String),
    /// `:key`.
    Sym(String),
    /// Single punctuation of interest: `( ) , [ ] { }` and `=>` as `>`.
    Punct(char),
    /// `do`/`end`/`if`/... keywords are reported as Ident; anything else is dropped.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub tok: Tok,
    pub line: u32,
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// Tokenizes `code` whose first byte is on `line` (1-based).
pub fn tokenize(code: &str, line: u32) -> Vec<Token> {
    let mut out = Vec::new();
    Lexer {
        s: code.as_bytes(),
        src: code,
        i: 0,
        line,
        out: &mut out,
    }
    .run();
    out
}

struct Lexer<'a, 'o> {
    s: &'a [u8],
    src: &'a str,
    i: usize,
    line: u32,
    out: &'o mut Vec<Token>,
}

impl Lexer<'_, '_> {
    fn peek(&self, k: usize) -> u8 {
        self.s.get(self.i + k).copied().unwrap_or(0)
    }

    fn push(&mut self, tok: Tok, line: u32) {
        self.out.push(Token { tok, line });
    }

    /// Last significant byte before `pos`, skipping spaces/tabs.
    fn prev_byte(&self, pos: usize) -> u8 {
        self.s[..pos]
            .iter()
            .rev()
            .copied()
            .find(|b| *b != b' ' && *b != b'\t')
            .unwrap_or(0)
    }

    fn bump(&mut self) {
        if self.s[self.i] == b'\n' {
            self.line += 1;
        }
        self.i += 1;
    }

    fn run(&mut self) {
        while self.i < self.s.len() {
            let c = self.s[self.i];
            match c {
                b'\n' => self.bump(),
                b'#' => {
                    while self.i < self.s.len() && self.s[self.i] != b'\n' {
                        self.i += 1;
                    }
                }
                b'"' | b'`' => self.string(c, true),
                b'\'' => self.string(b'\'', false),
                b'%' if matches!(self.peek(1), b'w' | b'W' | b'i' | b'I')
                    && !is_ident_byte(self.peek(2))
                    && self.peek(2) != b' ' =>
                {
                    self.i += 2;
                    self.skip_delimited();
                }
                b'%' if matches!(self.peek(1), b'q' | b'Q')
                    && !is_ident_byte(self.peek(2))
                    && self.peek(2) != b' ' =>
                {
                    self.i += 2;
                    self.skip_delimited();
                }
                b'<' if self.peek(1) == b'<'
                    && matches!(self.peek(2), b'~' | b'-' | b'A'..=b'Z') =>
                {
                    self.heredoc()
                }
                b':' if self.peek(1) == b':' => {
                    // Leading `::Foo` (top-level) only when not continuing an expression.
                    let prev = self.prev_byte(self.i);
                    if self.peek(2).is_ascii_uppercase()
                        && !is_ident_byte(prev)
                        && prev != b')'
                        && prev != b']'
                    {
                        self.i += 2;
                        self.constant();
                    } else {
                        self.i += 2;
                    }
                }
                b':' if self.peek(1) == b'"' => {
                    self.i += 1;
                    self.string(b'"', false);
                    if let Some(Token {
                        tok: Tok::Str(s),
                        line,
                    }) = self.out.last().cloned()
                    {
                        self.out.pop();
                        self.push(Tok::Sym(s), line);
                    }
                }
                b':' if is_ident_byte(self.peek(1)) && !self.peek(1).is_ascii_digit() => {
                    let line = self.line;
                    self.i += 1;
                    let start = self.i;
                    while self.i < self.s.len()
                        && (is_ident_byte(self.s[self.i]) || matches!(self.s[self.i], b'?' | b'!'))
                    {
                        self.i += 1;
                    }
                    let name = self.src[start..self.i].to_string();
                    self.push(Tok::Sym(name), line);
                }
                b'@' | b'$' => {
                    self.i += 1;
                    while self.i < self.s.len()
                        && (is_ident_byte(self.s[self.i]) || self.s[self.i] == b'@')
                    {
                        self.i += 1;
                    }
                }
                b'?' if is_ident_byte(self.peek(1))
                    && !is_ident_byte(self.peek(2))
                    && !is_ident_byte(self.prev_byte(self.i)) =>
                {
                    // character literal `?a`
                    self.i += 2;
                }
                b'A'..=b'Z' => {
                    let prev = self.prev_byte(self.i);
                    if prev == b'.' {
                        self.word(true);
                    } else {
                        self.constant();
                    }
                }
                b'0'..=b'9' => {
                    while self.i < self.s.len()
                        && (is_ident_byte(self.s[self.i]) || self.s[self.i] == b'.')
                        && self.s[self.i] != b'\n'
                    {
                        // `1.5` / `1.day`: stop at a dot followed by a letter so `1.day` keeps `day`.
                        if self.s[self.i] == b'.' && !self.peek(1).is_ascii_digit() {
                            break;
                        }
                        self.i += 1;
                    }
                }
                _ if is_ident_byte(c) => {
                    let prev = self.prev_byte(self.i);
                    self.word(prev == b'.');
                }
                b'=' if self.peek(1) == b'>' => {
                    let line = self.line;
                    self.i += 2;
                    self.push(Tok::Punct('>'), line);
                }
                b'(' | b')' | b',' | b'[' | b']' | b'{' | b'}' => {
                    let line = self.line;
                    self.i += 1;
                    self.push(Tok::Punct(c as char), line);
                }
                _ => self.i += 1,
            }
        }
    }

    /// Lowercase identifier, keyword or label.
    fn word(&mut self, after_dot: bool) {
        let line = self.line;
        let start = self.i;
        while self.i < self.s.len() && is_ident_byte(self.s[self.i]) {
            self.i += 1;
        }
        if self.i < self.s.len() && matches!(self.s[self.i], b'?' | b'!') && self.peek(1) != b'=' {
            self.i += 1;
        }
        let name = self.src[start..self.i].to_string();
        if self.peek(0) == b':' && self.peek(1) != b':' && !after_dot {
            self.i += 1;
            self.push(Tok::Label(name), line);
        } else {
            self.push(Tok::Ident { name, after_dot }, line);
        }
    }

    fn constant(&mut self) {
        let line = self.line;
        let mut path = String::new();
        loop {
            let start = self.i;
            while self.i < self.s.len() && is_ident_byte(self.s[self.i]) {
                self.i += 1;
            }
            path.push_str(&self.src[start..self.i]);
            if self.peek(0) == b':' && self.peek(1) == b':' && self.peek(2).is_ascii_uppercase() {
                self.i += 2;
                path.push_str("::");
                continue;
            }
            break;
        }
        // `Foo:` is a hash label, not a constant reference.
        if self.peek(0) == b':' && self.peek(1) != b':' {
            self.i += 1;
            self.push(Tok::Label(path), line);
            return;
        }
        // `Foo(` is a method call (`Integer(x)`); keep it anyway: Kernel
        // conversion methods resolve to no repo file.
        self.push(Tok::Const(path), line);
    }

    fn string(&mut self, quote: u8, interpolate: bool) {
        let line = self.line;
        self.i += 1;
        let mut content = String::new();
        let mut interpolated = false;
        while self.i < self.s.len() {
            let c = self.s[self.i];
            if c == b'\\' {
                if let Some(&n) = self.s.get(self.i + 1) {
                    content.push(n as char);
                }
                self.i += 1;
                if self.i < self.s.len() {
                    self.bump();
                }
                continue;
            }
            if c == quote {
                self.i += 1;
                break;
            }
            if interpolate && c == b'#' && self.peek(1) == b'{' {
                interpolated = true;
                self.i += 2;
                let start = self.i;
                let start_line = self.line;
                let mut depth = 1;
                while self.i < self.s.len() {
                    match self.s[self.i] {
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    self.bump();
                }
                let inner = &self.src[start..self.i];
                self.out.extend(tokenize(inner, start_line));
                if self.i < self.s.len() {
                    self.i += 1;
                }
                continue;
            }
            let ch_len = utf8_len(c);
            content.push_str(self.src.get(self.i..self.i + ch_len).unwrap_or(""));
            for _ in 0..ch_len {
                if self.i < self.s.len() {
                    self.bump();
                }
            }
        }
        if !interpolated {
            self.push(Tok::Str(content), line);
        } else {
            self.push(Tok::Other, line);
        }
    }

    fn skip_delimited(&mut self) {
        let open = self.peek(0);
        let close = match open {
            b'(' => b')',
            b'[' => b']',
            b'{' => b'}',
            b'<' => b'>',
            0 => return,
            other => other,
        };
        self.i += 1;
        let mut depth = 1;
        while self.i < self.s.len() {
            let c = self.s[self.i];
            if c == close {
                depth -= 1;
                if depth == 0 {
                    self.i += 1;
                    return;
                }
            } else if c == open && open != close {
                depth += 1;
            }
            self.bump();
        }
    }

    fn heredoc(&mut self) {
        self.i += 2;
        if matches!(self.peek(0), b'~' | b'-') {
            self.i += 1;
        }
        let quoted = matches!(self.peek(0), b'\'' | b'"');
        if quoted {
            self.i += 1;
        }
        let start = self.i;
        while self.i < self.s.len() && is_ident_byte(self.s[self.i]) {
            self.i += 1;
        }
        let id = self.src[start..self.i].to_string();
        if id.is_empty() {
            return;
        }
        // Skip to the end of the current line, then until the terminator line.
        while self.i < self.s.len() && self.s[self.i] != b'\n' {
            self.i += 1;
        }
        while self.i < self.s.len() {
            self.bump();
            let line_end = self.src[self.i..]
                .find('\n')
                .map_or(self.s.len(), |n| self.i + n);
            if self.src[self.i..line_end].trim() == id {
                self.i = line_end;
                return;
            }
        }
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn consts(code: &str) -> Vec<String> {
        tokenize(code, 1)
            .into_iter()
            .filter_map(|t| match t.tok {
                Tok::Const(c) => Some(c),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn constants() {
        assert_eq!(
            consts("Foo::Bar.baz(Qux, ::Top::Level)"),
            vec!["Foo::Bar", "Qux", "Top::Level"]
        );
        assert_eq!(
            consts("x.Foo; 'Str Const' # Comment Here"),
            Vec::<String>::new()
        );
        assert_eq!(consts("\"a #{User.count} b\""), vec!["User"]);
        assert_eq!(consts("{ Foo: 1, :Bar => 2 }"), Vec::<String>::new());
        assert_eq!(consts("Foo::bar"), vec!["Foo"]);
        assert_eq!(consts("%w[Foo Bar] Baz"), vec!["Baz"]);
    }

    #[test]
    fn render_tokens() {
        let toks: Vec<Tok> = tokenize("render partial: 'x/y', locals: { a: 1 }", 3)
            .into_iter()
            .map(|t| t.tok)
            .collect();
        assert_eq!(
            toks[0],
            Tok::Ident {
                name: "render".into(),
                after_dot: false
            }
        );
        assert_eq!(toks[1], Tok::Label("partial".into()));
        assert_eq!(toks[2], Tok::Str("x/y".into()));
    }
}
