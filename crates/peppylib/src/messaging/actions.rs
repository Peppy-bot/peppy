use super::{
    MessengerHandle, PROBE_TIMEOUT, SERVICE_PROBE_PAYLOAD, ServiceEndpoint, TopicPublisher,
};
use crate::error::{Error, Result};
use bytes::Bytes;
use config::node::QoSProfile;
use pmi::{MessengerBackend, Subscription, TopicMessage};
use tokio::time::Duration;

pub struct ActionMessenger;

pub struct ActionGoalHandle {
    daemon_node: String,
    instance_id: String,
    node_name: String,
    action_name: String,
    target_instance_id: Option<String>,
    goal_response: TopicMessage,
    feedback: Subscription,
}

impl std::fmt::Debug for ActionGoalHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActionGoalHandle")
            .field("daemon_node", &self.daemon_node)
            .field("instance_id", &self.instance_id)
            .field("node_name", &self.node_name)
            .field("action_name", &self.action_name)
            .field("target_instance_id", &self.target_instance_id)
            .finish_non_exhaustive()
    }
}

impl ActionGoalHandle {
    pub fn goal_response(&self) -> &TopicMessage {
        &self.goal_response
    }

    /// Receives the next feedback message.
    pub async fn on_next_feedback(&mut self) -> Result<TopicMessage> {
        self.feedback
            .on_next_message()
            .await
            .ok_or(Error::ActionFeedbackChannelClosed)
    }

    /// Attempts to receive the next feedback message without waiting.
    pub fn try_next_feedback(&mut self) -> Result<Option<TopicMessage>> {
        match self.feedback.rx.try_recv() {
            Ok(message) => Ok(Some(message)),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                Err(Error::ActionFeedbackChannelClosed)
            }
        }
    }
}

// https://docs.ros.org/en/foxy/_images/Action-SingleActionClient.gif
pub struct ActionCreation {
    pub goal_service: ServiceEndpoint,
    pub cancel_service: ServiceEndpoint,
    pub feedback_publisher: TopicPublisher,
    pub result_service: ServiceEndpoint,
}

impl ActionMessenger {
    pub async fn expose(
        messenger: &MessengerHandle,
        bound_daemon_node: &str,
        as_instance_id: &str,
        as_node_name: &str,
        as_action_name: &str,
    ) -> Result<ActionCreation> {
        messenger
            .expose_action(
                bound_daemon_node,
                as_node_name,
                as_action_name,
                as_instance_id,
            )
            .await
    }

