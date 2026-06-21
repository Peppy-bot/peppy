//! Comment-preserving completion of a user's `peppy_config.json5`.
//!
//! [`complete_config_content`] splices every missing known section or nested
//! field into the file content, copying the snippet (explanatory comments
//! included) from the bundled template, so the file on disk always lists every
//! available knob. Everything the user wrote survives byte-for-byte: their
//! values, their comments, their formatting, and any unknown keys.
//!
//! Rewriting the file through serde would destroy comments, and no
//! comment-preserving JSON5 editor crate exists, so this works directly on the
//! text: a minimal JSON5-aware scanner (strings, comments, and brace nesting,
//! nothing more) locates the insertion points, and the snippets are inserted
//! before the relevant closing brace. Before anything is written, the caller
//! gates the result through [`verify_completion`], so a splicing bug cannot
//! drop or alter any value the user wrote, cannot change what the file parses
//! to, and cannot splice again on the next start; a bad splice is discarded
//! with a warning and the user's file stays untouched.

use serde_json::Value;
use std::collections::HashMap;

use super::{
    API_FIELD_SNIPPET, DAEMON_GRACE_FIELD_SNIPPET, HIGH_THROUGHPUT_BUFFER_FIELD_SNIPPET,
    LIFECYCLE_SECTION_SNIPPET, MODE_SECTION_SNIPPET, PEER_SECTION_SNIPPET,
    RESOURCE_SERVERS_SECTION_SNIPPET, SHUTDOWN_GRACE_FIELD_SNIPPET, STANDARD_BUFFER_FIELD_SNIPPET,
};

/// A nested field of a top-level section, with the template snippet to splice
/// into the section's block when the field is absent.
struct FieldSpec {
    key: &'static str,
    snippet: &'static str,
}

/// A top-level entry of the bundled template: the snippet to splice into the
/// root object when the whole entry is absent, and the nested fields to splice
/// individually when the entry exists but is incomplete.
struct SectionSpec {
    key: &'static str,
    snippet: &'static str,
    fields: &'static [FieldSpec],
}

/// Every known config entry, in template order. New knobs must be added here
/// (and to the template composition) to be auto-completed into user files;
/// `template_matches_section_table` pins the two against each other.
const SECTIONS: &[SectionSpec] = &[
    SectionSpec {
        key: "mode",
        snippet: MODE_SECTION_SNIPPET,
        fields: &[],
    },
    SectionSpec {
        key: "peer",
        snippet: PEER_SECTION_SNIPPET,
        fields: &[
            FieldSpec {
                key: "standard_buffer_size",
                snippet: STANDARD_BUFFER_FIELD_SNIPPET,
            },
            FieldSpec {
                key: "high_throughput_buffer_size",
                snippet: HIGH_THROUGHPUT_BUFFER_FIELD_SNIPPET,
            },
        ],
    },
    SectionSpec {
        key: "lifecycle",
        snippet: LIFECYCLE_SECTION_SNIPPET,
        fields: &[
            FieldSpec {
                key: "daemon_grace_secs",
                snippet: DAEMON_GRACE_FIELD_SNIPPET,
            },
            FieldSpec {
                key: "shutdown_grace_secs",
                snippet: SHUTDOWN_GRACE_FIELD_SNIPPET,
            },
        ],
    },
    SectionSpec {
        key: "resource_servers",
        snippet: RESOURCE_SERVERS_SECTION_SNIPPET,
        fields: &[FieldSpec {
            key: "api",
            snippet: API_FIELD_SNIPPET,
        }],
    },
];

