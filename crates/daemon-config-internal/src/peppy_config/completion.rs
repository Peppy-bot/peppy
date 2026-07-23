//! Comment-preserving completion of a user's `peppy_config.json5`.
//!
//! [`complete_config_content`] detects every missing known entry and splices
//! each one whose parent block it can safely locate, at any nesting depth,
//! into the file content. It copies the snippet (explanatory comments
//! included) from the bundled template. Everything the user wrote survives
//! byte-for-byte: their values, their comments, their formatting, and any
//! unknown keys. Omissions whose source spelling the scanner cannot pair with
//! a block are reported separately instead of being silently skipped.
//!
//! Rewriting the file through serde would destroy comments, and no
//! comment-preserving JSON5 editor crate exists, so this works directly on the
//! text: a minimal JSON5-aware scanner (strings, comments, and brace nesting,
//! nothing more) locates the insertion points, and the snippets are inserted
//! before the relevant closing brace. Before anything is written, the caller
//! gates the result through [`verify_completion`], so a splicing bug cannot
//! drop or alter any value the user wrote, cannot change what the file parses
//! to, and cannot leave another splice possible on the next start; a bad
//! splice is discarded with a warning and the user's file stays untouched.

use serde_json::Value;
use std::collections::HashMap;

use super::{
    API_FIELD_SNIPPET, CORE_NODE_NAME_SECTION_SNIPPET, DAEMON_GRACE_FIELD_SNIPPET,
    FEDERATION_SECTION_SNIPPET, FEDERATION_TIMEOUT_FIELD_SNIPPET,
    HIGH_THROUGHPUT_BUFFER_FIELD_SNIPPET, LIFECYCLE_SECTION_SNIPPET,
    LOCAL_NODES_TOPOLOGY_FIELD_SNIPPET, MANAGED_SECTION_SNIPPET, RESOURCE_SERVERS_SECTION_SNIPPET,
    SHUTDOWN_GRACE_FIELD_SNIPPET, STANDARD_BUFFER_FIELD_SNIPPET,
    SUBSCRIBER_BUFFERS_SECTION_SNIPPET, ZENOH_SECTION_SNIPPET,
};

/// One known config entry: the template snippet to splice into its parent
/// block when the entry is absent, and the nested entries to complete
/// individually when it is present but incomplete. A leaf has no children; an
/// entry with children must have a snippet that embeds every child snippet
/// (`nested_snippets_compose_their_parents` pins that), so splicing a whole
/// missing branch and completing it field-by-field agree. An alternative key
/// counts the canonical entry as present without becoming a spliceable entry
/// of its own.
struct EntrySpec {
    key: &'static str,
    snippet: &'static str,
    alternatives: &'static [&'static str],
    children: &'static [EntrySpec],
}

/// Every known config entry, in template order. New knobs must be added here
/// (and to the template composition) to be auto-completed into user files;
/// `template_matches_section_table` pins the two against each other.
const SECTIONS: &[EntrySpec] = &[
    // The template materializes this entry as an explicit `core_node_name:
    // null,`: [`complete_config_content`] counts a null value as present (a
    // leaf spec never descends into its value), so the splice cannot repeat
    // once the line exists.
    EntrySpec {
        key: "core_node_name",
        snippet: CORE_NODE_NAME_SECTION_SNIPPET,
        alternatives: &[],
        children: &[],
    },
    EntrySpec {
        key: "zenoh",
        snippet: ZENOH_SECTION_SNIPPET,
        alternatives: &[],
        children: &[EntrySpec {
            key: "managed",
            snippet: MANAGED_SECTION_SNIPPET,
            alternatives: &["external"],
            children: &[
                EntrySpec {
                    key: "local_nodes_topology",
                    snippet: LOCAL_NODES_TOPOLOGY_FIELD_SNIPPET,
                    alternatives: &[],
                    children: &[],
                },
                EntrySpec {
                    key: "subscriber_buffers",
                    snippet: SUBSCRIBER_BUFFERS_SECTION_SNIPPET,
                    alternatives: &[],
                    children: &[
                        EntrySpec {
                            key: "standard_buffer_size",
                            snippet: STANDARD_BUFFER_FIELD_SNIPPET,
                            alternatives: &[],
                            children: &[],
                        },
                        EntrySpec {
                            key: "high_throughput_buffer_size",
                            snippet: HIGH_THROUGHPUT_BUFFER_FIELD_SNIPPET,
                            alternatives: &[],
                            children: &[],
                        },
                    ],
                },
                EntrySpec {
                    key: "federation",
                    snippet: FEDERATION_SECTION_SNIPPET,
                    alternatives: &[],
                    children: &[EntrySpec {
                        key: "connect_timeout_secs",
                        snippet: FEDERATION_TIMEOUT_FIELD_SNIPPET,
                        alternatives: &[],
                        children: &[],
                    }],
                },
            ],
        }],
    },
    EntrySpec {
        key: "lifecycle",
        snippet: LIFECYCLE_SECTION_SNIPPET,
        alternatives: &[],
        children: &[
            EntrySpec {
                key: "daemon_grace_secs",
                snippet: DAEMON_GRACE_FIELD_SNIPPET,
                alternatives: &[],
                children: &[],
            },
            EntrySpec {
                key: "shutdown_grace_secs",
                snippet: SHUTDOWN_GRACE_FIELD_SNIPPET,
                alternatives: &[],
                children: &[],
            },
        ],
    },
    EntrySpec {
        key: "resource_servers",
        snippet: RESOURCE_SERVERS_SECTION_SNIPPET,
        alternatives: &[],
        children: &[EntrySpec {
            key: "api",
            snippet: API_FIELD_SNIPPET,
            alternatives: &[],
            children: &[],
        }],
    },
];

