//! `stack join`: one more copy of an option onto the running stack.

use capnp::message::Builder;
use config::AnyType;
use config::runtime::{Name, first_duplicate};

use crate::encoding::{capnp_list_len, decode_message, encode_message, read_name};
use crate::{Payload, Result, launch_capnp};

use super::budgets::StackBudgets;
use super::words::{
    CopyReference, SelectionWord, read_copy_reference, read_selection_word, write_copy_reference,
    write_selection_word,
};

/// The shape of an override, quoted in every refusal so the message says what
/// to type.
const OVERRIDE_USAGE: &str = "an override is written INSTANCE.ARGUMENT=JSON5";

/// A parsed `INSTANCE.ARGUMENT=JSON5` override of one argument of one of the
/// copy's instances, holding only finite numbers.
#[derive(Debug, Clone, PartialEq)]
pub struct ArgumentOverride {
    instance_id: Name,
    argument: Name,
    value: AnyType,
}

impl ArgumentOverride {
    pub fn new(
        instance_id: Name,
        argument: Name,
        value: AnyType,
    ) -> std::result::Result<Self, ArgumentOverrideError> {
        if !has_only_finite_numbers(&value) {
            return Err(ArgumentOverrideError::NonFinite {
                instance_id,
                argument,
            });
        }
        Ok(Self {
            instance_id,
            argument,
            value,
        })
    }

    /// The instance, named as the copy's fragment writes it.
    pub fn instance_id(&self) -> &Name {
        &self.instance_id
    }

    pub fn argument(&self) -> &Name {
        &self.argument
    }

    pub fn value(&self) -> &AnyType {
        &self.value
    }
}

/// Why a word is not an argument override.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArgumentOverrideError {
    #[error("{OVERRIDE_USAGE}")]
    Shape,
    #[error("invalid instance name: {0}; {OVERRIDE_USAGE}")]
    InstanceName(String),
    #[error("invalid argument name: {0}; {OVERRIDE_USAGE}")]
    ArgumentName(String),
    #[error("invalid value for `{key}`: {reason}; {OVERRIDE_USAGE}")]
    Value { key: String, reason: String },
    #[error("`{instance_id}.{argument}` contains a non-finite number; use finite JSON5 numbers")]
    NonFinite { instance_id: Name, argument: Name },
}

impl std::str::FromStr for ArgumentOverride {
    type Err = ArgumentOverrideError;

    fn from_str(word: &str) -> std::result::Result<Self, Self::Err> {
        let (key, raw) = word.split_once('=').ok_or(ArgumentOverrideError::Shape)?;
        let (instance_id, argument) = key.split_once('.').ok_or(ArgumentOverrideError::Shape)?;
        let instance_id = Name::new(instance_id)
            .map_err(|error| ArgumentOverrideError::InstanceName(error.to_string()))?;
        let argument = Name::new(argument)
            .map_err(|error| ArgumentOverrideError::ArgumentName(error.to_string()))?;
        let value = serde_json5::from_str(raw).map_err(|error| ArgumentOverrideError::Value {
            key: key.to_owned(),
            reason: error.to_string(),
        })?;
        Self::new(instance_id, argument, value)
    }
}

/// Whether every number the value holds, at any depth, is finite.
fn has_only_finite_numbers(value: &AnyType) -> bool {
    match value {
        AnyType::Float(value) => value.is_finite(),
        AnyType::Array(values) => values.iter().all(has_only_finite_numbers),
        AnyType::Object(values) => values.values().all(has_only_finite_numbers),
        AnyType::Null
        | AnyType::Bool(_)
        | AnyType::String(_)
        | AnyType::Int(_)
        | AnyType::UInt(_) => true,
    }
}

/// The argument a pair of overrides both name, in the order they arrived.
fn repeated_target(arguments: &[ArgumentOverride]) -> Option<(&Name, &Name)> {
    let targets = arguments
        .iter()
        .map(|argument| (argument.instance_id(), argument.argument()))
        .collect::<Vec<_>>();
    first_duplicate(&targets).copied()
}

/// Add one copy to the running stack: the copy of its option under its
/// name, its base on the copy's machine, and its own axes selected by
/// `selections`, each part on the machine its word names.
#[derive(Debug, Clone, PartialEq)]
pub struct StackJoinGoal {
    pub copy: CopyReference,
    pub selections: Vec<SelectionWord>,
    pub arguments: Vec<ArgumentOverride>,
    pub budgets: StackBudgets,
    /// The client of a daemon interface that a daemon sends the goal for;
    /// `None` when the sender asks for itself.
    pub client: Option<GoalClient>,
}

/// The client of a daemon interface that a daemon sends a stack goal for:
/// the leases of the robots read it in place of the sender of the goal.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GoalClient {
    pub core_node: String,
    pub instance_id: String,
}

