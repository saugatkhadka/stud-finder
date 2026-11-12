//! Navigation-vs-data classification of call sites (`JsCallSite::link`).
//!
//! The walker keeps a stack of [`Frame`]s describing where in the syntax the
//! current expression sits (a JSX `href`, an argument of `fetch`, the init of
//! `const url = ...`, ...). Expressions that only pass a value through
//! (`a + b`, templates, `?:`, `||`, parentheses, TS casts, `new URL(x)`) push
//! nothing, so the innermost frame is the context the URL value ends up in.
//!
//! [`resolve`] reads the frames up to the nearest function boundary:
//! 1. any data frame (HTTP callee, `form.post`, `<Link method="post">`) -> data;
//! 2. any navigation frame (`href=`, `location.href =`, `router.visit`) -> link;
//! 3. an object property `href`/`url`/`link`/`path` that is plain data (not
//!    inside an argument of an unknown call) -> link;
//! 4. the init of a simple variable -> decided by the variable's uses in the
//!    file (all uses navigation -> link; any data use -> data; else unclassified);
//! 5. anything else -> unclassified, which keeps `link = false`.

use oxc_span::Span;

#[derive(Debug, Clone, Copy)]
pub enum Frame<'a> {
    /// Argument of an HTTP call or of a data-submitting call, with the verb
    /// the context implies (`form.patch(url)` -> PATCH) if any.
    Data(Option<&'static str>),
    /// Value that is navigated to (or loaded as an asset).
    Link,
    /// Value compared against (`pathname === x`, `url.startsWith(x)`):
    /// consumes nothing, so a call site there is a link.
    Compare,
    /// Object property value whose key is `href`/`url`/`link`/`path`.
    NavProp,
    /// Other object property value (key span).
    Prop(Span),
    /// Argument of a call we know nothing about (callee span).
    Arg(Span),
    /// Init / assigned value of a simple variable.
    Var(&'a str),
    /// Assigned to something that is not a variable or `location` (target span).
    Assign(Span),
    Return,
    /// Other JSX attribute (name span).
    JsxAttr(Span),
    JsxChild,
    Export,
    /// Expression statement: the value is discarded.
    Stmt,
    /// Not a use of the value (`if` test, assignment target, `&&` left side,
    /// route-helper arguments).
    Ignore,
    /// Function boundary: frames outside it do not describe values inside.
    Func,
}

#[derive(Debug, Clone, Copy)]
pub enum Class<'a> {
    Data,
    Link,
    Var(&'a str),
    /// Innermost frame, `None` when there is none (top-level expression).
    Unknown(Option<Frame<'a>>),
}

/// Use bits per identifier name.
pub const USE_LINK: u8 = 1;
pub const USE_DATA: u8 = 2;
pub const USE_OTHER: u8 = 4;

pub fn resolve<'a>(stack: &[Frame<'a>]) -> Class<'a> {
    let scope = match stack.iter().rposition(|f| matches!(f, Frame::Func)) {
        Some(i) => &stack[i + 1..],
        None => stack,
    };
    if scope.iter().any(|f| matches!(f, Frame::Data(_))) {
        return Class::Data;
    }
    if scope.iter().any(|f| matches!(f, Frame::Link)) {
        return Class::Link;
    }
    if let Some(i) = scope.iter().rposition(|f| matches!(f, Frame::NavProp)) {
        // `{ href }` in plain data is a nav item; as an argument of an
        // unknown call (`useQuery({ url })`) it may be a request config.
        return match scope[..i].iter().rev().find(|f| matches!(f, Frame::Arg(_))) {
            None => Class::Link,
            Some(arg) => Class::Unknown(Some(*arg)),
        };
    }
    match scope.last() {
        Some(Frame::Compare) => Class::Link,
        Some(Frame::Var(name)) if !scope.iter().any(|f| matches!(f, Frame::Export)) => Class::Var(name),
        Some(Frame::Var(_)) => Class::Unknown(Some(Frame::Export)),
        Some(f) => Class::Unknown(Some(*f)),
        None => Class::Unknown(stack.last().copied()),
    }
}

/// How an identifier reference in this context uses its value (0 = not a
/// use), with the frame that made it an unclassified use.
pub fn use_bits<'a>(stack: &[Frame<'a>]) -> (u8, Option<Frame<'a>>) {
    match resolve(stack) {
        Class::Data => (USE_DATA, None),
        Class::Link => (USE_LINK, None),
        Class::Unknown(Some(Frame::Stmt | Frame::Ignore | Frame::Compare)) => (0, None),
        Class::Var(v) => (USE_OTHER, Some(Frame::Var(v))),
        Class::Unknown(f) => (USE_OTHER, f),
    }
}

