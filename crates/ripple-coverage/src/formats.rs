//! Report parsers. Each produces, per report path as written in the report,
//! two line bitsets (coverable, covered). JSON is parsed with streaming serde
//! visitors straight into the bitsets: no `serde_json::Value` tree.

use anyhow::{Result, bail};
use serde::de::{self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;

/// Highest line number accepted. Larger ones are malformed (and would cost
/// memory): skipped.
pub(crate) const MAX_LINE: u32 = 1 << 20;

/// Per-line bitsets, bit `i` = line `i + 1`. `hit` ⊆ `cov` after [`LineBits::fix`].
#[derive(Debug, Default, Clone)]
pub(crate) struct LineBits {
    pub cov: Vec<u64>,
    pub hit: Vec<u64>,
}

impl LineBits {
    /// Marks `line` coverable (and covered if `covered`). False if out of range.
    #[inline]
    pub(crate) fn set(&mut self, line: u32, covered: bool) -> bool {
        if line == 0 || line > MAX_LINE {
            return false;
        }
        let i = (line - 1) as usize;
        let w = i / 64;
        if w >= self.cov.len() {
            self.cov.resize(w + 1, 0);
            self.hit.resize(w + 1, 0);
        }
        let b = 1u64 << (i % 64);
        self.cov[w] |= b;
        if covered {
            self.hit[w] |= b;
        }
        true
    }

    /// Merge another measurement of the same file: a line is coverable if any
    /// report says so, covered if any run hit it (= summed hits > 0).
    pub(crate) fn or(&mut self, o: &LineBits) {
        if o.cov.len() > self.cov.len() {
            self.cov.resize(o.cov.len(), 0);
            self.hit.resize(o.cov.len(), 0);
        }
        for (i, (&c, &h)) in o.cov.iter().zip(&o.hit).enumerate() {
            self.cov[i] |= c;
            self.hit[i] |= h;
        }
    }

    pub(crate) fn fix(&mut self) {
        for (h, &c) in self.hit.iter_mut().zip(&self.cov) {
            *h &= c;
        }
        while self.cov.last() == Some(&0) {
            self.cov.pop();
            self.hit.pop();
        }
    }
}

/// One parsed report, keyed by the path as the report wrote it.
#[derive(Debug, Default)]
pub(crate) struct Parsed {
    pub entries: HashMap<String, LineBits>,
    /// Entries or records skipped as malformed.
    pub malformed: u32,
    /// Newest suite timestamp (SimpleCov), unix seconds.
    pub timestamp: Option<i64>,
}

impl Parsed {
    fn slot<'e>(entries: &'e mut HashMap<String, LineBits>, key: Cow<'_, str>) -> &'e mut LineBits {
        // Avoid allocating for a key seen before (a second suite).
        if entries.contains_key(key.as_ref()) {
            return entries.get_mut(key.as_ref()).unwrap();
        }
        entries.entry(key.into_owned()).or_default()
    }
}

// ---------------------------------------------------------------- LCOV

/// LCOV tracefile: `SF:<path>`, `DA:<line>,<hits>[,<checksum>]`,
/// `end_of_record`. Everything else (`TN`, `FN*`, `BR*`, `LF`, `LH`) is
/// ignored: line totals are recomputed from `DA`.
pub(crate) fn parse_lcov(bytes: &[u8]) -> Result<Parsed> {
    let mut p = Parsed::default();
    let mut cur: Option<String> = None;
    let mut seen_sf = false;
    for line in bytes.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(rest) = line.strip_prefix(b"SF:") {
            seen_sf = true;
            let path = String::from_utf8_lossy(rest).trim().to_string();
            if path.is_empty() {
                p.malformed += 1;
                cur = None;
            } else {
                Parsed::slot(&mut p.entries, Cow::Borrowed(&path));
                cur = Some(path);
            }
        } else if let Some(rest) = line.strip_prefix(b"DA:") {
            let Some(path) = &cur else {
                p.malformed += 1;
                continue;
            };
            let mut it = rest.split(|&b| b == b',');
            let ln = it.next().and_then(parse_u32);
            let hits = it
                .next()
                .map(|h| h.trim_ascii())
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| h.parse::<f64>().ok());
            match (ln, hits) {
                (Some(ln), Some(h))
                    if p.entries.get_mut(path.as_str()).unwrap().set(ln, h > 0.0) => {}
                _ => p.malformed += 1,
            }
        } else if line.trim_ascii() == b"end_of_record" {
            cur = None;
        }
    }
    if !seen_sf && !bytes.trim_ascii().is_empty() {
        bail!("not an LCOV tracefile: no SF: record");
    }
    Ok(p)
}