impl GoalClient {
    pub(crate) fn write(&self, mut wire: launch_capnp::goal_client::Builder<'_>) {
        wire.set_core_node(self.core_node.as_str());
        wire.set_instance_id(self.instance_id.as_str());
    }

    pub(crate) fn read(wire: launch_capnp::goal_client::Reader<'_>) -> Result<Self> {
        Ok(Self {
            core_node: wire.get_core_node()?.to_str()?.to_owned(),
            instance_id: wire.get_instance_id()?.to_str()?.to_owned(),
        })
    }
}

impl StackJoinGoal {
    pub fn new(copy: CopyReference, budgets: StackBudgets) -> Self {
        Self {
            copy,
            selections: Vec::new(),
            arguments: Vec::new(),
            budgets,
            client: None,
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut message = Builder::new_default();
        let mut goal = message.init_root::<launch_capnp::stack_join_goal::Builder>();
        write_copy_reference(goal.reborrow().init_copy(), &self.copy);
        goal.set_node_add_idle_timeout_secs(self.budgets.node_add_idle_timeout_secs);
        goal.set_node_build_idle_timeout_secs(self.budgets.node_build_idle_timeout_secs);
        goal.set_node_run_idle_timeout_secs(self.budgets.node_run_idle_timeout_secs);
        goal.set_max_timeout_secs(self.budgets.max_timeout_secs.unwrap_or(0));
        let mut selections = goal
            .reborrow()
            .init_selections(capnp_list_len(self.selections.len(), "selections")?);
        for (index, word) in self.selections.iter().enumerate() {
            write_selection_word(selections.reborrow().get(index as u32), word);
        }
        let mut arguments = goal
            .reborrow()
            .init_arguments(capnp_list_len(self.arguments.len(), "arguments")?);
        for (index, argument) in self.arguments.iter().enumerate() {
            let mut wire = arguments.reborrow().get(index as u32);
            wire.set_instance_id(argument.instance_id().as_str());
            wire.set_argument(argument.argument().as_str());
            wire.set_value(
                serde_json5::to_string(argument.value())
                    .map_err(|error| crate::Error::Encoding(error.to_string()))?,
            );
        }
        if let Some(client) = &self.client {
            client.write(goal.reborrow().init_client());
        }
        self.budgets.write_env_vars(
            goal.init_env_vars(capnp_list_len(self.budgets.env_vars.len(), "env_vars")?),
        );
        encode_message(&message)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let message = decode_message(data)?;
        let goal = message.get_root::<launch_capnp::stack_join_goal::Reader>()?;
        if !goal.has_copy() {
            return Err(crate::Error::Decoding(
                "`StackJoinGoal.copy` is missing".to_owned(),
            ));
        }
        let copy = read_copy_reference(goal.get_copy()?, "StackJoinGoal.copy")?;
        let selections = goal
            .get_selections()?
            .iter()
            .enumerate()
            .map(|(index, word)| {
                read_selection_word(word, &format!("StackJoinGoal.selections[{index}]"))
            })
            .collect::<Result<Vec<_>>>()?;
        let arguments = goal
            .get_arguments()?
            .iter()
            .map(read_argument_override)
            .collect::<Result<Vec<_>>>()?;
        if let Some((instance_id, argument)) = repeated_target(&arguments) {
            return Err(crate::Error::Decoding(format!(
                "`arguments` overrides `{instance_id}.{argument}` twice"
            )));
        }
        let budgets = StackBudgets::decode(
            goal.get_env_vars()?,
            goal.get_node_add_idle_timeout_secs(),
            goal.get_node_build_idle_timeout_secs(),
            goal.get_node_run_idle_timeout_secs(),
            goal.get_max_timeout_secs(),
        )?;
        let client = if goal.has_client() {
            Some(GoalClient::read(goal.get_client()?)?)
        } else {
            None
        };
        Ok(Self {
            copy,
            selections,
            arguments,
            budgets,
            client,
        })
    }
}

/// Reads one override of a [`StackJoinGoal`]: both names must be names and
/// the value must be JSON5 holding only finite numbers.
fn read_argument_override(
    argument: launch_capnp::argument_override::Reader<'_>,
) -> Result<ArgumentOverride> {
    let instance_id = read_name(
        argument.get_instance_id()?.to_str()?,
        "arguments.instance_id",
    )?;
    let key = read_name(argument.get_argument()?.to_str()?, "arguments.argument")?;
    let value = serde_json5::from_str(argument.get_value()?.to_str()?)
        .map_err(|error| crate::Error::Decoding(format!("`arguments.value`: {error}")))?;
    ArgumentOverride::new(instance_id, key, value)
        .map_err(|error| crate::Error::Decoding(error.to_string()))
}

impl crate::encoding::Wire for StackJoinGoal {
    type Root = launch_capnp::stack_join_goal::Owned;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::decoding_error;

