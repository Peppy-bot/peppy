//! The rule of the names that documents give to failure kinds and marks:
//! words of lowercase ASCII letters and digits that each start with a
//! letter, joined by one `_`, such as `leader_streaming` or `moves_robot`.
//! So two names never give the same identifier in another case: `kind_2`,
//! `kind__lost` and `kind_` are not names.

/// The longest name, in bytes. A name rides on the wire behind a one-byte
/// length.
pub(crate) const MAX_WORD_NAME_LEN: usize = 64;

/// Why `name` breaks the rule, or `None` when it is a name.
pub(crate) fn word_name_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("it is empty");
    }
    if name.len() > MAX_WORD_NAME_LEN {
        return Some("it is longer than 64 bytes");
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Some("it may hold only lowercase ASCII letters, digits and `_`");
    }
    if !name
        .split('_')
        .all(|word| word.starts_with(|c: char| c.is_ascii_lowercase()))
    {
        return Some(
            "it must be words that each start with a lowercase ASCII letter, joined by one `_`",
        );
    }
    None
}