    /// Sends a lightweight probe to check whether an action service is listening.
    #[allow(clippy::too_many_arguments)]
    pub async fn is_reachable(
        messenger: &MessengerHandle,
        bound_daemon_node: &str,
        as_instance_id: &str,
        target_node_name: &str,
        target_action_name: &str,
        target_daemon_node: Option<&str>,
        target_instance_id: Option<&str>,
    ) -> Result<bool> {
        match messenger
            .poll_service(
                "action",
                bound_daemon_node,
                as_instance_id,
                target_node_name,
                target_action_name,
                target_daemon_node,
                target_instance_id,
                Bytes::from_static(SERVICE_PROBE_PAYLOAD),
                PROBE_TIMEOUT,
            )
            .await
        {
            Ok(_) => Ok(true),
            Err(Error::ServiceUnreachable { .. }) => Ok(false),
            Err(Error::ServiceTimeout { .. }) => Ok(true),
            Err(e) => Err(e),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn send_goal(
        messenger: &MessengerHandle,
        as_daemon_node: &str,
        as_instance_id: &str,
        to_node_name: &str,
        to_action_name: &str,
        target_daemon_node: Option<&str>,
        target_instance_id: Option<&str>,
        goal_payload: Bytes,
        feedback_qos: QoSProfile,
        goal_timeout: Duration,
    ) -> Result<ActionGoalHandle> {
        let feedback_topic = {
            let sender_daemon = target_daemon_node.unwrap_or("*");
            match target_instance_id {
                Some(target_instance_id) => {
                    format!(
                        "{as_daemon_node}/{sender_daemon}/{as_instance_id}/{target_instance_id}/action/{to_node_name}/{to_action_name}/feedback/{target_instance_id}"
                    )
                }
                None => format!(
                    "{as_daemon_node}/*/{as_instance_id}/*/action/{to_node_name}/{to_action_name}/feedback/*"
                ),
            }
        };
        let goal_service_name = format!("{to_action_name}/goal");

        let feedback_subscription = {
            let messenger = messenger.messenger.lock().await;
            messenger
                .subscribe(&feedback_topic, feedback_qos.into())
                .await
        }
        .map_err(Error::PeppyMessagingInterface)?;

        let goal_response = messenger
            .poll_service(
                "action",
                as_daemon_node,
                as_instance_id,
                to_node_name,
                &goal_service_name,
                None,
                target_instance_id,
                goal_payload,
                goal_timeout,
            )
            .await?;

        Ok(ActionGoalHandle {
            daemon_node: as_daemon_node.to_string(),
            instance_id: as_instance_id.to_string(),
            node_name: to_node_name.to_string(),
            action_name: to_action_name.to_string(),
            target_instance_id: target_instance_id.map(|id| id.to_string()),
            goal_response,
            feedback: feedback_subscription,
        })
    }

    pub async fn cancel_goal(
        messenger_handle: &MessengerHandle,
        action_handle: &ActionGoalHandle,
        cancel_timeout: Duration,
    ) -> Result<TopicMessage> {
        Self::cancel_goal_with(
            messenger_handle,
            &action_handle.daemon_node,
            &action_handle.instance_id,
            &action_handle.node_name,
            &action_handle.action_name,
            action_handle.target_instance_id.as_deref(),
            cancel_timeout,
        )
        .await
    }

    /// Like [`cancel_goal`](Self::cancel_goal) but accepts individual fields,
    /// allowing callers to avoid holding a lock on the goal handle during the
    /// network round-trip.
    #[allow(clippy::too_many_arguments)]
    pub async fn cancel_goal_with(
        messenger_handle: &MessengerHandle,
        daemon_node: &str,
        instance_id: &str,
        node_name: &str,
        action_name: &str,
        target_instance_id: Option<&str>,
        cancel_timeout: Duration,
    ) -> Result<TopicMessage> {
        let cancel_service_name = format!("{action_name}/cancel");

        messenger_handle
            .poll_service(
                "action",
                daemon_node,
                instance_id,
                node_name,
                &cancel_service_name,
                None,
                target_instance_id,
                Bytes::new(),
                cancel_timeout,
            )
            .await
    }

    pub async fn request_result(
        messenger_handle: &MessengerHandle,
        action_handle: &ActionGoalHandle,
        result_timeout: Duration,
    ) -> Result<TopicMessage> {
        Self::request_result_with(
            messenger_handle,
            &action_handle.daemon_node,
            &action_handle.instance_id,
            &action_handle.node_name,
            &action_handle.action_name,
            action_handle.target_instance_id.as_deref(),
            result_timeout,
        )
        .await
    }

    /// Like [`request_result`](Self::request_result) but accepts individual
    /// fields, allowing callers to avoid holding a lock on the goal handle
    /// during the network round-trip.
    #[allow(clippy::too_many_arguments)]
    pub async fn request_result_with(
        messenger_handle: &MessengerHandle,
        daemon_node: &str,
        instance_id: &str,
        node_name: &str,
        action_name: &str,
        target_instance_id: Option<&str>,
        result_timeout: Duration,
    ) -> Result<TopicMessage> {
        let result_service_name = format!("{action_name}/result");

        messenger_handle
            .poll_service(
                "action",
                daemon_node,
                instance_id,
                node_name,
                &result_service_name,
                None,
                target_instance_id,
                Bytes::new(),
                result_timeout,
            )
            .await
            .map_err(|err| match err {
                Error::ServiceTimeout { instance_id, .. } => Error::ActionResultTimeout {
                    instance_id,
                    action_name: action_name.to_string(),
                },
                Error::ServiceUnreachable { instance_id, .. } => Error::ActionResultUnreachable {
                    instance_id,
                    action_name: action_name.to_string(),
                },
                other => other,
            })
    }
}
