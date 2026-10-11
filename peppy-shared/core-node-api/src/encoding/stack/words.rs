//! The words of a launch and a join: the option a selection word picks, the
//! machine of the part it selects, the copy it belongs to, and the reference
//! to a copy.
//!
//! The CLI parses each type from its text with [`FromStr`]. The wire, the
//! daemon and the MCP bridge carry the types, and a refusal prints them with
//! [`Display`], whose text parses back to the same value. Each parser trims
//! the whole value once, then splits it in one order: at `@` first (the
//! machine), then at `.` (the copy of a launch word), then at `=` (the
//! axis), then at `:` (the name of a copy reference). Axis, option, copy and
//! machine names hold none of these characters.

use std::fmt;
use std::str::FromStr;

use config::runtime::{
    CopyName, CopyNameError, CoreNodeName, CoreNodeNameError, CoreNodeTarget, Name,
};

use crate::encoding::{read_core_node_name, read_name};
use crate::{Result, launch_capnp, node_capnp};

/// The text of a selection word, quoted in its refusals.
const SELECTION_FORM: &str = "`option` or `axis=option`, followed by `@CORE_NODE` to run the \
                              part it selects on that machine";

/// The text of a launch word, quoted in its refusals.
const LAUNCH_FORM: &str = "`option` or `axis=option` for an axis of the stack, or \
                           `NAME.option` or `NAME.axis=option` for an own axis of the copy NAME, \
                           a copy's word followed by `@CORE_NODE` to run the part it selects on \
                           that machine";

/// The text of a copy reference, quoted in its refusals.
const COPY_FORM: &str = "`OPTION:NAME`, the option of an axis that runs as copies and the name \
                         the copy runs under, followed by `@CORE_NODE` to run the copy on that \
                         machine";

/// Why a text is not a word.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WordError {
    #[error("a word is blank (check for a stray comma); write {form}")]
    Blank { form: &'static str },
    #[error("`{word}` names more than one machine; write one `@CORE_NODE`")]
    TwoMachines { word: String },
    #[error("`{word}` names no machine after `@`; write `@CORE_NODE`, or drop the `@`")]
    NoMachine { word: String },
    #[error("machine `{machine}` in `{word}` {reason}")]
    BadMachine {
        word: String,
        machine: String,
        reason: CoreNodeNameError,
    },
    #[error("`{word}` names no {part}; write {form}")]
    MissingPart {
        word: String,
        part: &'static str,
        form: &'static str,
    },
    #[error("{part} `{value}` in `{word}`: {reason}")]
    BadName {
        word: String,
        part: &'static str,
        value: String,
        reason: String,
    },
    #[error("`{word}`: {reason}")]
    BadCopyName { word: String, reason: CopyNameError },
    #[error("`{word}` names no copy; write {COPY_FORM}")]
    NoCopy { word: String },
    #[error(
        "`{word}` places an option of the stack, and a stack instance runs where its \
         `core_node` link says: `--place LINK@CORE_NODE`"
    )]
    PlacedStackWord { word: String },
    #[error(
        "`{word}`: a join's words name options of the copied option's own axes, as `option` or \
         `axis=option`; `NAME.option` selects an own axis of a copy on `peppy stack launch`"
    )]
    ScopedJoinWord { word: String },
}

/// `option` or `axis=option`: one option of one component axis. With no
/// axis, the coordinator finds the axis that holds the option.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OptionChoice {
    pub axis: Option<Name>,
    pub option: Name,
}

/// `CHOICE[@CORE_NODE]`: one option of an own axis of a copy, and the
/// machine of the part it selects. With no machine, the part runs with the
/// copy's base.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SelectionWord {
    pub choice: OptionChoice,
    pub core_node: Option<CoreNodeTarget>,
}

/// `[NAME.]CHOICE[@CORE_NODE]`: one `--with` word of a launch. A stack word
/// selects an option of a stack axis, whose instances run where their
/// `core_node` link says, so it holds no machine; a copy word is a
/// selection word of the copy NAME.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LaunchWord {
    Stack(OptionChoice),
    Copy { copy: CopyName, word: SelectionWord },
}