/// Returns `content` with every missing known section or field appended from
/// the bundled template, or `None` when the file already spells out all of them
/// (or, defensively, when the content cannot be analyzed; the caller treats
/// both as "leave the file alone").
///
/// Expects `content` to already have parsed successfully as a `PeppyConfig`;
/// malformed input simply returns `None` rather than guessing at splice points.
pub(super) fn complete_config_content(content: &str) -> Option<String> {
    let doc: Value = serde_json5::from_str(content).ok()?;
    let doc = doc.as_object()?;

    let mut missing_sections: Vec<&SectionSpec> = Vec::new();
    let mut incomplete_sections: Vec<(&SectionSpec, Vec<&FieldSpec>)> = Vec::new();
    for section in SECTIONS {
        match doc.get(section.key) {
            None => missing_sections.push(section),
            Some(value) => {
                // A present-but-non-object section cannot have parsed as a
                // `PeppyConfig`; covered here anyway so this function never
                // relies on its caller's checks.
                let Some(block) = value.as_object() else {
                    continue;
                };
                let absent: Vec<&FieldSpec> = section
                    .fields
                    .iter()
                    .filter(|field| !block.contains_key(field.key))
                    .collect();
                if !absent.is_empty() {
                    incomplete_sections.push((section, absent));
                }
            }
        }
    }
    if missing_sections.is_empty() && incomplete_sections.is_empty() {
        return None;
    }

    let layout = scan_layout(content)?;

    // Each entry is (byte offset in `content`, text to insert there). Applied
    // in descending offset order so earlier offsets stay valid. When two
    // insertions share an offset, the one applied later lands EARLIER in the
    // output; every comma is therefore pushed after its snippet, so it always
    // ends up immediately behind the existing last entry.
    let mut insertions: Vec<(usize, String)> = Vec::new();

    if !missing_sections.is_empty() {
        let mut text = String::new();
        for section in &missing_sections {
            text.push('\n');
            text.push_str(section.snippet);
        }
        insertions.push((layout.root.close, text));
        if let Some(at) = layout.root.trailing_comma_insertion() {
            insertions.push((at, ",".to_string()));
        }
    }

    for (section, fields) in &incomplete_sections {
        // A block the scanner could not pair with this key (say, a key spelled
        // with string escapes) is skipped: the in-memory defaults still apply,
        // the file just keeps omitting the field.
        let Some(block) = layout.blocks.get(section.key) else {
            continue;
        };
        let mut text = String::new();
        for field in fields {
            text.push('\n');
            text.push_str(field.snippet);
        }
        insertions.push((block.close, text));
        if let Some(at) = block.trailing_comma_insertion() {
            insertions.push((at, ",".to_string()));
        }
    }
    if insertions.is_empty() {
        return None;
    }

    insertions.sort_by_key(|&(at, _)| std::cmp::Reverse(at));
    let mut completed = content.to_string();
    for (at, text) in insertions {
        completed.insert_str(at, &text);
    }
    Some(completed)
}

/// Whether `completed` is a faithful completion of `original`, parsed as
/// `config`: it must parse back to the same `PeppyConfig`, need no further
/// completion (every known knob now spelled out, so the splice cannot repeat
/// on the next start), and preserve every value the user wrote. Any `false`
/// means a splicing bug; the caller then discards `completed` unwritten.
pub(super) fn verify_completion(
    original: &str,
    completed: &str,
    config: &super::PeppyConfig,
) -> bool {
    let Ok(reparsed) = serde_json5::from_str::<super::PeppyConfig>(completed) else {
        return false;
    };
    if reparsed != *config {
        return false;
    }
    if complete_config_content(completed).is_some() {
        return false;
    }
    // The typed checks above ignore unknown keys, so also require every value
    // of the original document to survive untouched in the completed one.
    let Ok(original_doc) = serde_json5::from_str::<Value>(original) else {
        return false;
    };
    let Ok(completed_doc) = serde_json5::from_str::<Value>(completed) else {
        return false;
    };
    every_value_preserved(&original_doc, &completed_doc)
}

/// Every value in `original` must appear unchanged in `completed`. Objects may
/// gain entries (the spliced defaults) but never lose or alter existing ones;
/// anything else, arrays included, must be identical.
fn every_value_preserved(original: &Value, completed: &Value) -> bool {
    match (original, completed) {
        (Value::Object(original), Value::Object(completed)) => {
            original.iter().all(|(key, value)| {
                completed
                    .get(key)
                    .is_some_and(|kept| every_value_preserved(value, kept))
            })
        }
        _ => original == completed,
    }
}

