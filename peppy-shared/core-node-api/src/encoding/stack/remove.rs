//! `stack remove`: one copy off the running stack, by name.

use capnp::message::Builder;
use config::runtime::CopyName;

use super::join::GoalClient;
use super::words::read_copy_name;
use crate::encoding::{decode_message, encode_message};
use crate::{Payload, Result, launch_capnp};

/// Stop and remove the instances of the copy called `name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackRemoveGoal {
    pub name: CopyName,
    /// The client of a daemon interface that a daemon sends the goal for;
    /// `None` when the sender asks for itself.
    pub client: Option<GoalClient>,
}

impl StackRemoveGoal {
    pub fn new(name: CopyName) -> Self {
        Self { name, client: None }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut message = Builder::new_default();
        let mut goal = message.init_root::<launch_capnp::stack_remove_goal::Builder>();
        goal.set_name(self.name.as_str());
        if let Some(client) = &self.client {
            client.write(goal.init_client());
        }
        encode_message(&message)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let message = decode_message(data)?;
        let goal = message.get_root::<launch_capnp::stack_remove_goal::Reader>()?;
        let client = if goal.has_client() {
            Some(GoalClient::read(goal.get_client()?)?)
        } else {
            None
        };
        Ok(Self {
            name: read_copy_name(goal.get_name()?.to_str()?, "StackRemoveGoal.name")?,
            client,
        })
    }
}

impl crate::encoding::Wire for StackRemoveGoal {
    type Root = launch_capnp::stack_remove_goal::Owned;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::decoding_error;

    #[test]
    fn remove_goals_round_trip_and_refuse_invalid_names() {
        let remove = StackRemoveGoal::new(CopyName::new("bravo").unwrap());
        assert_eq!(
            StackRemoveGoal::decode(&remove.encode().unwrap()).unwrap(),
            remove
        );
        let for_client = StackRemoveGoal {
            client: Some(GoalClient {
                core_node: "cn-one".to_owned(),
                instance_id: "director_inst".to_owned(),
            }),
            ..remove.clone()
        };
        assert_eq!(
            StackRemoveGoal::decode(&for_client.encode().unwrap()).unwrap(),
            for_client
        );
        for name in ["", "bad/name", "self"] {
            let mut message = Builder::new_default();
            message
                .init_root::<launch_capnp::stack_remove_goal::Builder>()
                .set_name(name);
            let refusal =
                decoding_error(StackRemoveGoal::decode(&encode_message(&message).unwrap()));
            assert!(refusal.contains("StackRemoveGoal.name"), "{refusal}");
        }
    }
}