/// One filled own axis of a running copy, with the machine of the part it
/// selects when that part runs away from the copy's machine. It displays
/// as the [`SelectionWord`] that adds it again: `axis=option[@machine]`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AxisSelection {
    pub axis: Name,
    pub option: Name,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core_node: Option<CoreNodeName>,
}

/// `OPTION:NAME[@CORE_NODE]`: the copy of `option` that runs under `name`,
/// its base on `core_node`, the coordinator when no machine is given.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CopyReference {
    pub option: Name,
    pub name: CopyName,
    pub core_node: CoreNodeTarget,
}

impl From<&AxisSelection> for SelectionWord {
    fn from(selection: &AxisSelection) -> Self {
        Self {
            choice: OptionChoice {
                axis: Some(selection.axis.clone()),
                option: selection.option.clone(),
            },
            core_node: selection.core_node.clone().map(CoreNodeTarget::Named),
        }
    }
}

impl SelectionWord {
    /// The word with no machine: its part runs with the copy's base.
    pub fn with_copy(choice: OptionChoice) -> Self {
        Self {
            choice,
            core_node: None,
        }
    }
}

impl LaunchWord {
    /// The machine the word names, if any.
    pub fn core_node(&self) -> Option<&CoreNodeTarget> {
        match self {
            Self::Stack(_) => None,
            Self::Copy { word, .. } => word.core_node.as_ref(),
        }
    }
}

/// The trimmed text of a word, refused when blank.
fn trimmed<'a>(raw: &'a str, form: &'static str) -> std::result::Result<&'a str, WordError> {
    let word = raw.trim();
    if word.is_empty() {
        return Err(WordError::Blank { form });
    }
    Ok(word)
}

/// Splits `word` at its `@`: the text before it, and the machine after it.
fn split_machine(word: &str) -> std::result::Result<(&str, Option<CoreNodeTarget>), WordError> {
    let Some((head, machine)) = word.split_once('@') else {
        return Ok((word, None));
    };
    if machine.contains('@') {
        return Err(WordError::TwoMachines {
            word: word.to_owned(),
        });
    }
    if machine.is_empty() {
        return Err(WordError::NoMachine {
            word: word.to_owned(),
        });
    }
    let target = CoreNodeTarget::parse(machine).map_err(|reason| WordError::BadMachine {
        word: word.to_owned(),
        machine: machine.to_owned(),
        reason,
    })?;
    Ok((head, Some(target)))
}

/// One name of a word: `value`, refused when blank (naming the missing
/// `part`) or when it is not a name (naming it as `label`).
fn name_part(
    word: &str,
    value: &str,
    part: &'static str,
    label: &'static str,
    form: &'static str,
) -> std::result::Result<Name, WordError> {
    if value.is_empty() {
        return Err(WordError::MissingPart {
            word: word.to_owned(),
            part,
            form,
        });
    }
    Name::new(value).map_err(|reason| WordError::BadName {
        word: word.to_owned(),
        part: label,
        value: value.to_owned(),
        reason: reason.to_string(),
    })
}

/// The choice `head` writes, `option` or `axis=option`, of the whole word
/// `word`.
fn parse_choice(
    word: &str,
    head: &str,
    form: &'static str,
) -> std::result::Result<OptionChoice, WordError> {
    match head.split_once('=') {
        Some((axis, option)) => Ok(OptionChoice {
            axis: Some(name_part(word, axis, "axis before `=`", "axis", form)?),
            option: name_part(word, option, "option after `=`", "option", form)?,
        }),
        None => Ok(OptionChoice {
            axis: None,
            option: name_part(word, head, "option", "option", form)?,
        }),
    }
}

impl FromStr for OptionChoice {
    type Err = WordError;