/// A completion result: the rewritten file content plus what was spliced in.
///
/// `added_paths` names each spliced entry as a dot-separated path
/// ("federation" spelled "zenoh.managed.federation", a nested field
/// "lifecycle.shutdown_grace_secs"), grouped by the block it was spliced into
/// (outer blocks first) and in template order within a block. It reflects what
/// was actually inserted, not merely what was absent.
///
/// `unspliceable_paths` names settings that were detected as absent but whose
/// parent block the scanner could not safely locate. Those settings remain
/// absent from `content` and are never included in `added_paths`.
#[derive(Debug)]
pub(super) struct Completion {
    pub(super) content: String,
    pub(super) added_paths: Vec<String>,
    pub(super) unspliceable_paths: Vec<String>,
}

/// A known entry absent from the document: the path of the block it splices
/// into (root = empty), its template snippet, and its dotted path for the
/// added-settings log.
struct MissingEntry {
    parent_path: Vec<String>,
    snippet: &'static str,
    path: String,
}

/// Walks `specs` against `block`, recording every absent entry and descending
/// into present entries that have children. Per level, the absent entries come
/// first and the descents after, so the added-settings log lists each block's
/// own gaps before its children's.
fn collect_missing(
    specs: &'static [EntrySpec],
    block: &serde_json::Map<String, Value>,
    parent_path: &[String],
    missing: &mut Vec<MissingEntry>,
) {
    for spec in specs {
        if !block.contains_key(spec.key)
            && !spec
                .alternatives
                .iter()
                .any(|alternative| block.contains_key(*alternative))
        {
            let path = if parent_path.is_empty() {
                spec.key.to_string()
            } else {
                format!("{}.{}", parent_path.join("."), spec.key)
            };
            missing.push(MissingEntry {
                parent_path: parent_path.to_vec(),
                snippet: spec.snippet,
                path,
            });
        }
    }
    for spec in specs {
        if spec.children.is_empty() {
            continue;
        }
        let Some(value) = block.get(spec.key) else {
            continue;
        };
        // A present-but-non-object entry cannot have parsed as a
        // `PeppyConfig`; covered here anyway so this function never relies on
        // its caller's checks.
        let Some(child_block) = value.as_object() else {
            continue;
        };
        let mut child_path = parent_path.to_vec();
        child_path.push(spec.key.to_string());
        collect_missing(spec.children, child_block, &child_path, missing);
    }
}

/// Returns every omitted path in the order its entry appears in the bundled
/// template, regardless of whether the path was spliceable.
pub(super) fn missing_paths_in_template_order(completion: &Completion) -> Vec<String> {
    fn collect_known_paths(specs: &[EntrySpec], parent: &str, paths: &mut Vec<String>) {
        for spec in specs {
            let path = if parent.is_empty() {
                spec.key.to_string()
            } else {
                format!("{parent}.{}", spec.key)
            };
            paths.push(path.clone());
            collect_known_paths(spec.children, &path, paths);
        }
    }

    let mut known_paths = Vec::new();
    collect_known_paths(SECTIONS, "", &mut known_paths);

    let mut missing_paths: Vec<String> = completion
        .added_paths
        .iter()
        .chain(&completion.unspliceable_paths)
        .cloned()
        .collect();
    missing_paths.sort_by_key(|path| {
        known_paths
            .iter()
            .position(|known| known == path)
            .unwrap_or(usize::MAX)
    });
    missing_paths
}