    fn goal(name: &str) -> StackJoinGoal {
        StackJoinGoal::new(
            format!("openarm_real:{name}").parse().unwrap(),
            StackBudgets::new(1, 1, 1, None),
        )
    }

    /// A wire goal of the copy `alpha` of `openarm_real`, on the coordinator,
    /// with valid budgets, for the tests that write one field by hand.
    fn wire_goal(
        message: &mut Builder<capnp::message::HeapAllocator>,
    ) -> launch_capnp::stack_join_goal::Builder<'_> {
        let mut goal = message.init_root::<launch_capnp::stack_join_goal::Builder>();
        let mut copy = goal.reborrow().init_copy();
        copy.set_option("openarm_real");
        copy.set_name("alpha");
        copy.init_core_node().set_coordinator(());
        goal.set_node_add_idle_timeout_secs(1);
        goal.set_node_build_idle_timeout_secs(1);
        goal.set_node_run_idle_timeout_secs(1);
        goal
    }

    #[test]
    fn join_goals_round_trip() {
        let mut goal = goal("bravo");
        goal.copy = "openarm_real:bravo@jetson-2".parse().unwrap();
        goal.selections = vec![
            "commander=xr_commander".parse().unwrap(),
            "lerobot_recorder@self".parse().unwrap(),
            "ai_brain_vla@cn-gpu-box".parse().unwrap(),
        ];
        goal.arguments = vec!["backbone_inst.speed=0.2".parse().unwrap()];
        goal.budgets = StackBudgets::new(42, 900, 73, Some(1200))
            .with_env_vars(vec![("PATH".into(), "/usr/bin".into())]);
        assert_eq!(
            StackJoinGoal::decode(&goal.encode().unwrap()).unwrap(),
            goal
        );
    }

    /// A goal with no copy, or whose copy names no machine, is refused: no
    /// null pointer means the coordinator.
    #[test]
    fn a_goal_without_its_copy_is_refused() {
        let mut message = Builder::new_default();
        message.init_root::<launch_capnp::stack_join_goal::Builder>();
        let refusal = decoding_error(StackJoinGoal::decode(&encode_message(&message).unwrap()));
        assert!(
            refusal.contains("`StackJoinGoal.copy` is missing"),
            "{refusal}"
        );

        let mut message = Builder::new_default();
        let mut wire = message.init_root::<launch_capnp::stack_join_goal::Builder>();
        let mut copy = wire.reborrow().init_copy();
        copy.set_option("openarm_real");
        copy.set_name("alpha");
        let refusal = decoding_error(StackJoinGoal::decode(&encode_message(&message).unwrap()));
        assert!(
            refusal.contains("`StackJoinGoal.copy.core_node` is missing"),
            "{refusal}"
        );
    }

    /// An idle budget of 0 is refused at the wire, naming its phase.
    #[test]
    fn a_zero_idle_budget_is_refused() {
        let mut goal = goal("alpha");
        goal.budgets = StackBudgets::new(0, 600, 600, None);
        let refusal = decoding_error(StackJoinGoal::decode(&goal.encode().unwrap()));
        assert!(refusal.contains("node_add_idle_timeout_secs"), "{refusal}");
    }

    /// A bad name, a copy named `self` and a bad machine are refused where
    /// they arrive, each naming its field.
    #[test]
    fn bad_names_are_refused_at_the_wire_boundary() {
        for (option, name, machine, field) in [
            ("", "alpha", None, "`StackJoinGoal.copy.option`"),
            ("openarm_real", "", None, "`StackJoinGoal.copy.name`"),
            ("openarm_real", "self", None, "`StackJoinGoal.copy.name`"),
            (
                "openarm_real",
                &"a".repeat(64),
                None,
                "`StackJoinGoal.copy.name`",
            ),
            (
                "openarm_real",
                "alpha",
                Some("self"),
                "`StackJoinGoal.copy.core_node.named`",
            ),
            (
                "openarm_real",
                "alpha",
                Some("bad/name"),
                "`StackJoinGoal.copy.core_node.named`",
            ),
        ] {
            let mut message = Builder::new_default();
            let mut copy = wire_goal(&mut message).init_copy();
            copy.set_option(option);
            copy.set_name(name);
            match machine {
                Some(machine) => copy.init_core_node().set_named(machine),
                None => copy.init_core_node().set_coordinator(()),
            }
            let refusal = decoding_error(StackJoinGoal::decode(&encode_message(&message).unwrap()));
            assert!(refusal.contains(field), "{option}:{name}: {refusal}");
        }

        let mut message = Builder::new_default();
        let mut word = wire_goal(&mut message).init_selections(1).get(0);
        let mut choice = word.reborrow().init_choice();
        choice.reborrow().init_axis().set_unnamed(());
        choice.set_option("bad/option");
        word.init_core_node().set_with_copy(());
        let refusal = decoding_error(StackJoinGoal::decode(&encode_message(&message).unwrap()));
        assert!(
            refusal.contains("`StackJoinGoal.selections[0].choice.option`"),
            "{refusal}"
        );
    }