fn parse_u32(b: &[u8]) -> Option<u32> {
    std::str::from_utf8(b.trim_ascii()).ok()?.parse().ok()
}

// ---------------------------------------------------------------- JSON helpers

/// A string that borrows from the input when it has no escapes.
struct Str<'de>(Cow<'de, str>);

impl<'de> serde::Deserialize<'de> for Str<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Str<'de>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string")
            }
            fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
                Ok(Str(Cow::Borrowed(v)))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(Str(Cow::Owned(v.to_string())))
            }
        }
        d.deserialize_str(V)
    }
}

/// Any JSON value, reduced to "a number" or "something else".
enum Val {
    Num(f64),
    Other,
}

impl<'de> serde::Deserialize<'de> for Val {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Val;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any value")
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Val, E> {
                Ok(Val::Num(v as f64))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Val, E> {
                Ok(Val::Num(v as f64))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Val, E> {
                Ok(Val::Num(v))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Val, E> {
                Ok(Val::Other)
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<Val, E> {
                Ok(Val::Other)
            }
            fn visit_unit<E: de::Error>(self) -> Result<Val, E> {
                Ok(Val::Other)
            }
            fn visit_none<E: de::Error>(self) -> Result<Val, E> {
                Ok(Val::Other)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Val, A::Error> {
                while a.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Val::Other)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Val, A::Error> {
                while a.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(Val::Other)
            }
        }
        d.deserialize_any(V)
    }
}

// ---------------------------------------------------------------- SimpleCov

/// `.resultset.json`: `{suite: {"coverage": {path: LINES | {"lines": LINES, ...}}, "timestamp": N}}`
/// with `LINES = [hits | null | "ignored", ...]` (index 0 = line 1).
pub(crate) fn parse_simplecov(bytes: &[u8]) -> Result<Parsed> {
    let mut p = Parsed::default();
    let mut de = serde_json::Deserializer::from_slice(bytes);
    de.deserialize_map(Top(&mut p))?;
    de.end()?;
    Ok(p)
}

struct Top<'p>(&'p mut Parsed);

impl<'de> Visitor<'de> for Top<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a SimpleCov resultset object")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<(), A::Error> {
        while m.next_key::<IgnoredAny>()?.is_some() {
            m.next_value_seed(Suite(&mut *self.0))?;
        }
        Ok(())
    }
}

struct Suite<'p>(&'p mut Parsed);

impl<'de> DeserializeSeed<'de> for Suite<'_> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for Suite<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a SimpleCov suite object")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<(), A::Error> {
        while let Some(k) = m.next_key::<Str>()? {
            match k.0.as_ref() {
                "coverage" => m.next_value_seed(Files(&mut *self.0))?,
                "timestamp" => {
                    if let Val::Num(t) = m.next_value::<Val>()? {
                        let t = t as i64;
                        self.0.timestamp = Some(self.0.timestamp.map_or(t, |o| o.max(t)));
                    }
                }
                _ => {
                    m.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(())
    }
}

struct Files<'p>(&'p mut Parsed);

impl<'de> DeserializeSeed<'de> for Files<'_> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for Files<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a map of file -> coverage")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<(), A::Error> {
        let Parsed {
            entries, malformed, ..
        } = &mut *self.0;
        while let Some(k) = m.next_key::<Str>()? {
            let before = *malformed;
            let slot = Parsed::slot(entries, k.0.clone());
            m.next_value_seed(FileCov {
                slot,
                malformed: &mut *malformed,
            })?;
            // A malformed entry that added nothing is not an entry (it must
            // not read as "in the report, nothing coverable").
            if *malformed > before && entries.get(k.0.as_ref()).is_some_and(|b| b.cov.is_empty()) {
                entries.remove(k.0.as_ref());
            }
        }
        Ok(())
    }
}

