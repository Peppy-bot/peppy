use crate::generator::naming::sanitize_component;

pub(super) fn is_rust_keyword(ident: &str) -> bool {
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
pub(super) fn sanitize_rust_identifier(raw: &str) -> String {
    let mut ident = sanitize_component(raw);
    if is_rust_keyword(&ident) {
        ident.push('_');
    }
    ident
}

/// Builds a prefixed name from an optional candidate, falling back to `fallback`.
pub(super) fn prefixed_name(prefix: &str, candidate: Option<&str>, fallback: &str) -> String {
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

    let component = maybe_component.unwrap_or(fallback_component);

    if prefix.is_empty() {
        component
    } else {
        format!("{prefix}_{component}")
    }
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