/// Returns `content` with every safely placeable missing known entry appended
/// from the bundled template, plus the paths that were appended and the paths
/// that could not be placed. Returns `None` when the file already spells out
/// every known setting or, defensively, when the content cannot be analyzed.
///
/// Expects `content` to already have parsed successfully as a `PeppyConfig`;
/// malformed input simply returns `None` rather than guessing at splice points.
pub(super) fn complete_config_content(content: &str) -> Option<Completion> {
    let doc: Value = serde_json5::from_str(content).ok()?;
    let doc = doc.as_object()?;

    let mut missing: Vec<MissingEntry> = Vec::new();
    collect_missing(SECTIONS, doc, &[], &mut missing);
    if missing.is_empty() {
        return None;
    }

    let layout = match scan_layout(content) {
        Some(layout) => layout,
        None => {
            return Some(Completion {
                content: content.to_string(),
                added_paths: Vec::new(),
                unspliceable_paths: missing.into_iter().map(|entry| entry.path).collect(),
            });
        }
    };

    // One insertion per target block: group the missing entries by their
    // parent path, preserving first-encounter order, so siblings share a
    // single splice (and at most one separating comma).
    let mut groups: Vec<(Vec<String>, Vec<MissingEntry>)> = Vec::new();
    for entry in missing {
        match groups
            .iter_mut()
            .find(|(path, _)| *path == entry.parent_path)
        {
            Some((_, entries)) => entries.push(entry),
            None => groups.push((entry.parent_path.clone(), vec![entry])),
        }
    }

    // Each entry is (byte offset in `content`, text to insert there). Applied
    // in descending offset order so earlier offsets stay valid. When two
    // insertions share an offset, the one applied later lands EARLIER in the
    // output; every comma is therefore pushed after its snippet, so it always
    // ends up immediately behind the existing last entry.
    let mut insertions: Vec<(usize, String)> = Vec::new();
    // Collected alongside the insertions, not from the classification above,
    // so a skipped splice (unlocatable block) is never reported as added.
    let mut added_paths: Vec<String> = Vec::new();
    let mut unspliceable_paths: Vec<String> = Vec::new();

    for (parent_path, entries) in groups {
        // A block the scanner could not pair with its key chain (say, a key
        // spelled with string escapes) cannot be safely changed. The in-memory
        // defaults still apply, and every affected path is reported.
        let Some(block) = layout.block_at(&parent_path) else {
            unspliceable_paths.extend(entries.into_iter().map(|entry| entry.path));
            continue;
        };
        let mut text = String::new();
        for entry in entries {
            text.push('\n');
            text.push_str(entry.snippet);
            added_paths.push(entry.path);
        }
        insertions.push((block.close, text));
        if let Some(at) = block.trailing_comma_insertion() {
            insertions.push((at, ",".to_string()));
        }
    }
    insertions.sort_by_key(|&(at, _)| std::cmp::Reverse(at));
    let mut completed = content.to_string();
    for (at, text) in insertions {
        completed.insert_str(at, &text);
    }
    Some(Completion {
        content: completed,
        added_paths,
        unspliceable_paths,
    })
}

