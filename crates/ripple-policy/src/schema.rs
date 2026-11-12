//! The JSON Schema of the report (`ripple schema`) and a small validator for
//! the subset of JSON Schema it uses, so tests can check that real output
//! matches the published schema without an external crate.
//!
//! Supported keywords: `type` (string or list), `const`, `enum`,
//! `properties`, `required`, `additionalProperties` (bool or schema),
//! `items`, `minimum`, and `$ref` to `#/$defs/<name>`. Anything else is
//! ignored (descriptions, `$schema`, `title`).

use serde_json::Value;

/// The JSON Schema (draft 2020-12) for [`crate::Report`].
pub const REPORT_SCHEMA: &str = include_str!("report.schema.json");

/// Parsed [`REPORT_SCHEMA`].
pub fn report_schema() -> Value {
    serde_json::from_str(REPORT_SCHEMA).expect("report.schema.json is valid JSON")
}

/// Validates `value` against the report schema. Returns every violation as
/// `json/pointer: problem`; empty means valid.
pub fn validate_report(value: &Value) -> Vec<String> {
    let schema = report_schema();
    validate(&schema, value)
}

/// Validates `value` against `schema` (whose `$defs` resolve `$ref`s).
pub fn validate(schema: &Value, value: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    check(schema, schema, value, "", &mut errors);
    errors
}

fn type_ok(t: &str, v: &Value) -> bool {
    match t {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        "number" => v.is_number(),
        "integer" => v.is_i64() || v.is_u64(),
        _ => true,
    }
}

fn check(root: &Value, s: &Value, v: &Value, at: &str, errors: &mut Vec<String>) {
    let Some(s) = s.as_object() else { return };
    if let Some(r) = s.get("$ref").and_then(Value::as_str) {
        match r.strip_prefix("#/$defs/").and_then(|name| root.get("$defs")?.get(name)) {
            Some(target) => check(root, target, v, at, errors),
            None => errors.push(format!("{at}: unresolvable $ref {r}")),
        }
    }
    let at_or_root = if at.is_empty() { "/" } else { at };
    if let Some(t) = s.get("type") {
        let ok = match t {
            Value::String(t) => type_ok(t, v),
            Value::Array(ts) => ts.iter().filter_map(Value::as_str).any(|t| type_ok(t, v)),
            _ => true,
        };
        if !ok {
            errors.push(format!("{at_or_root}: expected type {t}, got {v}"));
            return;
        }
    }
    if let Some(c) = s.get("const")
        && c != v
    {
        errors.push(format!("{at_or_root}: expected {c}, got {v}"));
    }
    if let Some(Value::Array(options)) = s.get("enum")
        && !options.contains(v)
    {
        errors.push(format!("{at_or_root}: {v} is not one of {}", Value::Array(options.clone())));
    }
    if let (Some(min), Some(n)) = (s.get("minimum").and_then(Value::as_f64), v.as_f64())
        && n < min
    {
        errors.push(format!("{at_or_root}: {n} is below the minimum {min}"));
    }
    if let Value::Object(obj) = v {
        let props = s.get("properties").and_then(Value::as_object);
        if let Some(Value::Array(req)) = s.get("required") {
            for k in req.iter().filter_map(Value::as_str) {
                if !obj.contains_key(k) {
                    errors.push(format!("{at_or_root}: missing required field `{k}`"));
                }
            }
        }
        for (k, child) in obj {
            let path = format!("{at}/{k}");
            match props.and_then(|p| p.get(k)) {
                Some(ps) => check(root, ps, child, &path, errors),
                None => match s.get("additionalProperties") {
                    Some(Value::Bool(false)) => errors.push(format!("{at_or_root}: unexpected field `{k}`")),
                    Some(extra @ Value::Object(_)) => check(root, extra, child, &path, errors),
                    _ => {}
                },
            }
        }
    }
    if let (Value::Array(items), Some(is)) = (v, s.get("items")) {
        for (i, child) in items.iter().enumerate() {
            check(root, is, child, &format!("{at}/{i}"), errors);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_validator_catches_what_it_claims_to() {
        let schema = json!({
            "type": "object", "additionalProperties": false, "required": ["a"],
            "properties": {
                "a": { "$ref": "#/$defs/n" },
                "b": { "type": "array", "items": { "enum": ["x", "y"] } },
                "c": { "type": ["string", "null"] },
                "d": { "const": true }
            },
            "$defs": { "n": { "type": "integer", "minimum": 0 } }
        });
        assert!(validate(&schema, &json!({"a": 1, "b": ["x"], "c": null, "d": true})).is_empty());
        let errs = validate(&schema, &json!({"a": -1, "b": ["z"], "c": 3, "d": false, "e": 1}));
        assert_eq!(errs.len(), 5, "{errs:#?}");
        assert!(errs.iter().any(|e| e.contains("minimum")));
        assert!(errs.iter().any(|e| e.contains("/b/0")));
        assert!(errs.iter().any(|e| e.contains("unexpected field `e`")));
        assert_eq!(validate(&schema, &json!({}))[0], "/: missing required field `a`");
    }

    /// `docs/report.schema.json` is the published copy of the embedded schema.
    #[test]
    fn published_copy_matches_the_embedded_schema() {
        let published = include_str!("../../../docs/report.schema.json");
        assert!(published == REPORT_SCHEMA, "docs/report.schema.json differs: cp crates/ripple-policy/src/report.schema.json docs/");
    }

    #[test]
    fn every_ref_in_the_report_schema_resolves() {
        let schema = report_schema();
        fn refs(v: &Value, out: &mut Vec<String>) {
            match v {
                Value::Object(m) => {
                    if let Some(Value::String(r)) = m.get("$ref") {
                        out.push(r.clone());
                    }
                    m.values().for_each(|x| refs(x, out));
                }
                Value::Array(a) => a.iter().for_each(|x| refs(x, out)),
                _ => {}
            }
        }
        let mut all = Vec::new();
        refs(&schema, &mut all);
        assert!(!all.is_empty());
        for r in all {
            let name = r.strip_prefix("#/$defs/").expect("local ref");
            assert!(schema["$defs"].get(name).is_some(), "{r} does not resolve");
        }
        assert_eq!(schema["properties"]["schema_version"]["const"], json!(crate::SCHEMA_VERSION));
    }
}
