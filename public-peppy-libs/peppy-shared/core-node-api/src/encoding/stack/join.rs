//! `stack join`: one more copy of an option onto the running stack.

use capnp::message::Builder;
use config::AnyType;
use config::runtime::{CoreNodeName, Name, first_duplicate};

use crate::encoding::{
    capnp_list_len, decode_message, encode_message, read_core_node_name, read_name, required_text,
    write_text_list,
};
use crate::{Payload, Result, launch_capnp};

use super::budgets::StackBudgets;
use super::read_selections;

/// The shape of an override, quoted in every refusal so the message says what
/// to type.
const OVERRIDE_USAGE: &str = "an override is written INSTANCE.ARGUMENT=JSON5";

/// Where one copy runs as a whole: on its coordinator, or on one concrete
/// machine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum JoinPlacement {
    #[default]
    Local,
    CoreNode(CoreNodeName),
}

impl JoinPlacement {
    pub fn resolve(&self, coordinator: &CoreNodeName) -> CoreNodeName {
        match self {
            Self::Local => coordinator.clone(),
            Self::CoreNode(host) => host.clone(),
        }
    }
}

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
        if !finite(&value) {
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
fn finite(value: &AnyType) -> bool {
    match value {
        AnyType::Float(value) => value.is_finite(),
        AnyType::Array(values) => values.iter().all(finite),
        AnyType::Object(values) => values.values().all(finite),
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

/// Add one copy of `option` to the running stack under `name`, its own axes
/// selected by `selections`.
#[derive(Debug, Clone, PartialEq)]
pub struct StackJoinGoal {
    pub name: Name,
    pub option: String,
    pub selections: Vec<String>,
    pub arguments: Vec<ArgumentOverride>,
    pub placement: JoinPlacement,
    pub budgets: StackBudgets,
}

impl StackJoinGoal {
    pub fn new(name: Name, option: impl Into<String>, budgets: StackBudgets) -> Self {
        Self {
            name,
            option: option.into(),
            selections: Vec::new(),
            arguments: Vec::new(),
            placement: JoinPlacement::Local,
            budgets,
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut message = Builder::new_default();
        let mut goal = message.init_root::<launch_capnp::stack_join_goal::Builder>();
        goal.set_name(self.name.as_str());
        goal.set_option(&self.option);
        goal.set_node_add_idle_timeout_secs(self.budgets.node_add_idle_timeout_secs);
        goal.set_node_build_idle_timeout_secs(self.budgets.node_build_idle_timeout_secs);
        goal.set_node_run_idle_timeout_secs(self.budgets.node_run_idle_timeout_secs);
        goal.set_max_timeout_secs(self.budgets.max_timeout_secs.unwrap_or(0));
        write_text_list(
            goal.reborrow()
                .init_selections(capnp_list_len(self.selections.len(), "selections")?),
            &self.selections,
        );
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
        match &self.placement {
            JoinPlacement::Local => goal.reborrow().init_placement().set_coordinator(()),
            JoinPlacement::CoreNode(host) => goal
                .reborrow()
                .init_placement()
                .set_core_node(host.as_str()),
        }
        self.budgets.write_env_vars(
            goal.init_env_vars(capnp_list_len(self.budgets.env_vars.len(), "env_vars")?),
        );
        encode_message(&message)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let message = decode_message(data)?;
        let goal = message.get_root::<launch_capnp::stack_join_goal::Reader>()?;
        let name = read_name(goal.get_name()?.to_str()?, "StackJoinGoal.name")?;
        let option = required_text(goal.get_option()?.to_str()?, "StackJoinGoal.option")?;
        let selections = read_selections(goal.get_selections()?, "StackJoinGoal.selections")?;
        let arguments =
            goal.get_arguments()?
                .iter()
                .map(|argument| {
                    let instance_id = read_name(
                        argument.get_instance_id()?.to_str()?,
                        "arguments.instance_id",
                    )?;
                    let key = read_name(argument.get_argument()?.to_str()?, "arguments.argument")?;
                    let value = serde_json5::from_str(argument.get_value()?.to_str()?).map_err(
                        |error| crate::Error::Decoding(format!("`arguments.value`: {error}")),
                    )?;
                    ArgumentOverride::new(instance_id, key, value)
                        .map_err(|error| crate::Error::Decoding(error.to_string()))
                })
                .collect::<Result<Vec<_>>>()?;
        if let Some((instance_id, argument)) = repeated_target(&arguments) {
            return Err(crate::Error::Decoding(format!(
                "`arguments` overrides `{instance_id}.{argument}` twice"
            )));
        }
        use launch_capnp::stack_join_goal::placement::Which;
        let placement = match goal.get_placement().which()? {
            Which::Coordinator(()) => JoinPlacement::Local,
            Which::CoreNode(host) => {
                JoinPlacement::CoreNode(read_core_node_name(host?.to_str()?, "placement")?)
            }
        };
        let budgets = StackBudgets::decode(
            goal.get_env_vars()?,
            goal.get_node_add_idle_timeout_secs(),
            goal.get_node_build_idle_timeout_secs(),
            goal.get_node_run_idle_timeout_secs(),
            goal.get_max_timeout_secs(),
        )?;
        Ok(Self {
            name,
            option,
            selections,
            arguments,
            placement,
            budgets,
        })
    }
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
            Name::new(name).unwrap(),
            "openarm_real",
            StackBudgets::default(),
        )
    }

    #[test]
    fn join_goals_round_trip() {
        let mut goal = goal("bravo");
        goal.selections = vec!["commander=xr_commander".into(), "lerobot_recorder".into()];
        goal.arguments = vec!["backbone_inst.speed=0.2".parse().unwrap()];
        goal.placement = JoinPlacement::CoreNode(CoreNodeName::new("jetson-2").unwrap());
        goal.budgets = StackBudgets::new(42, 900, 73, Some(1200))
            .with_env_vars(vec![("PATH".into(), "/usr/bin".into())]);
        assert_eq!(
            StackJoinGoal::decode(&goal.encode().unwrap()).unwrap(),
            goal
        );
    }

    /// A copy nobody places runs on the coordinator, which is also what an
    /// absent placement decodes to; `self` is no machine's name.
    #[test]
    fn placement_round_trips_and_resolves() {
        let decoded = StackJoinGoal::decode(&goal("alpha").encode().unwrap()).unwrap();
        assert_eq!(decoded.placement, JoinPlacement::Local);
        let coordinator = CoreNodeName::new("laptop").unwrap();
        assert_eq!(decoded.placement.resolve(&coordinator), coordinator);
        let jetson = CoreNodeName::new("jetson-1").unwrap();
        assert_eq!(
            JoinPlacement::CoreNode(jetson.clone()).resolve(&coordinator),
            jetson
        );

        let mut message = Builder::new_default();
        let mut wire = message.init_root::<launch_capnp::stack_join_goal::Builder>();
        wire.set_name("alpha");
        wire.set_option("openarm_v2");
        wire.init_placement().set_core_node("self");
        let refusal = decoding_error(StackJoinGoal::decode(&encode_message(&message).unwrap()));
        assert!(refusal.contains("`placement`"), "{refusal}");
    }

    /// An idle budget of 0 is refused at the wire, naming its phase.
    #[test]
    fn a_zero_idle_budget_is_refused() {
        let mut goal = goal("alpha");
        goal.budgets = StackBudgets::new(0, 600, 600, None);
        let refusal = decoding_error(StackJoinGoal::decode(&goal.encode().unwrap()));
        assert!(refusal.contains("node_add_idle_timeout_secs"), "{refusal}");
    }

    #[test]
    fn an_empty_name_or_option_is_rejected_at_the_wire_boundary() {
        for (name, option) in [("", ""), ("alpha", ""), ("bad/name", "openarm_v2")] {
            let mut message = Builder::new_default();
            let mut wire = message.init_root::<launch_capnp::stack_join_goal::Builder>();
            wire.set_name(name);
            wire.set_option(option);
            assert!(StackJoinGoal::decode(&encode_message(&message).unwrap()).is_err());
        }
    }

    /// A blank word is the empty segment of a caller's comma list, so the
    /// refusal names the entry and the shape of a real one.
    #[test]
    fn a_blank_selection_is_refused_with_the_shape_of_a_real_one() {
        let mut goal = goal("alpha");
        goal.selections.push(String::new());
        let refusal = decoding_error(StackJoinGoal::decode(&goal.encode().unwrap()));
        assert!(
            refusal.contains("`StackJoinGoal.selections[0]` is empty"),
            "{refusal}"
        );
        assert!(refusal.contains("`axis=option`"), "{refusal}");
        assert!(refusal.contains("stray comma"), "{refusal}");
    }

    #[test]
    fn wire_arguments_reject_non_finite_numbers_and_invalid_names() {
        for (instance, key, value) in [
            ("arm", "gain", "NaN"),
            ("arm", "gain", "{value: [Infinity]}"),
            ("bad/name", "gain", "1"),
            ("arm", "", "1"),
        ] {
            let mut message = Builder::new_default();
            let mut goal = message.init_root::<launch_capnp::stack_join_goal::Builder>();
            goal.set_name("alpha");
            goal.set_option("openarm_real");
            let mut arguments = goal.init_arguments(1);
            let mut argument = arguments.reborrow().get(0);
            argument.set_instance_id(instance);
            argument.set_argument(key);
            argument.set_value(value);
            assert!(StackJoinGoal::decode(&encode_message(&message).unwrap()).is_err());
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
