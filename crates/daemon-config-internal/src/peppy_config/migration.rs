//! Removal of settings this peppy release no longer reads from a user's
//! `peppy_config.json5`.
//!
//! The document is parsed strictly (`deny_unknown_fields`), so a setting that
//! an older release wrote into the file stops the daemon from starting. Each
//! entry in [`REMOVED_SETTINGS`] names one such setting. [`remove_settings`]
//! cuts it out of the text, keeping every other byte (values, comments,
//! formatting) as the user left it, together with the explanatory comment
//! the older template wrote directly above it, when that comment is still
//! there unchanged. Only the listed settings are removed; every other unknown
//! field is still refused by the strict parse.
//!
//! Before the result is used, [`remove_settings`] checks it: the edited text
//! must parse to exactly the original document without the removed settings.
//! If the scanner cannot locate a setting safely, or the check fails, the
//! caller refuses to start and names the setting to delete by hand, leaving
//! the file untouched.

use serde_json::Value;

use super::completion::scan_layout;

/// One setting a past release wrote and this release no longer reads.
pub(super) struct RemovedSetting {
    /// The key chain from the document root, for example
    /// `["zenoh", "managed", "federation"]`.
    pub(super) path: &'static [&'static str],
    /// The comment lines the older template wrote directly above the key,
    /// trimmed. Removed with the setting only when present exactly as written.
    template_comment: &'static [&'static str],
}

impl RemovedSetting {
    pub(super) fn dotted_path(&self) -> String {
        self.path.join(".")
    }
}

/// Every setting removed from the document, in the order they are removed.
pub(super) const REMOVED_SETTINGS: &[RemovedSetting] = &[RemovedSetting {
    // `connect_timeout_secs` bounded a startup resolution of the per-user
    // router over HTTP, which the managed router no longer performs.
    path: &["zenoh", "managed", "federation"],
    template_comment: &[
        "// Per-user zenoh-router federation: how the daemon links its local router to",
        "// your private cloud router. Only tuned to bound a slow/unreachable backend",
        "// during the federation step.",
    ],
}];

/// The outcome of [`remove_settings`] on a document that carries at least
/// one removed setting.
pub(super) struct Removal {
    /// The document without the removed settings.
    pub(super) content: String,
    /// The dotted paths of the settings that were removed.
    pub(super) removed_paths: Vec<String>,
}

/// Why a removed setting could not be taken out of the text.
#[derive(Debug)]
pub(super) struct RemovalError {
    pub(super) path: String,
}

/// Returns `Ok(None)` when `content` carries none of [`REMOVED_SETTINGS`] (or
/// is not a JSON5 document at all, which the strict parse then reports), and
/// the edited document otherwise.
pub(super) fn remove_settings(content: &str) -> std::result::Result<Option<Removal>, RemovalError> {
    let Ok(original) = serde_json5::from_str::<Value>(content) else {
        return Ok(None);
    };
    let present: Vec<&RemovedSetting> = REMOVED_SETTINGS
        .iter()
        .filter(|setting| value_at(&original, setting.path).is_some())
        .collect();
    if present.is_empty() {
        return Ok(None);
    }

    let mut edited = content.to_string();
    let mut expected = original;
    for setting in &present {
        let error = || RemovalError {
            path: setting.dotted_path(),
        };
        edited = cut_entry(&edited, setting).ok_or_else(error)?;
        remove_value_at(&mut expected, setting.path).ok_or_else(error)?;
        let reparsed = serde_json5::from_str::<Value>(&edited).map_err(|_| error())?;
        if reparsed != expected {
            return Err(error());
        }
    }

    Ok(Some(Removal {
        content: edited,
        removed_paths: present
            .iter()
            .map(|setting| setting.dotted_path())
            .collect(),
    }))
}

fn value_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter()
        .try_fold(value, |current, key| current.as_object()?.get(*key))
}

fn remove_value_at(value: &mut Value, path: &[&str]) -> Option<Value> {
    let (last, parents) = path.split_last()?;
    let parent = parents
        .iter()
        .try_fold(value, |current, key| current.as_object_mut()?.get_mut(*key))?;
    parent.as_object_mut()?.remove(*last)
}

/// Cuts the entry `setting` names out of `content`: from the start of the line
/// that holds its key (or the key itself, when other text precedes it on that
/// line) through its closing brace, a following comma, and the rest of that
/// line when nothing but whitespace remains on it. The older template's
/// comment directly above the key goes with it when it is unchanged. Only an
/// object-valued entry is handled, which is what every removed setting is.
fn cut_entry(content: &str, setting: &RemovedSetting) -> Option<String> {
    let layout = scan_layout(content)?;
    let path: Vec<String> = setting.path.iter().map(|key| key.to_string()).collect();
    let span = layout.block_at(&path)?;
    let key_start = span.key_start?;

    let line_start = content[..key_start].rfind('\n').map_or(0, |i| i + 1);
    let mut start = if content[line_start..key_start].trim().is_empty() {
        line_start
    } else {
        key_start
    };
    if start == line_start {
        start = extend_over_template_comment(content, start, setting.template_comment);
    }

    let mut end = span.close + 1;
    let after_brace = &content[end..];
    let spaces = after_brace.len() - after_brace.trim_start_matches([' ', '\t']).len();
    if after_brace[spaces..].starts_with(',') {
        end += spaces + 1;
        if start == key_start {
            // Cut from mid-line: the separator after the comma goes too, so
            // the next entry takes the removed one's place.
            let after_comma = &content[end..];
            end += after_comma.len() - after_comma.trim_start_matches([' ', '\t']).len();
        }
    }
    let rest = &content[end..];
    let line_end = rest.find('\n').map_or(rest.len(), |i| i + 1);
    if rest[..line_end].trim().is_empty() {
        end += line_end;
    }

    let mut edited = String::with_capacity(content.len());
    edited.push_str(&content[..start]);
    edited.push_str(&content[end..]);
    Some(edited)
}

