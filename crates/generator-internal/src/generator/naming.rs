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

/// Sanitizes an `iface_tag` for use as a generated module segment and as a
/// Zenoh wire-path segment. Currently this is just a hyphen→underscore
/// conversion; the rest of [`sanitize_component`]'s heuristics would lose
/// information (e.g. case) without buying us anything, since interface tags
/// are already constrained to letters/digits/underscores/hyphens.
pub(crate) fn sanitize_iface_tag(raw: &str) -> String {
    raw.replace('-', "_")
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

/// Builds a sanitized module name from node and name components.
///
/// Returns a combined `node_name` string, or the non-empty component if the other is empty.
pub(crate) fn module_name_from_components(node: &str, name: &str) -> String {
    let node_component = sanitize_component(node);
    let name_component = sanitize_component(name);

    match (node_component.is_empty(), name_component.is_empty()) {
        (false, false) => format!("{node_component}_{name_component}"),
        (false, true) => node_component,
        (true, false) => name_component,
        (true, true) => String::new(),
    }
}

/// Builds a raw (unsanitized) label from node and name components.
///
/// Unlike [`module_name_from_components`], this preserves the original characters
/// so that names which differ only in separator style (e.g. `foo-bar` vs `foo_bar`)
/// remain distinct.  Used as the grouping key for artifacts before sanitization.
pub(crate) fn raw_module_label(node: &str, name: &str) -> String {
    let node = node.trim();
    let name = name.trim();

    match (node.is_empty(), name.is_empty()) {
        (false, false) => format!("{node}::{name}"),
        (false, true) => node.to_string(),
        (true, false) => name.to_string(),
        (true, true) => String::new(),
    }
}

/// Sanitizes a display name for use in generated comments and doc attributes.
///
/// Trims whitespace, replaces control characters with spaces, and collapses
/// consecutive whitespace into single spaces.
pub(crate) fn sanitize_node_display_name(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Returns the intermediate field name used when generating a struct for
/// array-of-objects items.  E.g. `"frames"` → `"frames_item"`.
pub(crate) fn array_item_field_name(field_name: &str) -> String {
    format!("{field_name}_item")
}

/// Returns the full CamelCase type/class name for an array-of-objects item struct.
/// E.g. `("Message", "frames")` → `"MessageFramesItem"`.
pub(crate) fn array_item_type_name(struct_prefix: &str, field_name: &str) -> String {
    format!(
        "{struct_prefix}{}",
        to_camel_case(&array_item_field_name(field_name))
    )
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
pub(crate) fn sanitize_capnp_field_name(input: &str) -> String {
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

/// Resolved schema file stem with both the base name and the full file stem.
pub(crate) struct ResolvedSchemaFileStem {
    /// The sanitized base name (without the `_message` suffix).
    pub base_name: String,
    /// The full file stem (always ends with `_message`).
    pub file_stem: String,
}

/// Resolves a Cap'n Proto schema file stem from a raw schema key.
///
/// Sanitizes the key to snake_case, falls back to `"message"` if empty,
/// and appends `"_message"` suffix when not already present.
pub(crate) fn resolve_schema_file_stem(schema_key: &str) -> ResolvedSchemaFileStem {
    let key_component = sanitize_component(schema_key);
    let base_name = if key_component.is_empty() {
        "message".to_string()
    } else {
        key_component
    };

    if base_name.ends_with("_message") {
        let stem_base = base_name.strip_suffix("_message").unwrap().to_string();
        ResolvedSchemaFileStem {
            base_name: stem_base,
            file_stem: base_name,
        }
    } else {
        let file_stem = format!("{base_name}_message");
        ResolvedSchemaFileStem {
            base_name,
            file_stem,
        }
    }
}

/// Sanitizes `raw` via [`sanitize_component`]; if the result is empty,
/// substitutes `fallback`. Used by the shared schema-key helpers below so
/// degenerate inputs (empty producer or entity name) produce a stable,
/// non-empty schema_key in both the Rust and Python generators.
pub(crate) fn sanitize_or(raw: &str, fallback: &str) -> String {
    let component = sanitize_component(raw);
    if component.is_empty() {
        fallback.to_string()
    } else {
        component
    }
}

/// Schema key for the cap'n proto message backing a consumed topic.
///
/// Same output in both generators so a Rust node and a Python node that
/// consume identical topics produce matching capnp file_stems. The producer
/// node is intentionally NOT part of the key: two consumers of the same
/// topic name from different producers share one capnp file, and the
/// per-language `register_schema` collision check makes the dedup safe by
/// reusing the first registration's struct identity for any later one.
pub(crate) fn consumed_topic_schema_key(producer_name: &str, topic_name: &str) -> String {
    let topic = sanitize_component(topic_name);
    let producer = sanitize_component(producer_name);
    if !topic.is_empty() {
        format!("on_next_{topic}")
    } else if !producer.is_empty() {
        format!("on_next_{producer}")
    } else {
        String::from("on_next_topic")
    }
}

/// Request-message schema key for a consumed service. Includes the producer
/// node so two consumers of a same-named service from different producers
/// each get their own capnp file (and never silently share a deduplicated
/// schema).
///
/// Shared between the Rust and Python generators.
pub(crate) fn consumed_service_request_schema_key(
    producer_name: &str,
    service_name: &str,
) -> String {
    let producer = sanitize_or(producer_name, "node");
    let service = sanitize_or(service_name, "service");
    format!("poll_{producer}_{service}")
}

/// Response-message schema key for a consumed service. Always
/// `{request_schema_key}_response` so the pair stays consistent.
pub(crate) fn consumed_service_response_schema_key(
    producer_name: &str,
    service_name: &str,
) -> String {
    format!(
        "{}_response",
        consumed_service_request_schema_key(producer_name, service_name)
    )
}

/// The cap'n proto schema keys a consumed action needs (one per wire message
/// type that carries a user-defined payload). Bundled so callers don't have to
/// re-derive the `(producer, action)` tuple for each kind. The cancel reply has
/// no per-action schema; it is the framework cancel-ack decoded by peppylib.
pub(crate) struct ConsumedActionSchemaKeys {
    pub goal_request: String,
    pub goal_response: String,
    pub feedback: String,
    pub result_response: String,
}

/// Schema keys for every cap'n proto message a consumed action produces.
/// Includes the producer node so cross-producer same-action-name doesn't
/// collide on the deduplicated file_stem.
///
/// Shared between the Rust and Python generators so the same input produces
/// matching file_stems in both languages.
pub(crate) fn consumed_action_schema_keys(
    producer_name: &str,
    action_name: &str,
) -> ConsumedActionSchemaKeys {
    let producer = sanitize_or(producer_name, "node");
    let action = sanitize_or(action_name, "action");
    let prefix = format!("{producer}_{action}");
    ConsumedActionSchemaKeys {
        goal_request: format!("{prefix}_fire_goal"),
        goal_response: format!("{prefix}_fire_goal_response"),
        feedback: format!("{prefix}_feedback"),
        result_response: format!("{prefix}_get_result_response"),
    }
}

/// Generates a unique module name by appending a numeric suffix on collision.
///
/// `sanitize_fn` converts the raw name into a valid module name for the target language.
/// On the first occurrence the name is used as-is; subsequent duplicates get `_1`, `_2`, etc.
pub(crate) fn unique_module_name(
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
    fn array_item_field_name_appends_item_suffix() {
        assert_eq!(array_item_field_name("frames"), "frames_item");
        assert_eq!(array_item_field_name("data"), "data_item");
    }

    #[test]
    fn array_item_type_name_produces_camel_case() {
        assert_eq!(
            array_item_type_name("Message", "frames"),
            "MessageFramesItem"
        );
        assert_eq!(
            array_item_type_name("MessageHeader", "points"),
            "MessageHeaderPointsItem"
        );
    }

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

    #[test]
    fn resolve_schema_file_stem_appends_message_suffix() {
        let resolved = resolve_schema_file_stem("my_topic");
        assert_eq!(resolved.base_name, "my_topic");
        assert_eq!(resolved.file_stem, "my_topic_message");
    }

    #[test]
    fn resolve_schema_file_stem_keeps_existing_message_suffix() {
        let resolved = resolve_schema_file_stem("goal_response_message");
        assert_eq!(resolved.base_name, "goal_response");
        assert_eq!(resolved.file_stem, "goal_response_message");
    }

    #[test]
    fn resolve_schema_file_stem_empty_key_falls_back_to_message() {
        let resolved = resolve_schema_file_stem("");
        assert_eq!(resolved.base_name, "message");
        assert_eq!(resolved.file_stem, "message_message");
    }

    #[test]
    fn resolve_schema_file_stem_sanitizes_special_characters() {
        let resolved = resolve_schema_file_stem("My-Topic");
        assert_eq!(resolved.base_name, "my_topic");
        assert_eq!(resolved.file_stem, "my_topic_message");
    }

    #[test]
    fn sanitize_node_display_name_replaces_control_chars() {
        assert_eq!(sanitize_node_display_name("hello\nworld"), "hello world");
        assert_eq!(sanitize_node_display_name("foo\r\nbar"), "foo bar");
        assert_eq!(sanitize_node_display_name("a\tb\0c"), "a b c");
    }

    #[test]
    fn sanitize_node_display_name_trims_and_collapses_whitespace() {
        assert_eq!(sanitize_node_display_name("  spaces  "), "spaces");
        assert_eq!(sanitize_node_display_name("  a   b  "), "a b");
    }

    #[test]
    fn sanitize_node_display_name_passthrough() {
        assert_eq!(sanitize_node_display_name("normal_name"), "normal_name");
        assert_eq!(sanitize_node_display_name(""), "");
    }

    #[test]
    fn raw_module_label_uses_unambiguous_separator() {
        assert_eq!(raw_module_label("sensor", "temp"), "sensor::temp");
        assert_ne!(
            raw_module_label("foo_bar", "baz"),
            raw_module_label("foo", "bar_baz"),
            "different (node, name) pairs must produce different labels"
        );
    }

    #[test]
    fn raw_module_label_handles_empty_components() {
        assert_eq!(raw_module_label("", "name"), "name");
        assert_eq!(raw_module_label("node", ""), "node");
        assert_eq!(raw_module_label("", ""), "");
    }

    #[test]
    fn raw_module_label_trims_whitespace() {
        assert_eq!(raw_module_label("  node  ", "  name  "), "node::name");
    }
}