/// Whether `completed` is a faithful completion of `original`, parsed as
/// `config`: it must parse back to the same `PeppyConfig`, leave no further
/// splice possible on the next start, and preserve every value the user wrote.
/// An omission that remains unspliceable is acceptable because another pass
/// cannot make progress on it. Any `false` means a splicing bug; the caller
/// then discards `completed` unwritten.
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
    if complete_config_content(completed)
        .is_some_and(|completion| !completion.added_paths.is_empty())
    {
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
/// object literal reachable from it through a chain of object keys, keyed by
/// that chain (last occurrence winning to match serde's duplicate-key
/// behavior). Objects inside arrays have no key chain and are not recorded.
struct DocumentLayout {
    root: BlockSpan,
    blocks: HashMap<Vec<String>, BlockSpan>,
}

impl DocumentLayout {
    fn block_at(&self, path: &[String]) -> Option<&BlockSpan> {
        if path.is_empty() {
            return Some(&self.root);
        }
        self.blocks.get(path)
    }
}

/// Scanner state: inside code, a string literal, or a comment.
enum ScanState {
    Code,
    Str { quote: char, escaped: bool },
    LineComment,
    BlockComment,
}

/// One unclosed `{` or `[`, with the key-tracking state of its own entries.
/// `key` is set for an object that is the value of a keyed entry in an object
/// (and left `None` for the root, arrays, and array elements).
struct OpenDelimiter {
    is_object: bool,
    open_end: usize,
    key: Option<String>,
    /// The most recent completed identifier or string token directly inside
    /// this block; a following `:` turns it into `pending_key`.
    last_token: Option<String>,
    /// Set between `key:` and its value, for object blocks only.
    pending_key: Option<String>,
}

/// Single-pass scan of JSON5 text for the structure [`complete_config_content`]
/// needs. Tracks just enough of the grammar to never misread a brace: string
/// literals (with escapes), line and block comments, and `{`/`[` nesting.
/// Returns `None` on structurally broken input.
fn scan_layout(content: &str) -> Option<DocumentLayout> {
    let mut state = ScanState::Code;
    let mut stack: Vec<OpenDelimiter> = Vec::new();
    let mut root: Option<BlockSpan> = None;
    let mut blocks: HashMap<Vec<String>, BlockSpan> = HashMap::new();

    // Last significant char seen anywhere (offset-just-past, char).
    let mut last_significant: Option<(usize, char)> = None;
    // An identifier-ish token currently being accumulated (start offset).
    let mut word_start: Option<usize> = None;
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
                    if let Some(frame) = stack.last_mut()
                        && frame.is_object
                    {
                        frame.last_token = Some(content[string_start + 1..i].to_string());
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
                // even inside arrays; the `&&` chain keeps that order.
                if ends_word
                    && let Some(start) = word_start.take()
                    && let Some(frame) = stack.last_mut()
                    && frame.is_object
                {
                    frame.last_token = Some(content[start..i].to_string());
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
                        let key = stack
                            .last_mut()
                            .filter(|parent| parent.is_object)
                            .and_then(|parent| parent.pending_key.take());
                        stack.push(OpenDelimiter {
                            is_object: true,
                            open_end: i + 1,
                            key,
                            last_token: None,
                            pending_key: None,
                        });
                    }
                    '[' => {
                        if let Some(parent) = stack.last_mut() {
                            parent.pending_key = None;
                        }
                        stack.push(OpenDelimiter {
                            is_object: false,
                            open_end: i + 1,
                            key: None,
                            last_token: None,
                            pending_key: None,
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
                        } else if let Some(key) = frame.key
                            && stack[1..]
                                .iter()
                                .all(|ancestor| ancestor.is_object && ancestor.key.is_some())
                        {
                            // The full key chain from the root: every ancestor
                            // besides the root object contributes its key.
                            let mut path: Vec<String> = stack[1..]
                                .iter()
                                .map(|ancestor| ancestor.key.clone().expect("checked above"))
                                .collect();
                            path.push(key);
                            blocks.insert(path, span);
                        }
                    }
                    ']' => {
                        let frame = stack.pop()?;
                        if frame.is_object {
                            return None;
                        }
                    }
                    ',' => {
                        if let Some(frame) = stack.last_mut() {
                            frame.last_token = None;
                            frame.pending_key = None;
                        }
                    }
                    ':' => {
                        if let Some(frame) = stack.last_mut()
                            && frame.is_object
                        {
                            frame.pending_key = frame.last_token.take();
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

/// Flattens a serialized config document into dot-separated leaf paths
/// ("lifecycle.daemon_grace_secs"). Objects recurse; everything else (numbers,
/// strings, nulls, arrays) is a leaf, matching what completion can splice as a
/// single value. Test-only: the schema walk behind the struct/table/template
/// coverage pins here and the end-to-end upgrade test in `peppy_config`.
#[cfg(test)]
pub(super) fn leaf_paths(value: &Value) -> Vec<String> {
    fn walk(value: &Value, prefix: &str, paths: &mut Vec<String>) {
        match value.as_object() {
            Some(entries) => {
                for (key, nested) in entries {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    walk(nested, &path, paths);
                }
            }
            None => paths.push(prefix.to_string()),
        }
    }
    let mut paths = Vec::new();
    walk(value, "", &mut paths);
    paths
}

#[cfg(test)]
mod tests {
    use super::super::{
        DEFAULT_DAEMON_GRACE_SECS, DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE,
        DEFAULT_PEPPY_CONFIG_TEMPLATE, DEFAULT_SHUTDOWN_GRACE_SECS, PEPPY_CONFIG_FILE, PeppyConfig,
        SHUTDOWN_GRACE_FIELD_SNIPPET, TEMPLATE_HEADER,
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

    /// A parent snippet that does not embed one of its children would make a
    /// whole-branch splice disagree with a field-by-field completion of the
    /// same branch (and desync fresh files from completed ones).
    #[test]
    fn nested_snippets_compose_their_parents() {
        fn assert_children_embedded(spec: &EntrySpec) {
            for child in spec.children {
                assert!(
                    spec.snippet.contains(child.snippet),
                    "snippet for `{}` does not embed its child `{}`",
                    spec.key,
                    child.key
                );
                assert_children_embedded(child);
            }
        }
        for section in SECTIONS {
            assert_children_embedded(section);
        }
    }

    /// A defaulted `PeppyConfig` field the section table does not cover would
    /// ship a release whose user files silently never gain the setting; a table
    /// entry without a default field would splice a knob the parser ignores.
    /// The default schema is enumerated by serializing `PeppyConfig::default()`
    /// (variant-specific required fields are pinned separately below).
    #[test]
    fn section_table_matches_config_struct() {
        let schema =
            serde_json::to_value(PeppyConfig::default()).expect("PeppyConfig must serialize");
        let mut schema_paths = leaf_paths(&schema);

        fn table_leaf_paths(specs: &[EntrySpec], prefix: &str, out: &mut Vec<String>) {
            for spec in specs {
                let path = if prefix.is_empty() {
                    spec.key.to_string()
                } else {
                    format!("{prefix}.{}", spec.key)
                };
                if spec.children.is_empty() {
                    out.push(path);
                } else {
                    table_leaf_paths(spec.children, &path, out);
                }
            }
        }
        let mut table_paths: Vec<String> = Vec::new();
        table_leaf_paths(SECTIONS, "", &mut table_paths);

        let missing_from_table: Vec<&String> = schema_paths
            .iter()
            .filter(|path| !table_paths.contains(path))
            .collect();
        assert!(
            missing_from_table.is_empty(),
            "PeppyConfig fields that completion cannot splice into user files: \
             {missing_from_table:?}. Files written by older releases would never gain \
             them in {PEPPY_CONFIG_FILE}. For each field: add a snippet const next to \
             the others in peppy_config.rs, splice it into the parent section snippet \
             (or DEFAULT_PEPPY_CONFIG_TEMPLATE for a new top-level entry), and register \
             it in SECTIONS under its parent's `children`."
        );

        let stale_table_entries: Vec<&String> = table_paths
            .iter()
            .filter(|path| !schema_paths.contains(path))
            .collect();
        assert!(
            stale_table_entries.is_empty(),
            "SECTIONS entries that are not PeppyConfig fields: {stale_table_entries:?}. \
             Remove each one together with its template snippet; a renamed or removed \
             setting is a breaking change handled by fail-loud validation and a runbook, \
             not by completion."
        );

        // Sorted equality on top of the set differences above: a duplicate
        // SECTIONS key would pass both filters yet splice twice.
        schema_paths.sort();
        table_paths.sort();
        assert_eq!(schema_paths, table_paths);
    }

    /// The template must spell out exactly the struct's settings: a snippet
    /// carrying a stray key the table never declared would ship unknown keys
    /// in fresh files, and a field skipped from serialization would desync the
    /// pins while `section_table_matches_config_struct` still passes.
    #[test]
    fn template_matches_config_struct() {
        let template: Value = serde_json5::from_str(DEFAULT_PEPPY_CONFIG_TEMPLATE)
            .expect("bundled template must parse");
        let mut template_paths = leaf_paths(&template);
        template_paths.sort();

        let schema =
            serde_json::to_value(PeppyConfig::default()).expect("PeppyConfig must serialize");
        let mut schema_paths = leaf_paths(&schema);
        schema_paths.sort();

        assert_eq!(template_paths, schema_paths);
    }

    /// `zenoh.external.endpoint` is required only by the non-default external
    /// variant, so it must not appear in the managed template or be invented by
    /// config completion. Pin that exceptional schema surface explicitly
    /// instead of weakening the default-schema coverage tests above.
    #[test]
    fn external_zenoh_variant_schema_is_pinned() {
        let external = serde_json::to_value(super::super::ZenohConfig::External(
            super::super::ExternalZenohConfig {
                endpoint: "tcp/router.internal:7448".to_string(),
            },
        ))
        .expect("external zenoh config must serialize");
        assert_eq!(
            external,
            serde_json::json!({
                "external": {
                    "endpoint": "tcp/router.internal:7448",
                },
            })
        );
    }

    /// Pins the payload of the "added settings" log line `load_or_create`
    /// emits: grouped by target block (outer blocks first), template order
    /// within a block.
    #[test]
    fn completion_reports_the_paths_it_added() {
        let completion = complete_config_content(
            r#"{ zenoh: { managed: { local_nodes_topology: "peer" } }, lifecycle: { daemon_grace_secs: 60 } }"#,
        )
        .expect("sections, zenoh entries, and a lifecycle field missing");
        assert_eq!(
            completion.added_paths,
            [
                "core_node_name",
                "resource_servers",
                "zenoh.managed.subscriber_buffers",
                "zenoh.managed.federation",
                "lifecycle.shutdown_grace_secs",
            ]
        );
        // A fully completed file reports nothing to add.
        assert!(complete_config_content(&completion.content).is_none());
    }

    #[test]
    fn complete_template_needs_no_completion() {
        assert!(complete_config_content(DEFAULT_PEPPY_CONFIG_TEMPLATE).is_none());
    }

    #[test]
    fn empty_object_gains_every_section() {
        let completed = complete_config_content("{}")
            .expect("everything is missing")
            .content;
        assert_eq!(parse(&completed), PeppyConfig::default());
        for section in SECTIONS {
            let key = format!("{}:", section.key);
            assert!(completed.contains(&key), "expected {key} in:\n{completed}");
        }
        // A completed file needs no further completion.
        assert!(complete_config_content(&completed).is_none());
    }

    #[test]
    fn empty_zenoh_block_gains_the_managed_block() {
        let completed = complete_config_content("{ zenoh: {} }")
            .expect("managed block is missing")
            .content;
        assert_eq!(parse(&completed), PeppyConfig::default());
        assert_eq!(completed.matches("managed:").count(), 1);
        assert!(complete_config_content(&completed).is_none());
    }

    #[test]
    fn external_zenoh_block_gains_no_managed_knobs() {
        let content = r#"{ zenoh: { external: { endpoint: "tcp/router.internal:7448" } } }"#;
        let completed = complete_config_content(content)
            .expect("top-level sections are missing")
            .content;
        assert!(matches!(
            parse(&completed).zenoh,
            super::super::ZenohConfig::External(_)
        ));
        let document: Value = serde_json5::from_str(&completed).unwrap();
        assert_eq!(
            document["zenoh"],
            serde_json::json!({
                "external": { "endpoint": "tcp/router.internal:7448" }
            })
        );

        let fully_spelled = serde_json5::to_string(&PeppyConfig {
            zenoh: super::super::ZenohConfig::External(super::super::ExternalZenohConfig {
                endpoint: "tcp/router.internal:7448".to_string(),
            }),
            ..PeppyConfig::default()
        })
        .unwrap();
        assert!(complete_config_content(&fully_spelled).is_none());
    }

    #[test]
    fn explicit_null_core_node_name_counts_as_present() {
        // The knob's default is spelled as `null`, not omitted; a file that
        // already carries the null line must not gain a second one.
        let completed = complete_config_content("{ core_node_name: null }")
            .expect("every other section missing")
            .content;
        assert_eq!(parse(&completed), PeppyConfig::default());
        assert_eq!(completed.matches("core_node_name:").count(), 1);
        assert!(complete_config_content(&completed).is_none());
    }

    #[test]
    fn missing_trailing_comma_gets_one_before_appending() {
        let completed = complete_config_content(
            r#"{ zenoh: { managed: { local_nodes_topology: "router" } } }"#,
        )
        .expect("everything else missing")
        .content;
        let config = parse(&completed);
        assert!(!config.zenoh.gossip());
        assert_eq!(
            config.lifecycle.daemon_grace_secs,
            DEFAULT_DAEMON_GRACE_SECS
        );
        // Both the root's last entry and the zenoh block's last entry gained a
        // separating comma before their spliced siblings.
        assert!(completed.contains(r#"local_nodes_topology: "router","#));
        assert!(complete_config_content(&completed).is_none());
    }

    #[test]
    fn partial_subscriber_buffers_block_gains_missing_field() {
        let completed = complete_config_content(
            r#"{ zenoh: { managed: { subscriber_buffers: { standard_buffer_size: 64 } } } }"#,
        )
        .expect("high_throughput_buffer_size missing")
        .content;
        let config = parse(&completed);
        assert_eq!(config.zenoh.subscriber_buffers().standard_buffer_size, 64);
        assert_eq!(
            config
                .zenoh
                .subscriber_buffers()
                .high_throughput_buffer_size,
            DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE
        );
        assert!(complete_config_content(&completed).is_none());
    }

    /// The deepest schema path
    /// (`zenoh.managed.federation.connect_timeout_secs`, four levels) is
    /// spliced into its triply nested block, not a shallower one.
    #[test]
    fn empty_federation_block_gains_timeout_at_depth_four() {
        let completed = complete_config_content(r#"{ zenoh: { managed: { federation: {} } } }"#)
            .expect("connect_timeout_secs and everything else missing")
            .content;
        let config = parse(&completed);
        assert_eq!(config, PeppyConfig::default());
        // The field landed inside the existing federation block (which the
        // user spelled compactly), not as a second federation block.
        assert_eq!(completed.matches("federation: {").count(), 1);
        assert!(completed.contains("connect_timeout_secs:"));
        assert!(complete_config_content(&completed).is_none());
    }

    #[test]
    fn partial_lifecycle_block_gains_field_with_its_comment() {
        let completed = complete_config_content(r#"{ lifecycle: { daemon_grace_secs: 600, } }"#)
            .expect("shutdown_grace_secs missing")
            .content;
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
        let completed = complete_config_content(
            "{ zenoh: { managed: { subscriber_buffers: {}, federation: {} } }, lifecycle: {} }",
        )
        .expect("fields missing")
        .content;
        assert_eq!(parse(&completed), PeppyConfig::default());
        assert!(complete_config_content(&completed).is_none());
    }

    #[test]
    fn user_content_survives_byte_for_byte() {
        let content = r#"// my own note about a future setting
{
  future_setting: "keep", // an unknown key, preserved verbatim
  future_knob: { nested: "}{" },
  /* braces in comments: } { */
}
// trailing remark
"#;
        let completed = complete_config_content(content)
            .expect("core_node_name, zenoh, lifecycle, resource_servers missing")
            .content;
        parse(&completed);

        // Pin the exact splice: the missing sections go in front of the root's
        // closing brace (its last '}'; the trailing remark contains none), with
        // no comma added since the last entry already has one, and every other
        // byte of the user's file untouched.
        let close = content.rfind('}').unwrap();
        let expected = format!(
            "{}\n{}\n{}\n{}\n{}{}",
            &content[..close],
            super::super::CORE_NODE_NAME_SECTION_SNIPPET,
            super::super::ZENOH_SECTION_SNIPPET,
            super::super::LIFECYCLE_SECTION_SNIPPET,
            super::super::RESOURCE_SERVERS_SECTION_SNIPPET,
            &content[close..]
        );
        assert_eq!(completed, expected);
    }

    #[test]
    fn comma_lands_before_a_trailing_comment() {
        // Also covers JSON5 single-quoted strings.
        let completed = complete_config_content("{ core_node_name: 'robot-7' // why named\n}")
            .expect("everything else missing")
            .content;
        let config = parse(&completed);
        assert_eq!(config.core_node_name.as_deref(), Some("robot-7"));
        // The separating comma goes right after the value, not after the
        // comment that trails it.
        assert!(
            completed.contains("core_node_name: 'robot-7', // why named"),
            "comma landed elsewhere in:\n{completed}"
        );
    }

    /// JSON5 ends a `//` comment at any LineTerminator (LF, CR, U+2028,
    /// U+2029). A scanner that only honors LF reads code that hides between a
    /// lone CR and the next LF differently from serde_json5; two such comments
    /// used to rotate brace pairings and splice into the wrong user object.
    #[test]
    fn lone_cr_terminates_line_comments_like_serde() {
        let content = "{\nzenoh: { managed: { subscriber_buffers: { standard_buffer_size: 1 // X\r } } },\njunk: // Y\r {\nb: 2 },\ncore_node_name: null\n}\n";
        let completed = complete_config_content(content)
            .expect("fields missing")
            .content;
        let config = parse(&completed);
        assert_eq!(config.zenoh.subscriber_buffers().standard_buffer_size, 1);
        assert_eq!(
            config
                .zenoh
                .subscriber_buffers()
                .high_throughput_buffer_size,
            DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE
        );

        // The buffer field belongs inside `zenoh.managed.subscriber_buffers`,
        // not inside the unknown `junk` object whose braces sit behind the
        // CR-terminated comments.
        let junk_block =
            &completed[completed.find("junk").unwrap()..completed.find("core_node_name").unwrap()];
        assert!(
            !junk_block.contains("high_throughput_buffer_size"),
            "spliced into junk:\n{completed}"
        );
        assert!(verify_completion(content, &completed, &config));
        assert!(complete_config_content(&completed).is_none());
    }

    #[test]
    fn u2028_terminates_line_comments_like_serde() {
        let content = "{ core_node_name: null // note\u{2028}}";
        let completed = complete_config_content(content)
            .expect("everything else missing")
            .content;
        assert_eq!(parse(&completed), PeppyConfig::default());
        assert!(complete_config_content(&completed).is_none());
    }

    #[test]
    fn verify_completion_rejects_unfaithful_results() {
        let original = r#"{ note: "keep me" }"#;
        let config: PeppyConfig = serde_json5::from_str(original).unwrap();
        let completed = complete_config_content(original)
            .expect("everything missing")
            .content;
        assert!(verify_completion(original, &completed, &config));

        // Still missing knobs: would splice again on every start.
        assert!(!verify_completion(
            original,
            r#"{ note: "keep me", core_node_name: null }"#,
            &config
        ));
        // An altered unknown-key value is corruption even though the typed
        // config parses identically.
        let tampered = completed.replace("keep me", "lost");
        assert!(!verify_completion(original, &tampered, &config));
        // A different parsed config is rejected outright.
        let topology_flipped = completed.replace(
            r#"local_nodes_topology: "peer""#,
            r#"local_nodes_topology: "router""#,
        );
        assert!(!verify_completion(original, &topology_flipped, &config));
    }

    #[test]
    fn quoted_keys_are_recognized() {
        let completed = complete_config_content(r#"{ "lifecycle": { "daemon_grace_secs": 60 } }"#)
            .expect("shutdown_grace_secs missing")
            .content;
        let config = parse(&completed);
        assert_eq!(config.lifecycle.daemon_grace_secs, 60);
        assert_eq!(
            config.lifecycle.shutdown_grace_secs,
            DEFAULT_SHUTDOWN_GRACE_SECS
        );
        // The existing quoted block was completed in place, not duplicated.
        assert_eq!(completed.matches("lifecycle").count(), 1);

        // Quoted keys pair up along the whole nested chain too: the missing
        // buffer field joins the user's quoted block instead of a freshly
        // spliced duplicate `subscriber_buffers` block (which would repeat
        // both fields).
        let completed = complete_config_content(
            r#"{ "zenoh": { "managed": { "subscriber_buffers": { "standard_buffer_size": 64 } } } }"#,
        )
        .expect("high_throughput_buffer_size missing")
        .content;
        let config = parse(&completed);
        assert_eq!(config.zenoh.subscriber_buffers().standard_buffer_size, 64);
        assert_eq!(completed.matches("standard_buffer_size").count(), 1);
        assert_eq!(completed.matches("high_throughput_buffer_size").count(), 1);
        assert!(complete_config_content(&completed).is_none());
    }

    #[test]
    fn escaped_section_key_reports_unspliceable_paths() {
        let content = DEFAULT_PEPPY_CONFIG_TEMPLATE
            .replacen("  lifecycle: {", r#"  "life\u0063ycle": {"#, 1)
            .replacen(SHUTDOWN_GRACE_FIELD_SNIPPET, "", 1);

        let completion =
            complete_config_content(&content).expect("lifecycle.shutdown_grace_secs is missing");

        assert_eq!(parse(&content), PeppyConfig::default());
        assert_eq!(completion.content, content);
        assert!(completion.added_paths.is_empty());
        assert_eq!(
            completion.unspliceable_paths,
            ["lifecycle.shutdown_grace_secs"]
        );
    }

    #[test]
    fn adjacent_closing_braces_get_comma_between_entries() {
        // Worst-case offsets: the doubly-nested block close, its parents'
        // closes, the comma insertions, and the root's section insertion all
        // touch neighboring bytes.
        let completed = complete_config_content("{zenoh:{managed:{subscriber_buffers:{}}}}")
            .expect("everything else missing")
            .content;
        assert_eq!(parse(&completed), PeppyConfig::default());
        assert!(complete_config_content(&completed).is_none());
    }

    #[test]
    fn arrays_in_unknown_keys_do_not_confuse_the_scanner() {
        let completed =
            complete_config_content(r#"{ tags: ["a", "b", { x: 1 }], core_node_name: null }"#)
                .expect("everything else missing")
                .content;
        assert_eq!(parse(&completed), PeppyConfig::default());
        assert!(completed.contains(r#"tags: ["a", "b", { x: 1 }]"#));
    }

    #[test]
    fn malformed_content_is_left_alone() {
        assert!(complete_config_content("{ zenoh: ").is_none());
        assert!(complete_config_content("").is_none());
        assert!(complete_config_content("[1, 2]").is_none());
    }
}
