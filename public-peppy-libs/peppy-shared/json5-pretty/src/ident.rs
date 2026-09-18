//! JSON5 identifier grammar used to decide when an object key can be
//! emitted without quotes.

/// Returns `true` when `s` is a bare JSON5 identifier and may therefore be
/// emitted as an unquoted object key.
pub(crate) fn is_identifier(s: &str) -> bool {
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