    fn from_str(raw: &str) -> std::result::Result<Self, Self::Err> {
        const FORM: &str = "`option` or `axis=option`";
        let word = trimmed(raw, FORM)?;
        parse_choice(word, word, FORM)
    }
}

impl FromStr for SelectionWord {
    type Err = WordError;

    fn from_str(raw: &str) -> std::result::Result<Self, Self::Err> {
        let word = trimmed(raw, SELECTION_FORM)?;
        let (head, core_node) = split_machine(word)?;
        if head.contains('.') {
            return Err(WordError::ScopedJoinWord {
                word: word.to_owned(),
            });
        }
        Ok(Self {
            choice: parse_choice(word, head, SELECTION_FORM)?,
            core_node,
        })
    }
}

impl FromStr for LaunchWord {
    type Err = WordError;

    fn from_str(raw: &str) -> std::result::Result<Self, Self::Err> {
        let word = trimmed(raw, LAUNCH_FORM)?;
        let (head, core_node) = split_machine(word)?;
        let Some((copy, rest)) = head.split_once('.') else {
            if core_node.is_some() {
                return Err(WordError::PlacedStackWord {
                    word: word.to_owned(),
                });
            }
            return Ok(Self::Stack(parse_choice(word, head, LAUNCH_FORM)?));
        };
        if copy.is_empty() {
            return Err(WordError::MissingPart {
                word: word.to_owned(),
                part: "copy before `.`",
                form: LAUNCH_FORM,
            });
        }
        let copy = CopyName::new(copy).map_err(|reason| WordError::BadCopyName {
            word: word.to_owned(),
            reason,
        })?;
        Ok(Self::Copy {
            copy,
            word: SelectionWord {
                choice: parse_choice(word, rest, LAUNCH_FORM)?,
                core_node,
            },
        })
    }
}

impl FromStr for AxisSelection {
    type Err = WordError;

    fn from_str(raw: &str) -> std::result::Result<Self, Self::Err> {
        const FORM: &str = "`axis=option`, followed by `@CORE_NODE` when the part runs on \
                            another machine than the copy";
        let word = trimmed(raw, FORM)?;
        let (head, core_node) = split_machine(word)?;
        let Some((axis, option)) = head.split_once('=') else {
            return Err(WordError::MissingPart {
                word: word.to_owned(),
                part: "axis before `=`",
                form: FORM,
            });
        };
        let core_node = match core_node {
            None => None,
            Some(CoreNodeTarget::Named(machine)) => Some(machine),
            Some(CoreNodeTarget::Coordinator) => {
                return Err(WordError::BadMachine {
                    word: word.to_owned(),
                    machine: config::runtime::SELF_CORE_NODE.to_owned(),
                    reason: CoreNodeNameError::Reserved,
                });
            }
        };
        Ok(Self {
            axis: name_part(word, axis, "axis before `=`", "axis", FORM)?,
            option: name_part(word, option, "option after `=`", "option", FORM)?,
            core_node,
        })
    }
}

impl FromStr for CopyReference {
    type Err = WordError;

    fn from_str(raw: &str) -> std::result::Result<Self, Self::Err> {
        let word = trimmed(raw, COPY_FORM)?;
        let (head, core_node) = split_machine(word)?;
        let Some((option, name)) = head.split_once(':') else {
            return Err(WordError::NoCopy {
                word: word.to_owned(),
            });
        };
        let option = name_part(word, option, "option before `:`", "option", COPY_FORM)?;
        if name.is_empty() {
            return Err(WordError::MissingPart {
                word: word.to_owned(),
                part: "copy name after `:`",
                form: COPY_FORM,
            });
        }
        let name = CopyName::new(name).map_err(|reason| WordError::BadCopyName {
            word: word.to_owned(),
            reason,
        })?;
        Ok(Self {
            option,
            name,
            core_node: core_node.unwrap_or(CoreNodeTarget::Coordinator),
        })
    }
}

impl fmt::Display for OptionChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.axis {
            Some(axis) => write!(f, "{axis}={}", self.option),
            None => write!(f, "{}", self.option),
        }
    }
}