/// Either the legacy `[..]` line array or `{"lines": [..], "branches": ..}`.
struct FileCov<'p> {
    slot: &'p mut LineBits,
    malformed: &'p mut u32,
}

impl<'de> DeserializeSeed<'de> for FileCov<'_> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for FileCov<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a line array or an object with \"lines\"")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, a: A) -> Result<(), A::Error> {
        Lines {
            slot: self.slot,
            malformed: self.malformed,
        }
        .visit_seq(a)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<(), A::Error> {
        while let Some(k) = m.next_key::<Str>()? {
            if k.0 == "lines" {
                m.next_value_seed(Lines {
                    slot: &mut *self.slot,
                    malformed: &mut *self.malformed,
                })?;
            } else {
                m.next_value::<IgnoredAny>()?;
            }
        }
        Ok(())
    }
    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        *self.malformed += 1;
        Ok(())
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
        self.visit_unit()
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> {
        self.visit_unit()
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
        self.visit_unit()
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<(), E> {
        self.visit_unit()
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<(), E> {
        self.visit_unit()
    }
}

struct Lines<'p> {
    slot: &'p mut LineBits,
    malformed: &'p mut u32,
}

impl<'de> DeserializeSeed<'de> for Lines<'_> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Lines<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a line array")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<(), A::Error> {
        let mut line: u32 = 0;
        let mut too_long = false;
        while let Some(v) = a.next_element::<Val>()? {
            line = line.saturating_add(1);
            // null (not coverable), "ignored" (nocov) and junk: not coverable.
            if let Val::Num(h) = v
                && !self.slot.set(line, h > 0.0)
            {
                too_long = true;
            }
        }
        if too_long {
            *self.malformed += 1;
        }
        Ok(())
    }
    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        *self.malformed += 1;
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<(), A::Error> {
        while m.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        self.visit_unit()
    }
}

// ---------------------------------------------------------------- Istanbul

#[derive(serde::Deserialize)]
struct Pos {
    line: u32,
}

#[derive(serde::Deserialize)]
struct Loc {
    start: Pos,
}

#[derive(serde::Deserialize)]
struct IstEntry {
    #[serde(default, rename = "statementMap")]
    statement_map: HashMap<String, Loc>,
    #[serde(default)]
    s: HashMap<String, f64>,
}

/// `coverage-final.json`: `{path: {"statementMap": {id: {"start": {"line": L}, ..}}, "s": {id: hits}, ..}}`.
/// Line coverage as Istanbul's own LCOV reporter derives it: a line is
/// coverable if a statement starts on it, covered if any such statement ran.
/// Each entry is parsed on its own; a malformed one is skipped.
pub(crate) fn parse_istanbul(bytes: &[u8]) -> Result<Parsed> {
    let raw: HashMap<Str, &serde_json::value::RawValue> = serde_json::from_slice(bytes)?;
    let mut p = Parsed::default();
    for (k, v) in raw {
        match serde_json::from_str::<IstEntry>(v.get()) {
            Ok(e) => {
                let slot = Parsed::slot(&mut p.entries, k.0);
                let mut bad = false;
                for (id, loc) in &e.statement_map {
                    let hits = e.s.get(id).copied().unwrap_or(0.0);
                    bad |= !slot.set(loc.start.line, hits > 0.0);
                }
                p.malformed += u32::from(bad);
            }
            Err(_) => p.malformed += 1,
        }
    }
    Ok(p)
}

impl std::hash::Hash for Str<'_> {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        self.0.hash(h)
    }
}
impl PartialEq for Str<'_> {
    fn eq(&self, o: &Self) -> bool {
        self.0 == o.0
    }
}
impl Eq for Str<'_> {}