    /// Each refusal of a wire override names what is wrong with it. The goal
    /// carries valid budgets, so no later field refusal can stand in for the
    /// override's own.
    #[test]
    fn wire_argument_refusals_name_the_override_field() {
        for (instance, key, value, expected) in [
            (
                "arm",
                "gain",
                "NaN",
                "`arm.gain` contains a non-finite number",
            ),
            (
                "arm",
                "gain",
                "{value: [Infinity]}",
                "`arm.gain` contains a non-finite number",
            ),
            ("bad/name", "gain", "1", "`arguments.instance_id`"),
            ("arm", "", "1", "`arguments.argument`"),
            ("arm", "gain", "{", "`arguments.value`"),
        ] {
            let mut message = Builder::new_default();
            let mut argument = wire_goal(&mut message).init_arguments(1).get(0);
            argument.set_instance_id(instance);
            argument.set_argument(key);
            argument.set_value(value);
            let refusal = decoding_error(StackJoinGoal::decode(&encode_message(&message).unwrap()));
            assert!(refusal.contains(expected), "{refusal}");
        }
    }

    /// Two overrides of one argument have no order that settles them, so the
    /// pair is refused where it arrives.
    #[test]
    fn one_argument_is_overridden_at_most_once() {
        let mut goal = goal("alpha");
        goal.arguments = vec![
            "arm_inst.speed=0.2".parse().unwrap(),
            "arm_inst.speed=0.4".parse().unwrap(),
        ];
        let refusal = decoding_error(StackJoinGoal::decode(&goal.encode().unwrap()));
        assert!(
            refusal.contains("`arguments` overrides `arm_inst.speed` twice"),
            "{refusal}"
        );

        goal.arguments = vec![
            "arm_inst.speed=0.2".parse().unwrap(),
            "arm_inst.gain=0.4".parse().unwrap(),
            "wrist_inst.speed=0.4".parse().unwrap(),
        ];
        assert_eq!(
            StackJoinGoal::decode(&goal.encode().unwrap()).unwrap(),
            goal
        );
    }

    #[test]
    fn overrides_parse_names_and_preserve_json5_values() {
        let argument: ArgumentOverride = "arm_inst.options={gains: [0.2, 1], mode: 'pose'}"
            .parse()
            .unwrap();
        assert_eq!(argument.instance_id().as_str(), "arm_inst");
        assert_eq!(argument.argument().as_str(), "options");
        assert!(matches!(argument.value(), AnyType::Object(_)));
        for word in ["", "arm.speed"] {
            assert_eq!(
                word.parse::<ArgumentOverride>().unwrap_err(),
                ArgumentOverrideError::Shape,
                "{word}"
            );
        }
        assert!(matches!(
            "bad/inst.speed=2".parse::<ArgumentOverride>(),
            Err(ArgumentOverrideError::InstanceName(_))
        ));
        assert!(matches!(
            "arm..speed=2".parse::<ArgumentOverride>(),
            Err(ArgumentOverrideError::ArgumentName(_))
        ));
        for word in ["arm.speed=", "arm.speed=pose"] {
            assert!(
                matches!(
                    word.parse::<ArgumentOverride>(),
                    Err(ArgumentOverrideError::Value { .. })
                ),
                "{word}"
            );
        }
        assert!(
            ArgumentOverrideError::Shape
                .to_string()
                .contains("INSTANCE.ARGUMENT=JSON5")
        );
    }

    #[test]
    fn non_finite_numbers_are_rejected_at_every_depth() {
        for value in [
            "NaN",
            "Infinity",
            "-Infinity",
            "[1, NaN]",
            "{gain: [Infinity]}",
        ] {
            let error = format!("arm.gain={value}")
                .parse::<ArgumentOverride>()
                .unwrap_err();
            assert_eq!(
                error,
                ArgumentOverrideError::NonFinite {
                    instance_id: Name::new("arm").unwrap(),
                    argument: Name::new("gain").unwrap(),
                }
            );
            assert!(error.to_string().contains("non-finite"), "{error}");
        }
        assert!(
            ArgumentOverride::new(
                Name::new("arm").unwrap(),
                Name::new("gain").unwrap(),
                AnyType::Float(f64::NAN)
            )
            .is_err()
        );
    }
}
