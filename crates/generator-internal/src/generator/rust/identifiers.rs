use crate::generator::naming::{non_empty_str, sanitize_component, to_camel_case};

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

    let component = maybe_component.unwrap_or(fallback_component);

    if prefix.is_empty() {
        component
    } else {
        format!("{prefix}_{component}")
    }
}

/// The camel-case struct prefix of a consumed service's generated
/// request/response types. Every renderer that names these types derives
/// them from this one rule.
pub(crate) fn consumed_service_struct_prefix(service_name: &str) -> String {
    to_camel_case(&prefixed_name("", non_empty_str(service_name), "service"))
}

/// The camel-case prefix of a consumed action's generated types, derived
/// from the producer (contract) and action names (e.g. `UvcCameraEnable`,
/// continued as `UvcCameraEnableActionGoal` for nested goal structs).
pub(crate) fn consumed_action_type_prefix(producer_name: &str, action_name: &str) -> String {
    let node_component = sanitize_component(producer_name);
    let action_component = sanitize_component(action_name);
    let base_component = match (node_component.is_empty(), action_component.is_empty()) {
        (true, true) => "action".to_string(),
        (true, false) => action_component,
        (false, true) => node_component,
        (false, false) => format!("{node_component}_{action_component}"),
    };
    to_camel_case(&base_component)
}

/// The struct name of a nested object field, continuing its parent's
/// prefix.
pub(crate) fn nested_struct_name(struct_prefix: &str, field_name: &str) -> String {
    format!("{struct_prefix}{}", to_camel_case(field_name))
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

    /// The three names below are the contract between the Rust backend's
    /// renderers: they derive the types they emit and the types they
    /// reference from these functions, so the exact strings are pinned
    /// here, empty components included.
    #[test]
    fn the_consumed_action_type_prefix_joins_producer_and_action() {
        assert_eq!(
            consumed_action_type_prefix("uvc_camera", "enable"),
            "UvcCameraEnable"
        );
        assert_eq!(consumed_action_type_prefix("", "enable"), "Enable");
        assert_eq!(consumed_action_type_prefix("uvc_camera", ""), "UvcCamera");
        assert_eq!(consumed_action_type_prefix("", ""), "Action");
    }

    #[test]
    fn the_consumed_service_struct_prefix_falls_back_to_service() {
        assert_eq!(
            consumed_service_struct_prefix("video_stream_info"),
            "VideoStreamInfo"
        );
        assert_eq!(consumed_service_struct_prefix(""), "Service");
    }

    #[test]
    fn a_nested_struct_name_continues_its_parent_prefix() {
        assert_eq!(
            nested_struct_name("UvcCameraEnableActionGoal", "frame_header"),
            "UvcCameraEnableActionGoalFrameHeader"
        );
    }
}