/// Where an object literal opens and closes in the source text, plus what the
/// last significant (non-whitespace, non-comment) character inside it is, which
/// decides whether appending an entry needs a separating comma first.
struct BlockSpan {
    /// Byte offset just past the opening `{`.
    open_end: usize,
    /// Byte offset of the closing `}`.
    close: usize,
    /// Byte offset just past the last significant character before the closing
    /// brace. Equal to `open_end` when the object is empty.
    last_significant_end: usize,
    /// The last significant character itself (the `{` itself when empty).
    last_significant_char: char,
}

impl BlockSpan {
    /// Byte offset at which a `,` must be inserted before appending another
    /// entry to this object, or `None` when the object is empty or its last
    /// entry already has a trailing comma.
    fn trailing_comma_insertion(&self) -> Option<usize> {
        let is_empty = self.last_significant_end == self.open_end;
        if is_empty || self.last_significant_char == ',' {
            return None;
        }
        Some(self.last_significant_end)
    }
}

/// The insertion points of a config document: the root object, and every
/// top-level key whose value is an object literal (keyed by name, last
/// occurrence winning to match serde's duplicate-key behavior).
struct DocumentLayout {
    root: BlockSpan,
    blocks: HashMap<String, BlockSpan>,
}

/// Scanner state: inside code, a string literal, or a comment.
enum ScanState {
    Code,
    Str { quote: char, escaped: bool },
    LineComment,
    BlockComment,
}

/// One unclosed `{` or `[`. `key` is set for an object that is the value of a
/// top-level entry (and left `None` for the root, arrays, and deeper nesting).
struct OpenDelimiter {
    is_object: bool,
    open_end: usize,
    key: Option<String>,
}

