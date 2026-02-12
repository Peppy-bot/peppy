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

/// Returns true when `ident` is a Rust keyword/reserved word.
pub(crate) fn is_rust_keyword(ident: &str) -> bool {
    matches!(
        ident,
        "as" | "break"
            | "const"
            | "continue"
            | "crate"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "async"
            | "await"
            | "dyn"
            | "abstract"
            | "become"
            | "box"
            | "do"
            | "final"
            | "macro"
            | "override"
            | "priv"
            | "try"
            | "typeof"
            | "unsized"
            | "virtual"
            | "yield"
            | "union"
    )
}

/// Converts a raw string into a Rust identifier-safe component.
///
/// This is like [`sanitize_component`] but appends `_` when the result is a
/// Rust keyword (e.g. `type` -> `type_`).
pub(crate) fn sanitize_rust_identifier(raw: &str) -> String {
    let mut ident = sanitize_component(raw);
    if is_rust_keyword(&ident) {
        ident.push('_');
    }
    ident
}

/// Returns `None` when the trimmed string is empty, otherwise `Some(value)`.
pub(crate) fn non_empty_str(value: &str) -> Option<&str> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Builds a prefixed name from an optional candidate, falling back to `fallback`.
pub(crate) fn prefixed_name(prefix: &str, candidate: Option<&str>, fallback: &str) -> String {
    let fallback_component = match sanitize_rust_identifier(fallback) {
        component if component.is_empty() => "item".to_string(),
        component => component,
    };

    let maybe_component = candidate.and_then(|value| {
        let sanitized = sanitize_rust_identifier(value);
        if sanitized.is_empty() {
            None
        } else {
            Some(sanitized)
        }
    });

    let component = maybe_component
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback_component.clone());

    if prefix.is_empty() {
        component
    } else {
        format!("{prefix}_{component}")
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
            prev_was_lower_or_digit = true;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_keywords_are_escaped() {
        assert_eq!(sanitize_rust_identifier("type"), "type_");
        assert_eq!(sanitize_rust_identifier("match"), "match_");
        assert_eq!(sanitize_rust_identifier("mod"), "mod_");
    }

    #[test]
    fn non_keywords_are_unchanged() {
        assert_eq!(sanitize_rust_identifier("frame_id"), "frame_id");
        assert_eq!(sanitize_rust_identifier("video-stream"), "video_stream");
    }
}