impl fmt::Display for SelectionWord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.choice)?;
        match &self.core_node {
            Some(target) => write!(f, "@{target}"),
            None => Ok(()),
        }
    }
}

impl fmt::Display for LaunchWord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stack(choice) => write!(f, "{choice}"),
            Self::Copy { copy, word } => write!(f, "{copy}.{word}"),
        }
    }
}

impl fmt::Display for AxisSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", SelectionWord::from(self))
    }
}

impl fmt::Display for CopyReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.option, self.name)?;
        match &self.core_node {
            CoreNodeTarget::Coordinator => Ok(()),
            CoreNodeTarget::Named(machine) => write!(f, "@{machine}"),
        }
    }
}

/// A decode refusal of a struct pointer the message leaves null.
fn missing(field: &str) -> crate::Error {
    crate::Error::Decoding(format!("`{field}` is missing"))
}

/// A text field that must be a [`CopyName`]; the refusal names the field.
pub(crate) fn read_copy_name(value: &str, field: &str) -> Result<CopyName> {
    CopyName::new(value).map_err(|error| crate::Error::Decoding(format!("`{field}`: {error}")))
}

pub(crate) fn write_core_node_target(
    mut wire: launch_capnp::core_node_target::Builder<'_>,
    target: &CoreNodeTarget,
) {
    match target {
        CoreNodeTarget::Coordinator => wire.set_coordinator(()),
        CoreNodeTarget::Named(machine) => wire.set_named(machine.as_str()),
    }
}

pub(crate) fn read_core_node_target(
    wire: launch_capnp::core_node_target::Reader<'_>,
    field: &str,
) -> Result<CoreNodeTarget> {
    use launch_capnp::core_node_target::Which;
    match wire.which()? {
        Which::Coordinator(()) => Ok(CoreNodeTarget::Coordinator),
        Which::Named(machine) => read_core_node_name(machine?.to_str()?, &format!("{field}.named"))
            .map(CoreNodeTarget::Named),
    }
}

pub(crate) fn write_option_choice(
    mut wire: launch_capnp::option_choice::Builder<'_>,
    choice: &OptionChoice,
) {
    match &choice.axis {
        Some(axis) => wire.reborrow().init_axis().set_named(axis.as_str()),
        None => wire.reborrow().init_axis().set_unnamed(()),
    }
    wire.set_option(choice.option.as_str());
}

pub(crate) fn read_option_choice(
    wire: launch_capnp::option_choice::Reader<'_>,
    field: &str,
) -> Result<OptionChoice> {
    use launch_capnp::option_choice::axis::Which;
    let axis = match wire.get_axis().which()? {
        Which::Unnamed(()) => None,
        Which::Named(axis) => Some(read_name(axis?.to_str()?, &format!("{field}.axis"))?),
    };
    Ok(OptionChoice {
        axis,
        option: read_name(wire.get_option()?.to_str()?, &format!("{field}.option"))?,
    })
}

pub(crate) fn write_selection_word(
    mut wire: launch_capnp::selection_word::Builder<'_>,
    word: &SelectionWord,
) {
    write_option_choice(wire.reborrow().init_choice(), &word.choice);
    match &word.core_node {
        None => wire.init_core_node().set_with_copy(()),
        Some(target) => write_core_node_target(wire.init_core_node().init_target(), target),
    }
}

pub(crate) fn read_selection_word(
    wire: launch_capnp::selection_word::Reader<'_>,
    field: &str,
) -> Result<SelectionWord> {
    use launch_capnp::selection_word::core_node::Which;
    if !wire.has_choice() {
        return Err(missing(&format!("{field}.choice")));
    }
    let choice = read_option_choice(wire.get_choice()?, &format!("{field}.choice"))?;
    let core_node = match wire.get_core_node().which()? {
        Which::WithCopy(()) => None,
        Which::Target(target) => {
            if !wire.get_core_node().has_target() {
                return Err(missing(&format!("{field}.core_node.target")));
            }
            Some(read_core_node_target(
                target?,
                &format!("{field}.core_node.target"),
            )?)
        }
    };
    Ok(SelectionWord { choice, core_node })
}

