//! Pretty-print a `Serialize` value as JSON5 with unquoted object keys.
//!
//! `serde_json::to_string_pretty` always quotes keys, which produces
//! valid JSON5 but loses the lighter style used elsewhere in this
//! workspace (see `default_repositories.json5`). This helper round-trips
//! through `serde_json::Value` and emits keys without quotes when they
//! match the JSON5 identifier grammar.

use std::fmt::Write;

const INDENT: &str = "  ";

/// Serialize `value` as a pretty-printed JSON5 string. Object keys are
/// emitted unquoted when they are valid JSON5 identifiers; otherwise
/// they fall back to a JSON-style quoted string.
pub fn to_string_pretty<T: ?Sized + serde::Serialize>(
    value: &T,
) -> Result<String, serde_json::Error> {
    let v = serde_json::to_value(value)?;
    let mut out = String::new();
    write_value(&mut out, &v, 0);
    Ok(out)
}

fn write_value(out: &mut String, v: &serde_json::Value, depth: usize) {
    match v {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(b) => write!(out, "{b}").unwrap(),
        serde_json::Value::Number(n) => write!(out, "{n}").unwrap(),
        serde_json::Value::String(s) => write_string(out, s),
        serde_json::Value::Array(arr) => write_array(out, arr, depth),
        serde_json::Value::Object(map) => write_object(out, map, depth),
    }
}

fn write_array(out: &mut String, arr: &[serde_json::Value], depth: usize) {
    if arr.is_empty() {
        out.push_str("[]");
        return;
    }
    out.push('[');
    let len = arr.len();
    for (i, item) in arr.iter().enumerate() {
        out.push('\n');
        push_indent(out, depth + 1);
        write_value(out, item, depth + 1);
        if i + 1 < len {
            out.push(',');
        }
    }
    out.push('\n');
    push_indent(out, depth);
    out.push(']');
}

fn write_object(out: &mut String, map: &serde_json::Map<String, serde_json::Value>, depth: usize) {
    if map.is_empty() {
        out.push_str("{}");
        return;
    }
    out.push('{');
    let len = map.len();
    for (i, (k, v)) in map.iter().enumerate() {
        out.push('\n');
        push_indent(out, depth + 1);
        write_key(out, k);
        out.push_str(": ");
        write_value(out, v, depth + 1);
        if i + 1 < len {
            out.push(',');
        }
    }
    out.push('\n');
    push_indent(out, depth);
    out.push('}');
}

fn write_key(out: &mut String, key: &str) {
    if is_identifier(key) {
        out.push_str(key);
    } else {
        write_string(out, key);
    }
}

fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                write!(out, "\\u{:04x}", c as u32).unwrap();
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn push_indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str(INDENT);
    }
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !is_ident_start(first) {
        return false;
    }
    if !chars.all(is_ident_continue) {
        return false;
    }
    !is_reserved_word(s)
}

fn is_ident_start(c: char) -> bool {
    matches!(c, 'A'..='Z' | 'a'..='z' | '_' | '$')
}

fn is_ident_continue(c: char) -> bool {
    matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '$')
}

fn is_reserved_word(s: &str) -> bool {
    matches!(
        s,
        "true" | "false" | "null" | "Infinity" | "NaN" | "undefined"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unquoted_keys_for_simple_identifiers() {
        let v = json!({ "node_name": "foo", "node_tag": "v1" });
        let s = to_string_pretty(&v).unwrap();
        assert!(s.contains("node_name: \"foo\""), "got: {s}");
        assert!(s.contains("node_tag: \"v1\""), "got: {s}");
    }

    #[test]
    fn quotes_keys_with_special_chars() {
        let v = json!({ "weird-key": 1, "with space": 2, "1starts": 3 });
        let s = to_string_pretty(&v).unwrap();
        assert!(s.contains("\"weird-key\": 1"), "got: {s}");
        assert!(s.contains("\"with space\": 2"), "got: {s}");
        assert!(s.contains("\"1starts\": 3"), "got: {s}");
    }

    #[test]
    fn quotes_reserved_words() {
        let v = json!({ "true": 1, "null": 2 });
        let s = to_string_pretty(&v).unwrap();
        assert!(s.contains("\"true\": 1"), "got: {s}");
        assert!(s.contains("\"null\": 2"), "got: {s}");
    }

    #[test]
    fn arrays_nest_with_indentation() {
        let v = json!([{ "a": [1, 2] }]);
        let s = to_string_pretty(&v).unwrap();
        assert_eq!(s, "[\n  {\n    a: [\n      1,\n      2\n    ]\n  }\n]");
    }

    #[test]
    fn empty_collections() {
        let v = json!({ "arr": [], "obj": {} });
        let s = to_string_pretty(&v).unwrap();
        assert!(s.contains("arr: []"), "got: {s}");
        assert!(s.contains("obj: {}"), "got: {s}");
    }

    #[test]
    fn round_trips_via_serde_json5() {
        let v = json!([
            {
                "node_name": "openarm01_arm",
                "tag": "v1",
                "duplicate": false,
            }
        ]);
        let s = to_string_pretty(&v).unwrap();
        let parsed: serde_json::Value = serde_json5::from_str(&s).unwrap();
        assert_eq!(parsed, v);
    }

    #[test]
    fn escapes_quotes_in_strings() {
        let v = json!({ "msg": "she said \"hi\"" });
        let s = to_string_pretty(&v).unwrap();
        assert!(s.contains(r#"msg: "she said \"hi\"""#), "got: {s}");
    }
}