/// Short, stable description of an unclassified context, e.g. `arg:useState`,
/// `prop:endpoint`, `jsx:url`, `return`, `var:unused`.
pub fn label(frame: Option<Frame<'_>>, src: &str) -> String {
    let text = |s: Span| -> String {
        let t: String = src.get(s.start as usize..s.end as usize).unwrap_or("?").split_whitespace().collect::<Vec<_>>().join(" ");
        if t.len() > 40 {
            let mut end = 40;
            while !t.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}…", &t[..end])
        } else {
            t
        }
    };
    match frame {
        None => "top-level".into(),
        Some(Frame::Prop(s)) => format!("prop:{}", text(s).trim_matches(['"', '\''])),
        Some(Frame::Arg(s)) => format!("arg:{}", text(s)),
        Some(Frame::Assign(s)) => format!("assign:{}", text(s)),
        Some(Frame::JsxAttr(s)) => format!("jsx:{}", text(s)),
        Some(Frame::Return) => "return".into(),
        Some(Frame::JsxChild) => "jsx-child".into(),
        Some(Frame::Export) => "export".into(),
        Some(Frame::Stmt) => "statement".into(),
        Some(Frame::Ignore) => "test/ignored".into(),
        Some(Frame::Func) => "function".into(),
        Some(Frame::Var(_)) => "var".into(),
        Some(Frame::Data(_) | Frame::Link | Frame::Compare | Frame::NavProp) => "nav".into(),
    }
}

pub fn is_nav_prop(key: &str) -> bool {
    matches!(key, "href" | "url" | "link" | "path")
}

/// JSX attributes whose value is navigated to or loaded as an asset.
pub fn is_nav_attr(name: &str) -> bool {
    matches!(name, "href" | "to" | "src" | "xlinkHref" | "srcSet" | "poster" | "action" | "formAction")
}

/// What the arguments of a call to `callee` (as written, `window.` stripped)
/// are used for. `None` = pass-through (the call returns the URL itself).
pub enum CallUse {
    Data(Option<&'static str>),
    Link,
    Compare,
    PassThrough,
    Unknown,
}

pub fn call_use(callee: &str) -> CallUse {
    let text = ["window.", "globalThis.", "self.", "document."].iter().find_map(|p| callee.strip_prefix(p)).unwrap_or(callee);
    let (recv, last) = match text.rsplit_once('.') {
        Some((r, l)) => (Some(r), l),
        None => (None, text),
    };
    let recv_last = recv.map(|r| r.rsplit('.').next().unwrap_or(r));
    match (recv_last, last) {
        (None, "open") => CallUse::Link,
        (Some("location"), "assign" | "replace") => CallUse::Link,
        (Some(_), "pushState" | "replaceState") => CallUse::Link,
        (Some("history"), "push" | "replace") => CallUse::Link,
        (_, "navigate") => CallUse::Link,
        // Inertia / Turbo / Next-style page visits: the props go to the
        // visited page, not to this file.
        (Some(_), "visit") => CallUse::Link,
        (Some("router" | "Inertia" | "Router"), "get" | "replace" | "push") => CallUse::Link,
        // Inertia `router.post`, `useForm().patch`, `form.submit`, API clients.
        (Some(_), "post" | "put" | "patch" | "delete" | "destroy" | "submit" | "get" | "request") => CallUse::Data(verb_of_method(last)),
        (Some("navigator"), "sendBeacon") => CallUse::Data(Some("POST")),
        (None, "post" | "put" | "patch" | "destroy" | "submit") => CallUse::Data(verb_of_method(last)),
        (Some(_), "startsWith" | "endsWith" | "includes" | "indexOf" | "match" | "test") => CallUse::Compare,
        (None, "encodeURI" | "encodeURIComponent" | "String") => CallUse::PassThrough,
        (Some(_), "toString" | "concat" | "push" | "unshift") => CallUse::PassThrough,
        _ => CallUse::Unknown,
    }
}

/// `x`, `window.location`, `location.href`, `document.location.href`, `top.location`...
pub fn is_location_target(target: &str) -> bool {
    let t: String = target.chars().filter(|c| !c.is_whitespace()).collect();
    t == "location" || t == "location.href" || t.ends_with(".location") || t.ends_with(".location.href")
}

pub fn is_identifier(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$') && !s.starts_with(|c: char| c.is_ascii_digit())
}

/// `post` -> POST, `destroy` -> DELETE; `submit`/`request` -> unknown.
pub fn verb_of_method(m: &str) -> Option<&'static str> {
    Some(match m.to_ascii_lowercase().as_str() {
        "get" => "GET",
        "post" => "POST",
        "put" => "PUT",
        "patch" => "PATCH",
        "delete" | "destroy" => "DELETE",
        _ => return None,
    })
}

/// The verb of the innermost data frame in the current function that names
/// one (`form.post(url, { headers })`: the object names none, `post` does).
pub fn data_verb(stack: &[Frame<'_>]) -> Option<&'static str> {
    for f in stack.iter().rev() {
        match f {
            Frame::Func => return None,
            Frame::Data(Some(v)) => return Some(v),
            _ => {}
        }
    }
    None
}