pub(crate) fn write_launch_word(wire: launch_capnp::launch_word::Builder<'_>, word: &LaunchWord) {
    match word {
        LaunchWord::Stack(choice) => write_option_choice(wire.init_stack(), choice),
        LaunchWord::Copy { copy, word } => {
            let mut wire = wire.init_copy();
            wire.set_copy(copy.as_str());
            write_selection_word(wire.init_word(), word);
        }
    }
}

pub(crate) fn read_launch_word(
    wire: launch_capnp::launch_word::Reader<'_>,
    field: &str,
) -> Result<LaunchWord> {
    use launch_capnp::launch_word::Which;
    match wire.which()? {
        Which::Stack(choice) => {
            if !wire.has_stack() {
                return Err(missing(&format!("{field}.stack")));
            }
            Ok(LaunchWord::Stack(read_option_choice(
                choice?,
                &format!("{field}.stack"),
            )?))
        }
        Which::Copy(copy) => {
            if !wire.has_copy() {
                return Err(missing(&format!("{field}.copy")));
            }
            let copy = copy?;
            if !copy.has_word() {
                return Err(missing(&format!("{field}.copy.word")));
            }
            Ok(LaunchWord::Copy {
                copy: read_copy_name(copy.get_copy()?.to_str()?, &format!("{field}.copy.copy"))?,
                word: read_selection_word(copy.get_word()?, &format!("{field}.copy.word"))?,
            })
        }
    }
}

pub(crate) fn write_copy_reference(
    mut wire: launch_capnp::copy_reference::Builder<'_>,
    copy: &CopyReference,
) {
    wire.set_option(copy.option.as_str());
    wire.set_name(copy.name.as_str());
    write_core_node_target(wire.init_core_node(), &copy.core_node);
}

pub(crate) fn read_copy_reference(
    wire: launch_capnp::copy_reference::Reader<'_>,
    field: &str,
) -> Result<CopyReference> {
    if !wire.has_core_node() {
        return Err(missing(&format!("{field}.core_node")));
    }
    Ok(CopyReference {
        option: read_name(wire.get_option()?.to_str()?, &format!("{field}.option"))?,
        name: read_copy_name(wire.get_name()?.to_str()?, &format!("{field}.name"))?,
        core_node: read_core_node_target(wire.get_core_node()?, &format!("{field}.core_node"))?,
    })
}

pub(crate) fn write_axis_selection(
    mut wire: node_capnp::axis_selection::Builder<'_>,
    selection: &AxisSelection,
) {
    wire.set_axis(selection.axis.as_str());
    wire.set_option(selection.option.as_str());
    match &selection.core_node {
        None => wire.init_core_node().set_with_copy(()),
        Some(machine) => wire.init_core_node().set_named(machine.as_str()),
    }
}