/// Moves `start` (the start of a line) up over the lines directly above it
/// when they are exactly `comment`, compared line by line after trimming.
fn extend_over_template_comment(content: &str, start: usize, comment: &[&str]) -> usize {
    if comment.is_empty() {
        return start;
    }
    let above: Vec<&str> = content[..start].split_inclusive('\n').collect();
    let Some(first) = above.len().checked_sub(comment.len()) else {
        return start;
    };
    let candidate = &above[first..];
    let matches = candidate
        .iter()
        .zip(comment)
        .all(|(line, expected)| line.trim() == *expected);
    if !matches {
        return start;
    }
    start - candidate.iter().map(|line| line.len()).sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The managed block as a release before the removal wrote it, with a
    /// user comment kept above `subscriber_buffers`.
    const OLD_DOCUMENT: &str = r#"{
  zenoh: {
    managed: {
      local_nodes_topology: "peer",

      // my own note: keep the buffers large
      subscriber_buffers: {
        standard_buffer_size: 64,
        high_throughput_buffer_size: 256,
      },

      // Per-user zenoh-router federation: how the daemon links its local router to
      // your private cloud router. Only tuned to bound a slow/unreachable backend
      // during the federation step.
      federation: {
        // Seconds the daemon spends resolving your per-user cloud router before
        // giving up for this attempt (it retries in the background). Bounds the
        // federation done at startup and on each `peppy platform login`/`logout`;
        // minimum 1. If the backend is unreachable within this window the daemon
        // stays standalone rather than blocking.
        connect_timeout_secs: 5,
      },
    },
  },
}
"#;

    #[test]
    fn the_federation_block_and_its_template_comment_are_cut_and_nothing_else() {
        let removal = remove_settings(OLD_DOCUMENT)
            .expect("the block is located")
            .expect("the block is present");
        assert_eq!(removal.removed_paths, ["zenoh.managed.federation"]);
        assert_eq!(
            removal.content,
            r#"{
  zenoh: {
    managed: {
      local_nodes_topology: "peer",

      // my own note: keep the buffers large
      subscriber_buffers: {
        standard_buffer_size: 64,
        high_throughput_buffer_size: 256,
      },

    },
  },
}
"#
        );
    }

    #[test]
    fn a_comment_the_user_changed_above_the_block_is_kept() {
        let content = r#"{ zenoh: { managed: {
      // federation: my own reminder
      federation: { connect_timeout_secs: 5 },
    } } }"#;
        let removal = remove_settings(content).unwrap().unwrap();
        assert!(removal.content.contains("// federation: my own reminder"));
        assert!(!removal.content.contains("connect_timeout_secs"));
    }

    #[test]
    fn a_block_on_one_line_with_other_entries_is_cut_alone() {
        let content = r#"{ zenoh: { managed: { federation: { connect_timeout_secs: 5 }, local_nodes_topology: "peer" } } }"#;
        let removal = remove_settings(content).unwrap().unwrap();
        assert_eq!(
            removal.content,
            r#"{ zenoh: { managed: { local_nodes_topology: "peer" } } }"#
        );
    }

    #[test]
    fn a_quoted_key_is_cut_too() {
        let content =
            r#"{ "zenoh": { "managed": { "federation": { "connect_timeout_secs": 5 } } } }"#;
        let removal = remove_settings(content).unwrap().unwrap();
        assert_eq!(removal.content, r#"{ "zenoh": { "managed": {  } } }"#);
    }

    #[test]
    fn a_document_without_removed_settings_is_left_alone() {
        for content in [
            r#"{ zenoh: { managed: { local_nodes_topology: "peer" } } }"#,
            // Same key under another parent is not the removed setting.
            r#"{ zenoh: { federation: { connect_timeout_secs: 5 } } }"#,
            "{ not json5",
        ] {
            assert!(remove_settings(content).unwrap().is_none(), "{content}");
        }
    }

    #[test]
    fn a_setting_that_is_not_an_object_cannot_be_cut_and_says_which() {
        let content = r#"{ zenoh: { managed: { federation: 5 } } }"#;
        let error = remove_settings(content)
            .err()
            .expect("only an object entry is cut");
        assert_eq!(error.path, "zenoh.managed.federation");
    }
}
