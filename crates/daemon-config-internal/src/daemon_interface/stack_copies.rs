//! `stack_copies:v1`: the copies of the running launch, listed, added and
//! removed. Its scope names the launcher options a client may add, with a
//! description of each, and the most copies of them the stack holds.

use crate::internal::launcher::{CompositionError, PreparedLauncher};
use config::runtime::{CoreNodeName, Name};
use peppy_mcp_catalog::ExposureBundle;
use serde::{Deserialize, Deserializer, de};
use serde_json::Value;
use std::collections::HashSet;
use std::num::NonZeroU16;

/// The interface's document, compiled in.
pub(super) const DOCUMENT: &str = include_str!("stack_copies.v1.json5");

/// A member of `stack_copies:v1`, as an exposure entry names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StackCopiesMember {
    /// The service that answers the scope's options and the copies of them
    /// on the stack.
    List,
    /// The action that adds a copy: its goal names the copy and its option.
    Join,
    /// The action that removes a copy: its goal names the copy.
    Remove,
}

impl StackCopiesMember {
    /// Every member of the interface.
    pub const ALL: [Self; 3] = [Self::List, Self::Join, Self::Remove];

    /// The member's name in the interface's document.
    pub fn name(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Join => "join",
            Self::Remove => "remove",
        }
    }

    /// The member the document names `name`, if any.
    pub fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|member| member.name() == name)
    }
}

/// The most characters the description of a scoped option holds.
pub const MAX_DESCRIPTION_CHARS: usize = 200;

/// The scope of a `stack_copies` target: the launcher options a client may
/// add, each once and with a description, and the most copies of these
/// options the stack holds. Parsing holds every rule, so a value of this
/// type is a scope the server can use.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawStackCopiesScope")]
pub struct StackCopiesScope {
    options: Vec<ScopedOption>,
    max_copies: NonZeroU16,
}

/// One launcher option a client may add a copy of.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedOption {
    /// The option of an axis that runs as copies: the value `join` takes.
    pub option: Name,
    /// One line of at most [`MAX_DESCRIPTION_CHARS`] characters that says
    /// what a copy of the option is.
    #[serde(deserialize_with = "deserialize_description")]
    pub description: String,
}

/// Wire shape of [`StackCopiesScope`]. `max_copies` is read as any number
/// so that a value outside its range is refused with the range.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStackCopiesScope {
    options: Vec<ScopedOption>,
    max_copies: serde_json::Number,
}

impl TryFrom<RawStackCopiesScope> for StackCopiesScope {
    type Error = String;

    fn try_from(raw: RawStackCopiesScope) -> Result<Self, String> {
        if raw.options.is_empty() {
            return Err(
                "`options` names no option; a scope names at least one launcher option a client \
                 may add"
                    .to_owned(),
            );
        }
        let mut seen = HashSet::with_capacity(raw.options.len());
        for entry in &raw.options {
            if !seen.insert(entry.option.as_str()) {
                return Err(format!(
                    "`options` names option `{}` more than once",
                    entry.option
                ));
            }
        }
        let max_copies = raw
            .max_copies
            .as_u64()
            .and_then(|value| u16::try_from(value).ok())
            .and_then(NonZeroU16::new)
            .ok_or_else(|| {
                format!(
                    "`max_copies` is a whole number from 1 to {}, got {}",
                    u16::MAX,
                    raw.max_copies
                )
            })?;
        Ok(Self {
            options: raw.options,
            max_copies,
        })
    }
}

fn deserialize_description<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let description = String::deserialize(deserializer)?;
    if description.trim().is_empty() {
        return Err(de::Error::custom(
            "a description says what a copy of the option is, and this one is empty",
        ));
    }
    if description.chars().any(char::is_control) {
        return Err(de::Error::custom(
            "a description is one line, with no line break or other control character",
        ));
    }
    let length = description.chars().count();
    if length > MAX_DESCRIPTION_CHARS {
        return Err(de::Error::custom(format!(
            "a description is at most {MAX_DESCRIPTION_CHARS} characters, and this one has \
             {length}"
        )));
    }
    Ok(description)
}

impl StackCopiesScope {
    /// The options a client may add, in the order the launcher lists them.
    pub fn options(&self) -> &[ScopedOption] {
        &self.options
    }

    /// The most copies of the scope's options the stack holds.
    pub fn max_copies(&self) -> NonZeroU16 {
        self.max_copies
    }

    /// Every scoped option is an option of an axis that runs as copies:
    /// one refusal per option that is not, with the menu of the options a
    /// join can add.
    pub(super) fn check_launch(&self, prepared: &PreparedLauncher) -> Vec<String> {
        self.options
            .iter()
            .filter_map(|entry| {
                let error = prepared.repeatable_axis_of(entry.option.as_str()).err()?;
                Some(match error {
                    // The join's own refusal names the option and the menu.
                    CompositionError::JoinUnknownOption { .. } => error.to_string(),
                    _ => format!("option `{}`: {error}", entry.option),
                })
            })
            .collect()
    }

    /// `join.option` takes only the scope's options, and `join.name` and
    /// `remove.name` only a core node name: the name rule the daemon and the
    /// CLI apply to a copy.
    pub(super) fn narrow(&self, bundle: &mut ExposureBundle, target: &str) {
        let copy_name = CoreNodeName::json_schema();
        let options: Vec<&str> = self
            .options
            .iter()
            .map(|entry| entry.option.as_str())
            .collect();
        for task in bundle.tasks.iter_mut().filter(|task| task.target == target) {
            let Some(properties) = task
                .input_schema
                .get_mut("properties")
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            match StackCopiesMember::named(&task.member) {
                Some(StackCopiesMember::Join) => {
                    properties.insert(
                        "option".to_owned(),
                        serde_json::json!({ "type": "string", "enum": options }),
                    );
                    properties.insert("name".to_owned(), copy_name.clone());
                }
                Some(StackCopiesMember::Remove) => {
                    properties.insert("name".to_owned(), copy_name.clone());
                }
                Some(StackCopiesMember::List) | None => {}
            }
        }
    }
}