pub(crate) fn read_axis_selection(
    wire: node_capnp::axis_selection::Reader<'_>,
    field: &str,
) -> Result<AxisSelection> {
    use node_capnp::axis_selection::core_node::Which;
    let core_node = match wire.get_core_node().which()? {
        Which::WithCopy(()) => None,
        Which::Named(machine) => Some(read_core_node_name(
            machine?.to_str()?,
            &format!("{field}.core_node.named"),
        )?),
    };
    Ok(AxisSelection {
        axis: read_name(wire.get_axis()?.to_str()?, &format!("{field}.axis"))?,
        option: read_name(wire.get_option()?.to_str()?, &format!("{field}.option"))?,
        core_node,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::{decode_message, encode_message};
    use capnp::message::Builder;

    fn name(value: &str) -> Name {
        Name::new(value).unwrap()
    }

    fn machine(value: &str) -> CoreNodeTarget {
        CoreNodeTarget::Named(CoreNodeName::new(value).unwrap())
    }

    fn choice(axis: Option<&str>, option: &str) -> OptionChoice {
        OptionChoice {
            axis: axis.map(name),
            option: name(option),
        }
    }

    /// Each form parses into its value, and the value displays as text that
    /// parses back to it.
    #[test]
    fn each_word_parses_and_displays_its_form() {
        assert_eq!(
            " ai_brain_vla ".parse::<OptionChoice>().unwrap(),
            choice(None, "ai_brain_vla")
        );
        assert_eq!(
            "brain=ai_brain_vla".parse::<OptionChoice>().unwrap(),
            choice(Some("brain"), "ai_brain_vla")
        );
        for (text, expected) in [
            (
                "ai_brain_vla@cn-gpu-box",
                SelectionWord {
                    choice: choice(None, "ai_brain_vla"),
                    core_node: Some(machine("cn-gpu-box")),
                },
            ),
            (
                "brain=ai_brain_vla@cn-gpu-box",
                SelectionWord {
                    choice: choice(Some("brain"), "ai_brain_vla"),
                    core_node: Some(machine("cn-gpu-box")),
                },
            ),
            (
                "ai_brain_vla@self",
                SelectionWord {
                    choice: choice(None, "ai_brain_vla"),
                    core_node: Some(CoreNodeTarget::Coordinator),
                },
            ),
            (
                "cameras_sim",
                SelectionWord::with_copy(choice(None, "cameras_sim")),
            ),
        ] {
            let word: SelectionWord = text.parse().unwrap();
            assert_eq!(word, expected, "{text}");
            assert_eq!(word.to_string(), text);
        }
        for (text, expected) in [
            (
                "xiaozhu.ai_brain_vla",
                LaunchWord::Copy {
                    copy: CopyName::new("xiaozhu").unwrap(),
                    word: SelectionWord::with_copy(choice(None, "ai_brain_vla")),
                },
            ),
            (
                "xiaozhu.brain=ai_brain_vla@cn-gpu-box",
                LaunchWord::Copy {
                    copy: CopyName::new("xiaozhu").unwrap(),
                    word: SelectionWord {
                        choice: choice(Some("brain"), "ai_brain_vla"),
                        core_node: Some(machine("cn-gpu-box")),
                    },
                },
            ),
            (
                "simulation=mujoco",
                LaunchWord::Stack(choice(Some("simulation"), "mujoco")),
            ),
            ("mujoco", LaunchWord::Stack(choice(None, "mujoco"))),
        ] {
            let word: LaunchWord = text.parse().unwrap();
            assert_eq!(word, expected, "{text}");
            assert_eq!(word.to_string(), text);
        }
        for (text, core_node) in [
            ("openarm_sim:delta", CoreNodeTarget::Coordinator),
            ("openarm_sim:delta@cn-jetson-1", machine("cn-jetson-1")),
        ] {
            let copy: CopyReference = text.parse().unwrap();
            assert_eq!(
                copy,
                CopyReference {
                    option: name("openarm_sim"),
                    name: CopyName::new("delta").unwrap(),
                    core_node,
                },
                "{text}"
            );
            assert_eq!(copy.to_string(), text);
        }
        let on_self: CopyReference = "openarm_sim:delta@self".parse().unwrap();
        assert_eq!(on_self.core_node, CoreNodeTarget::Coordinator);
    }

    /// An axis selection displays as the selection word that adds it again.
    #[test]
    fn an_axis_selection_displays_as_the_word_that_adds_it_again() {
        let placed = AxisSelection {
            axis: name("brain"),
            option: name("ai_brain_vla"),
            core_node: Some(CoreNodeName::new("cn-gpu-box").unwrap()),
        };
        assert_eq!(placed.to_string(), "brain=ai_brain_vla@cn-gpu-box");
        assert_eq!(
            SelectionWord::from(&placed),
            "brain=ai_brain_vla@cn-gpu-box".parse().unwrap()
        );
        assert_eq!(
            "brain=ai_brain_vla@cn-gpu-box"
                .parse::<AxisSelection>()
                .unwrap(),
            placed
        );
        let with_copy = AxisSelection {
            core_node: None,
            ..placed
        };
        assert_eq!(with_copy.to_string(), "brain=ai_brain_vla");
        assert_eq!(
            "brain=ai_brain_vla".parse::<AxisSelection>().unwrap(),
            with_copy
        );
        assert!(matches!(
            "ai_brain_vla".parse::<AxisSelection>(),
            Err(WordError::MissingPart { .. })
        ));
        assert!(matches!(
            "brain=ai_brain_vla@self".parse::<AxisSelection>(),
            Err(WordError::BadMachine { .. })
        ));
        assert_eq!(
            serde_json::to_value(&with_copy).unwrap(),
            serde_json::json!({ "axis": "brain", "option": "ai_brain_vla" })
        );
    }

    /// Each half of a word has its own refusal.
    #[test]
    fn each_refusal_names_what_is_wrong() {
        let refusal = |result: std::result::Result<(), WordError>| result.unwrap_err().to_string();
        let selection = |text: &str| refusal(text.parse::<SelectionWord>().map(|_| ()));
        let launch = |text: &str| refusal(text.parse::<LaunchWord>().map(|_| ()));
        let copy = |text: &str| refusal(text.parse::<CopyReference>().map(|_| ()));

        assert!(selection(" ").contains("blank"));
        assert!(selection("a@b@c").contains("more than one machine"));
        assert!(selection("option@").contains("no machine after `@`"));
        let bad_machine = selection("option@bad/name");
        assert!(
            bad_machine.starts_with("machine `bad/name` in `option@bad/name` must be non-empty"),
            "{bad_machine}"
        );
        assert!(selection("@cn-gpu-box").contains("names no option"));
        assert!(selection("=ai_brain_vla").contains("names no axis before `=`"));
        assert!(selection("brain=").contains("names no option after `=`"));
        assert!(selection("bad/option").contains("option `bad/option` in `bad/option`"));
        let scoped = selection("alpha.option");
        assert!(scoped.contains("`peppy stack launch`"), "{scoped}");

        assert!(launch("mujoco@cn-gpu-box").contains("--place LINK@CORE_NODE"));
        assert!(launch(".option").contains("names no copy before `.`"));
        assert!(launch("alpha.").contains("names no option"));
        assert!(launch("self.option").contains("`self` names the daemon"));
        assert!(launch(&format!("{}.option", "n".repeat(64))).contains("placement link"));

        assert!(copy("openarm_sim").contains("names no copy"));
        assert!(copy("openarm_sim:delta@").contains("no machine after `@`"));
        assert!(copy("openarm_sim:@x").contains("names no copy name after `:`"));
        assert!(copy(":delta").contains("names no option before `:`"));
        assert!(copy("openarm_sim:self").contains("`self` names the daemon"));
        assert!(copy("openarm_sim:delta@a@b").contains("more than one machine"));
    }

    /// `copy` written into a fresh message and read back.
    fn copy_round_trip(copy: &CopyReference) -> CopyReference {
        let mut message = Builder::new_default();
        write_copy_reference(message.init_root(), copy);
        let bytes = encode_message(&message).unwrap();
        let decoded = decode_message(&bytes).unwrap();
        read_copy_reference(decoded.get_root().unwrap(), "copy").unwrap()
    }

    #[test]
    fn copy_references_round_trip_and_refuse_a_null_machine() {
        for text in ["openarm_sim:delta", "openarm_sim:delta@cn-jetson-1"] {
            let copy: CopyReference = text.parse().unwrap();
            assert_eq!(copy_round_trip(&copy), copy);
        }
        let mut message = Builder::new_default();
        let mut wire = message.init_root::<launch_capnp::copy_reference::Builder>();
        wire.set_option("openarm_sim");
        wire.set_name("delta");
        let bytes = encode_message(&message).unwrap();
        let decoded = decode_message(&bytes).unwrap();
        let refusal = read_copy_reference(decoded.get_root().unwrap(), "copy").unwrap_err();
        assert!(
            refusal.to_string().contains("`copy.core_node` is missing"),
            "{refusal}"
        );

        let mut message = Builder::new_default();
        let mut wire = message.init_root::<launch_capnp::copy_reference::Builder>();
        wire.set_option("openarm_sim");
        wire.set_name("self");
        wire.init_core_node().set_coordinator(());
        let bytes = encode_message(&message).unwrap();
        let decoded = decode_message(&bytes).unwrap();
        let refusal = read_copy_reference(decoded.get_root().unwrap(), "copy").unwrap_err();
        assert!(refusal.to_string().contains("`copy.name`"), "{refusal}");
    }

    #[test]
    fn launch_words_round_trip_and_refuse_null_structs() {
        for text in [
            "mujoco",
            "simulation=mujoco",
            "xiaozhu.ai_brain_vla",
            "xiaozhu.brain=ai_brain_vla@cn-gpu-box",
            "xiaozhu.cameras_sim@self",
        ] {
            let word: LaunchWord = text.parse().unwrap();
            let mut message = Builder::new_default();
            write_launch_word(message.init_root(), &word);
            let bytes = encode_message(&message).unwrap();
            let decoded = decode_message(&bytes).unwrap();
            assert_eq!(
                read_launch_word(decoded.get_root().unwrap(), "word").unwrap(),
                word,
                "{text}"
            );
        }

        let mut message = Builder::new_default();
        message
            .init_root::<launch_capnp::launch_word::Builder>()
            .init_copy()
            .set_copy("xiaozhu");
        let bytes = encode_message(&message).unwrap();
        let decoded = decode_message(&bytes).unwrap();
        let refusal = read_launch_word(decoded.get_root().unwrap(), "word").unwrap_err();
        assert!(
            refusal.to_string().contains("`word.copy.word` is missing"),
            "{refusal}"
        );

        let mut message = Builder::new_default();
        message.init_root::<launch_capnp::selection_word::Builder>();
        let bytes = encode_message(&message).unwrap();
        let decoded = decode_message(&bytes).unwrap();
        let refusal = read_selection_word(decoded.get_root().unwrap(), "word").unwrap_err();
        assert!(
            refusal.to_string().contains("`word.choice` is missing"),
            "{refusal}"
        );

        let mut message = Builder::new_default();
        let mut word = message.init_root::<launch_capnp::option_choice::Builder>();
        word.reborrow().init_axis().set_named("brain");
        word.set_option("bad/option");
        let bytes = encode_message(&message).unwrap();
        let decoded = decode_message(&bytes).unwrap();
        let refusal = read_option_choice(decoded.get_root().unwrap(), "word").unwrap_err();
        assert!(refusal.to_string().contains("`word.option`"), "{refusal}");
    }

    #[test]
    fn axis_selections_round_trip_and_refuse_self() {
        for text in ["brain=ai_brain_vla", "brain=ai_brain_vla@cn-gpu-box"] {
            let selection: AxisSelection = text.parse().unwrap();
            let mut message = Builder::new_default();
            write_axis_selection(message.init_root(), &selection);
            let bytes = encode_message(&message).unwrap();
            let decoded = decode_message(&bytes).unwrap();
            assert_eq!(
                read_axis_selection(decoded.get_root().unwrap(), "selection").unwrap(),
                selection
            );
        }
        let mut message = Builder::new_default();
        let mut wire = message.init_root::<node_capnp::axis_selection::Builder>();
        wire.set_axis("brain");
        wire.set_option("ai_brain_vla");
        wire.init_core_node().set_named("self");
        let bytes = encode_message(&message).unwrap();
        let decoded = decode_message(&bytes).unwrap();
        let refusal = read_axis_selection(decoded.get_root().unwrap(), "selection").unwrap_err();
        assert!(
            refusal.to_string().contains("`selection.core_node.named`"),
            "{refusal}"
        );
    }
}
