//! The pinned documents of one instance that the tests of `peppy node call`,
//! `echo` and `describe` read, and the mock messaging they run on.
//!
//! The node `arm:v1` implements `limb_motion:v1` through the slot `arm`,
//! refining the goal of `move_to` to three joints; it declares a service and
//! a topic of its own, and emits `joint_states` through its `arm_link:v1`
//! pairing slot `controller`, where it plays `arm`.

use core_node_api::encoding::{InstanceDocuments, PinnedDocument};
use message_codec::MessageCodec;
use message_codec::consumer::ConsumerIdentity;
use peppylib::{MessengerHandle, Payload};
use pmi::{Messenger, MessengerAdapter, MessengerBackend, MockAdapter};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;

pub(crate) const CORE_NODE: &str = "robot-7";
pub(crate) const INSTANCE: &str = "arm_inst";

pub(crate) const MANIFEST: &str = r#"{
    peppy_schema: "node/v1",
    manifest: {
        name: "arm", tag: "v1",
        implements: [{ name: "limb_motion", tag: "v1", link_id: "arm" }],
        depends_on: {
            pairings: [{ name: "arm_link", tag: "v1", role: "arm", link_id: "controller" }],
        },
    },
    interfaces: {
        topics: {
            emits: [
                { name: "heartbeat", qos_profile: "reliable", message_format: { count: "u64" } },
                { link_id: "arm", name: "limb_states" },
                { link_id: "controller", name: "joint_states" },
            ],
        },
        services: {
            exposes: [
                {
                    name: "set_gain",
                    request_message_format: { value: "i32" },
                    response_message_format: { current_value: "i32" },
                },
                { link_id: "arm", name: "check_move" },
            ],
        },
        actions: {
            exposes: [
                {
                    link_id: "arm",
                    name: "move_to",
                    refine: { goal_service: { request_message_format: { joints: { $length: 3 } } } },
                },
            ],
        },
    },
    execution: { language: "rust", run_cmd: ["sleep", "10"] },
}"#;

pub(crate) const LIMB_MOTION: &str = r#"{
    peppy_schema: "contract/v1",
    manifest: { name: "limb_motion", tag: "v1" },
    failure_kinds: { unreachable: "No plan to the target" },
    interfaces: {
        topics: [
            {
                name: "limb_states",
                qos_profile: "sensor_data",
                message_format: { joints: { $type: "array", $items: "f64" } },
            },
        ],
        services: [
            {
                name: "check_move",
                request_message_format: { joints: { $type: "array", $items: "f64" } },
                response_message_format: { reachable: "bool" },
                failure_kinds: ["unreachable"],
            },
        ],
        actions: [
            {
                name: "move_to",
                goal_service: {
                    request_message_format: { joints: { $type: "array", $items: "f64" } },
                },
                feedback_topic: { message_format: { progress: "f64" } },
                result_service: { response_message_format: { reached: "bool" } },
                failure_kinds: ["unreachable"],
            },
        ],
    },
}"#;

pub(crate) const ARM_LINK: &str = r#"{
    peppy_schema: "pairing/v1",
    manifest: { name: "arm_link", tag: "v1" },
    roles: ["controller", "arm"],
    topics: [
        {
            emitted_by: "controller",
            name: "joint_commands",
            qos_profile: "reliable",
            message_format: { target: { $type: "array", $items: "f64" } },
        },
        {
            emitted_by: "arm",
            name: "joint_states",
            qos_profile: "sensor_data",
            message_format: { positions: { $type: "array", $items: "f64" } },
        },
    ],
}"#;

/// The pinned documents of the instance, as the daemon gives them.
pub(crate) fn documents() -> InstanceDocuments {
    InstanceDocuments {
        core_node: CORE_NODE.to_owned(),
        instance_id: INSTANCE.to_owned(),
        manifest: PinnedDocument::new("arm", "v1", MANIFEST),
        contracts: vec![PinnedDocument::new("limb_motion", "v1", LIMB_MOTION)],
        pairings: vec![PinnedDocument::new("arm_link", "v1", ARM_LINK)],
    }
}

/// Who the command is on the wire in the tests.
pub(crate) fn caller() -> ConsumerIdentity {
    ConsumerIdentity {
        core_node: "laptop".to_owned(),
        instance_id: "peppy-cli-test".to_owned(),
    }
}

/// The codec of the message format `format`, written as JSON5.
pub(crate) fn codec(format: &str) -> MessageCodec {
    MessageCodec::new("test", serde_json5::from_str(format).expect("a format"))
        .expect("the format lays out")
}

/// `value` encoded with the codec of the message format `format`.
pub(crate) fn payload(format: &str, value: Value) -> Payload {
    Payload::from(codec(format).encode(&value).expect("the value encodes"))
}

/// A session on in-process mock messaging, which the instance's providers
/// and the command share.
pub(crate) async fn mock_messenger() -> MessengerHandle {
    let mut messenger = Messenger::new(MessengerAdapter::Mock(MockAdapter::default()));
    messenger
        .start_session()
        .await
        .expect("the mock session starts");
    MessengerHandle::from_shared(Arc::new(Mutex::new(messenger)))
}