/// Single-pass scan of JSON5 text for the structure [`complete_config_content`]
/// needs. Tracks just enough of the grammar to never misread a brace: string
/// literals (with escapes), line and block comments, and `{`/`[` nesting.
/// Returns `None` on structurally broken input.
fn scan_layout(content: &str) -> Option<DocumentLayout> {
    let mut state = ScanState::Code;
    let mut stack: Vec<OpenDelimiter> = Vec::new();
    let mut root: Option<BlockSpan> = None;
    let mut blocks: HashMap<String, BlockSpan> = HashMap::new();

    // Last significant char seen anywhere (offset-just-past, char).
    let mut last_significant: Option<(usize, char)> = None;
    // An identifier-ish token currently being accumulated (start offset).
    let mut word_start: Option<usize> = None;
    // The most recent completed identifier or string token at root level; a
    // following `:` turns it into `pending_key`.
    let mut last_root_token: Option<String> = None;
    // Set between `key:` and its value, at root level only.
    let mut pending_key: Option<String> = None;
    // Start offset of the string literal currently being scanned.
    let mut string_start = 0usize;

    let mut chars = content.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match state {
            ScanState::Str { quote, escaped } => {
                if escaped {
                    state = ScanState::Str {
                        quote,
                        escaped: false,
                    };
                } else if c == '\\' {
                    state = ScanState::Str {
                        quote,
                        escaped: true,
                    };
                } else if c == quote {
                    if stack.len() == 1 {
                        last_root_token = Some(content[string_start + 1..i].to_string());
                    }
                    last_significant = Some((i + c.len_utf8(), quote));
                    state = ScanState::Code;
                }
            }
            ScanState::LineComment => {
                // JSON5 terminates a line comment at any ECMAScript
                // LineTerminator, not just LF; exiting only on '\n' would let
                // code hide between a lone CR (or U+2028/U+2029) and the next
                // LF, where serde_json5 sees it but this scanner would not.
                if matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}') {
                    state = ScanState::Code;
                }
            }
            ScanState::BlockComment => {
                if c == '*' && matches!(chars.peek(), Some((_, '/'))) {
                    chars.next();
                    state = ScanState::Code;
                }
            }
            ScanState::Code => {
                // Word tokens end at whitespace, a structural char, a quote, or
                // a comment; close the current one before handling `c`.
                let ends_word = c.is_whitespace()
                    || matches!(c, '{' | '}' | '[' | ']' | ',' | ':' | '"' | '\'')
                    || (c == '/' && matches!(chars.peek(), Some((_, '/' | '*'))));
                // `take()` must run for every word end so the word is cleared
                // even outside root level; the `&&` chain keeps that order.
                if ends_word
                    && let Some(start) = word_start.take()
                    && stack.len() == 1
                {
                    last_root_token = Some(content[start..i].to_string());
                }

                if c == '/' && matches!(chars.peek(), Some((_, '/'))) {
                    chars.next();
                    state = ScanState::LineComment;
                    continue;
                }
                if c == '/' && matches!(chars.peek(), Some((_, '*'))) {
                    chars.next();
                    state = ScanState::BlockComment;
                    continue;
                }
                if c == '"' || c == '\'' {
                    string_start = i;
                    state = ScanState::Str {
                        quote: c,
                        escaped: false,
                    };
                    continue;
                }
                if c.is_whitespace() {
                    continue;
                }

                match c {
                    '{' => {
                        let key = if stack.len() == 1 {
                            pending_key.take()
                        } else {
                            pending_key = None;
                            None
                        };
                        stack.push(OpenDelimiter {
                            is_object: true,
                            open_end: i + 1,
                            key,
                        });
                    }
                    '[' => {
                        pending_key = None;
                        stack.push(OpenDelimiter {
                            is_object: false,
                            open_end: i + 1,
                            key: None,
                        });
                    }
                    '}' => {
                        let frame = stack.pop()?;
                        if !frame.is_object {
                            return None;
                        }
                        // `last_significant` still predates this brace, which
                        // is exactly the "last entry" info the span needs.
                        let (last_significant_end, last_significant_char) = last_significant?;
                        let span = BlockSpan {
                            open_end: frame.open_end,
                            close: i,
                            last_significant_end,
                            last_significant_char,
                        };
                        if stack.is_empty() {
                            root = Some(span);
                        } else if stack.len() == 1
                            && let Some(key) = frame.key
                        {
                            blocks.insert(key, span);
                        }
                    }
                    ']' => {
                        let frame = stack.pop()?;
                        if frame.is_object {
                            return None;
                        }
                    }
                    ',' => {
                        if stack.len() == 1 {
                            last_root_token = None;
                            pending_key = None;
                        }
                    }
                    ':' => {
                        if stack.len() == 1 {
                            pending_key = last_root_token.take();
                        }
                    }
                    _ => {
                        if word_start.is_none() {
                            word_start = Some(i);
                        }
                    }
                }
                last_significant = Some((i + c.len_utf8(), c));
            }
        }
    }

    if !stack.is_empty() {
        return None;
    }
    Some(DocumentLayout {
        root: root?,
        blocks,
    })
}

#[cfg(test)]
mod tests {
    use super::super::{
        DEFAULT_DAEMON_GRACE_SECS, DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE,
        DEFAULT_PEPPY_CONFIG_TEMPLATE, DEFAULT_SHUTDOWN_GRACE_SECS, PeppyConfig, TEMPLATE_HEADER,
    };
    use super::*;

    /// Parses completed content the way `load_or_create` would, so every test
    /// proves its splice result is real loadable config, not just plausible text.
    fn parse(content: &str) -> PeppyConfig {
        serde_json5::from_str(content).expect("completed content must stay valid json5")
    }

    /// The template must be exactly the section table in order, or splices
    /// would diverge from what a fresh file gets.
    #[test]
    fn template_matches_section_table() {
        let mut composed = String::from(TEMPLATE_HEADER);
        composed.push_str("{\n");
        for section in SECTIONS {
            composed.push('\n');
            composed.push_str(section.snippet);
        }
        // The first section follows the opening brace directly, without the
        // blank-line separator the splice path adds between sections.
        composed = composed.replacen("{\n\n", "{\n", 1);
        composed.push_str("}\n");
        assert_eq!(composed, DEFAULT_PEPPY_CONFIG_TEMPLATE);
    }

