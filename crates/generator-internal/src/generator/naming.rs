//! Shared naming utilities used by both Rust and Python generators.

/// Converts a raw string to a sanitized snake_case identifier component.
///
/// - Non-alphanumeric characters are replaced with underscores
/// - Consecutive separators produce a single underscore
/// - Leading/trailing underscores are stripped
/// - If the result starts with a digit, a leading underscore is prepended
pub(crate) fn sanitize_component(raw: &str) -> String {
    let mut out = String::new();
    let mut last_was_underscore = false;

    for ch in raw.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            last_was_underscore = false;
        } else if !out.is_empty() && !last_was_underscore {
            out.push('_');
            last_was_underscore = true;
        } else if out.is_empty() {
            last_was_underscore = true;
        }
    }

    while out.ends_with('_') {
        out.pop();
    }

    if out.is_empty() {
        return String::new();
    }

    if matches!(out.chars().next(), Some(c) if c.is_ascii_digit()) {
        out.insert(0, '_');
    }

    out
}

/// Returns `None` when the trimmed string is empty, otherwise `Some(value)`.
pub(crate) fn non_empty_str(value: &str) -> Option<&str> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Converts a CamelCase or mixed-case string to snake_case.
pub(crate) fn normalize_snake_case(input: &str) -> String {
    let mut result = String::new();
    let mut chars = input.chars().peekable();
    let mut prev_was_lower_or_digit = false;

    while let Some(ch) = chars.next() {
        if ch.is_ascii_uppercase() {
            let next_is_lower = chars
                .peek()
                .copied()
                .map(|next| next.is_ascii_lowercase())
                .unwrap_or(false);
            if !result.is_empty()
                && (prev_was_lower_or_digit || next_is_lower)
                && !result.ends_with('_')
            {
                result.push('_');
            }
            result.push(ch.to_ascii_lowercase());
            prev_was_lower_or_digit = false;
        } else if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            result.push(ch);
            prev_was_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        } else {
            if !result.ends_with('_') && !result.is_empty() {
                result.push('_');
            }
            prev_was_lower_or_digit = false;
        }
    }

    while result.ends_with('_') {
        result.pop();
    }

    if result.is_empty() {
        return String::from("message");
    }

    if result
        .chars()
        .next()
        .map(|ch| ch.is_ascii_digit())
        .unwrap_or(false)
    {
        result.insert(0, '_');
    }

    result
}

/// Builds a module name from node and name components.
///
/// Returns a combined `node_name` string, or the non-empty component if the other is empty.
pub fn module_name_from_components(node: &str, name: &str) -> String {
    let node_component = sanitize_component(node);
    let name_component = sanitize_component(name);

    match (node_component.is_empty(), name_component.is_empty()) {
        (false, false) => format!("{node_component}_{name_component}"),
        (false, true) => node_component,
        (true, false) => name_component,
        (true, true) => String::new(),
    }
}

/// Converts a snake_case or raw string to CamelCase.
pub(crate) fn to_camel_case(raw: &str) -> String {
    let sanitized = sanitize_component(raw);
    let mut out = String::new();

    for segment in sanitized.split('_').filter(|segment| !segment.is_empty()) {
        let mut chars = segment.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_ascii_uppercase());
            out.push_str(chars.as_str());
        }
    }

    if out.is_empty() {
        out.push_str("Item");
    }

    if !out
        .chars()
        .next()
        .map(|ch| ch.is_ascii_alphabetic())
        .unwrap_or(true)
    {
        out.insert(0, 'T');
    }

    out
}

/// Converts a field name to camelCase for Cap'n Proto field access.
///
/// Splits on non-alphanumeric characters, builds PascalCase, then lowercases
/// the first character. E.g., `frame_id` -> `frameId`, `sample_rate` -> `sampleRate`.
pub fn sanitize_capnp_field_name(input: &str) -> String {
    let mut pascal = String::new();
    for segment in input
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
    {
        let mut chars = segment.chars();
        if let Some(first) = chars.next() {
            pascal.push(first.to_ascii_uppercase());
            for ch in chars {
                pascal.push(ch.to_ascii_lowercase());
            }
        }
    }

    if pascal.is_empty() {
        return "_field".to_string();
    }

    let mut camel = String::with_capacity(pascal.len());
    let mut chars = pascal.chars();
    if let Some(first) = chars.next() {
        camel.push(first.to_ascii_lowercase());
        camel.extend(chars);
    }

    if camel.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
        camel.insert(0, '_');
    }

    if camel.is_empty() {
        "_field".to_string()
    } else {
        camel
    }
}

/// Resolves a Cap'n Proto schema file stem from a raw schema key.
///
/// Sanitizes the key to snake_case, falls back to `"message"` if empty,
/// and appends `"_message"` suffix when not already present.
pub(crate) fn resolve_schema_file_stem(schema_key: &str) -> String {
    let key_component = sanitize_component(schema_key);
    let base_name = if key_component.is_empty() {
        "message".to_string()
    } else {
        key_component
    };

    if base_name.ends_with("_message") {
        base_name
    } else {
        format!("{base_name}_message")
    }
}

/// Generates a unique module name by appending a numeric suffix on collision.
///
/// `sanitize_fn` converts the raw name into a valid module name for the target language.
/// On the first occurrence the name is used as-is; subsequent duplicates get `_1`, `_2`, etc.
pub fn unique_module_name(
    original: &str,
    counts: &mut std::collections::HashMap<String, usize>,
    sanitize_fn: fn(&str) -> String,
) -> String {
    let base = sanitize_fn(original);
    let counter = counts.entry(base.clone()).or_insert(0);
    let name = if *counter == 0 {
        base
    } else {
        format!("{base}_{counter}")
    };
    *counter += 1;
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capnp_field_name_camel_cases() {
        assert_eq!(sanitize_capnp_field_name("frame_id"), "frameId");
        assert_eq!(sanitize_capnp_field_name("sample_rate"), "sampleRate");
        assert_eq!(sanitize_capnp_field_name("encoding"), "encoding");
        assert_eq!(sanitize_capnp_field_name("x"), "x");
        assert_eq!(sanitize_capnp_field_name("return_type"), "returnType");
    }

    #[test]
    fn normalize_snake_case_standard_camel() {
        assert_eq!(normalize_snake_case("camelCase"), "camel_case");
        assert_eq!(normalize_snake_case("SensorData"), "sensor_data");
        assert_eq!(
            normalize_snake_case("GoalResponseMessage"),
            "goal_response_message"
        );
    }

    #[test]
    fn normalize_snake_case_consecutive_uppercase() {
        assert_eq!(normalize_snake_case("HTMLParser"), "html_parser");
        assert_eq!(normalize_snake_case("ABCDef"), "abc_def");
        assert_eq!(normalize_snake_case("HTTPSConnection"), "https_connection");
    }

    #[test]
    fn normalize_snake_case_edge_cases() {
        assert_eq!(normalize_snake_case("ABC"), "abc");
        assert_eq!(normalize_snake_case("A"), "a");
        assert_eq!(normalize_snake_case("alllowercase"), "alllowercase");
        assert_eq!(normalize_snake_case("already_snake"), "already_snake");
    }

    #[test]
    fn unique_module_name_deduplicates() {
        fn identity(s: &str) -> String {
            s.to_string()
        }
        let mut counts = std::collections::HashMap::new();
        assert_eq!(unique_module_name("foo", &mut counts, identity), "foo");
        assert_eq!(unique_module_name("foo", &mut counts, identity), "foo_1");
        assert_eq!(unique_module_name("foo", &mut counts, identity), "foo_2");
    }
}