    #[test]
    fn complete_template_needs_no_completion() {
        assert_eq!(complete_config_content(DEFAULT_PEPPY_CONFIG_TEMPLATE), None);
    }

    #[test]
    fn empty_object_gains_every_section() {
        let completed = complete_config_content("{}").expect("everything is missing");
        assert_eq!(parse(&completed), PeppyConfig::default());
        for key in ["mode:", "peer:", "lifecycle:", "resource_servers:"] {
            assert!(completed.contains(key), "expected {key} in:\n{completed}");
        }
        // A completed file needs no further completion.
        assert_eq!(complete_config_content(&completed), None);
    }

    #[test]
    fn missing_trailing_comma_gets_one_before_appending() {
        let completed =
            complete_config_content(r#"{ mode: "router" }"#).expect("peer and lifecycle missing");
        let config = parse(&completed);
        assert_eq!(config.mode, super::super::Mode::Router);
        assert_eq!(
            config.lifecycle.daemon_grace_secs,
            DEFAULT_DAEMON_GRACE_SECS
        );
        assert!(completed.contains(r#"mode: "router","#));
    }

    #[test]
    fn partial_peer_block_gains_missing_field() {
        let completed = complete_config_content(r#"{ peer: { standard_buffer_size: 64 } }"#)
            .expect("high_throughput_buffer_size missing");
        let config = parse(&completed);
        assert_eq!(config.peer.standard_buffer_size, 64);
        assert_eq!(
            config.peer.high_throughput_buffer_size,
            DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE
        );
        assert_eq!(complete_config_content(&completed), None);
    }

    #[test]
    fn partial_lifecycle_block_gains_field_with_its_comment() {
        let completed = complete_config_content(r#"{ lifecycle: { daemon_grace_secs: 600, } }"#)
            .expect("shutdown_grace_secs missing");
        let config = parse(&completed);
        assert_eq!(config.lifecycle.daemon_grace_secs, 600);
        assert_eq!(
            config.lifecycle.shutdown_grace_secs,
            DEFAULT_SHUTDOWN_GRACE_SECS
        );
        // The spliced field brings its explanatory comment along.
        assert!(completed.contains("// How long a clean shutdown"));
    }

    #[test]
    fn empty_nested_blocks_gain_their_fields() {
        let completed =
            complete_config_content("{ peer: {}, lifecycle: {} }").expect("fields missing");
        assert_eq!(parse(&completed), PeppyConfig::default());
        assert_eq!(complete_config_content(&completed), None);
    }

    #[test]
    fn user_content_survives_byte_for_byte() {
        let content = r#"// my own note about why router mode
{
  mode: "router", // pinned during the lab demo
  future_knob: { nested: "}{" },
  /* braces in comments: } { */
}
// trailing remark
"#;
        let completed =
            complete_config_content(content).expect("peer, lifecycle, resource_servers missing");
        parse(&completed);

        // Pin the exact splice: the missing sections go in front of the root's
        // closing brace (its last '}'; the trailing remark contains none), with
        // no comma added since the last entry already has one, and every other
        // byte of the user's file untouched.
        let close = content.rfind('}').unwrap();
        let expected = format!(
            "{}\n{}\n{}\n{}{}",
            &content[..close],
            super::super::PEER_SECTION_SNIPPET,
            super::super::LIFECYCLE_SECTION_SNIPPET,
            super::super::RESOURCE_SERVERS_SECTION_SNIPPET,
            &content[close..]
        );
        assert_eq!(completed, expected);
    }

    #[test]
    fn comma_lands_before_a_trailing_comment() {
        // Also covers JSON5 single-quoted strings.
        let completed = complete_config_content("{ mode: 'router' // why router\n}")
            .expect("peer and lifecycle missing");
        let config = parse(&completed);
        assert_eq!(config.mode, super::super::Mode::Router);
        // The separating comma goes right after the value, not after the
        // comment that trails it.
        assert!(
            completed.contains("mode: 'router', // why router"),
            "comma landed elsewhere in:\n{completed}"
        );
    }

    /// JSON5 ends a `//` comment at any LineTerminator (LF, CR, U+2028,
    /// U+2029). A scanner that only honors LF reads code that hides between a
    /// lone CR and the next LF differently from serde_json5; two such comments
    /// used to rotate brace pairings and splice into the wrong user object.
    #[test]
    fn lone_cr_terminates_line_comments_like_serde() {
        let content = "{\npeer: { standard_buffer_size: 1 // X\r },\njunk: // Y\r {\nb: 2 },\nmode: \"router\"\n}\n";
        let completed = complete_config_content(content).expect("fields missing");
        let config = parse(&completed);
        assert_eq!(config.peer.standard_buffer_size, 1);
        assert_eq!(
            config.peer.high_throughput_buffer_size,
            DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE
        );

        // The buffer field belongs inside `peer`, not inside the unknown
        // `junk` object whose braces sit behind the CR-terminated comments.
        let junk_block =
            &completed[completed.find("junk").unwrap()..completed.find("mode").unwrap()];
        assert!(
            !junk_block.contains("high_throughput_buffer_size"),
            "spliced into junk:\n{completed}"
        );
        assert!(verify_completion(content, &completed, &config));
        assert_eq!(complete_config_content(&completed), None);
    }

    #[test]
    fn u2028_terminates_line_comments_like_serde() {
        let content = "{ mode: \"peer\" // note\u{2028}}";
        let completed = complete_config_content(content).expect("peer and lifecycle missing");
        assert_eq!(parse(&completed), PeppyConfig::default());
        assert_eq!(complete_config_content(&completed), None);
    }

    #[test]
    fn verify_completion_rejects_unfaithful_results() {
        let original = r#"{ note: "keep me" }"#;
        let config: PeppyConfig = serde_json5::from_str(original).unwrap();
        let completed = complete_config_content(original).expect("everything missing");
        assert!(verify_completion(original, &completed, &config));

        // Still missing knobs: would splice again on every start.
        assert!(!verify_completion(
            original,
            r#"{ note: "keep me", mode: "peer" }"#,
            &config
        ));
        // An altered unknown-key value is corruption even though the typed
        // config parses identically.
        let tampered = completed.replace("keep me", "lost");
        assert!(!verify_completion(original, &tampered, &config));
        // A different parsed config is rejected outright.
        let mode_flipped = completed.replace(r#"mode: "peer""#, r#"mode: "router""#);
        assert!(!verify_completion(original, &mode_flipped, &config));
    }

    #[test]
    fn quoted_keys_are_recognized() {
        let completed = complete_config_content(r#"{ "lifecycle": { "daemon_grace_secs": 60 } }"#)
            .expect("shutdown_grace_secs missing");
        let config = parse(&completed);
        assert_eq!(config.lifecycle.daemon_grace_secs, 60);
        assert_eq!(
            config.lifecycle.shutdown_grace_secs,
            DEFAULT_SHUTDOWN_GRACE_SECS
        );
        // The existing quoted block was completed in place, not duplicated.
        assert_eq!(completed.matches("lifecycle").count(), 1);
    }

    #[test]
    fn adjacent_closing_braces_get_comma_between_entries() {
        // Worst-case offsets: the block close, the root's comma insertion, and
        // the root's section insertion all touch neighboring bytes.
        let completed = complete_config_content("{peer:{}}").expect("everything else missing");
        assert_eq!(parse(&completed), PeppyConfig::default());
        assert_eq!(complete_config_content(&completed), None);
    }

    #[test]
    fn arrays_in_unknown_keys_do_not_confuse_the_scanner() {
        let completed = complete_config_content(r#"{ tags: ["a", "b", { x: 1 }], mode: "peer" }"#)
            .expect("peer and lifecycle missing");
        assert_eq!(parse(&completed), PeppyConfig::default());
        assert!(completed.contains(r#"tags: ["a", "b", { x: 1 }]"#));
    }

    #[test]
    fn malformed_content_is_left_alone() {
        assert_eq!(complete_config_content("{ mode: "), None);
        assert_eq!(complete_config_content(""), None);
        assert_eq!(complete_config_content("[1, 2]"), None);
    }
}
