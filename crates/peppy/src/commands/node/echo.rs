//! `peppy node echo <instance> <topic>`: prints the messages of one topic of
//! a running instance, one JSON object per line on standard output, until
//! Ctrl-C.
//!
//! The command subscribes to the topic by its key, as a client with no node,
//! so it needs no slot that a launcher binds: a contract topic or a topic of
//! the node's own from the instance alone, and a pairing topic as an
//! observer of every pair that the instance's slot holds. It decodes each
//! message with the format of the pinned manifest. A topic that retains its
//! newest messages gives them first.

use super::interfaces::{Member, MemberAddress, SelectedMember};
use super::json_lines::write_json_line;
use crate::commands::action_poll::{CtrlC, Interrupts};
use crate::context::AppContext;
use crate::error::{Error, Result};
use config::node::{InterfaceKind, MessageFormat, NativeEmittedTopic};
use message_codec::MessageCodec;
use message_codec::consumer::{ConsumerIdentity, MemberBinding, TopicConsumer};
use peppylib::messaging::{MessengerHandle, ProducerRef};
use std::io::{ErrorKind, Write};
use std::sync::Arc;
use tracing::warn;

/// What `peppy node echo` prints.
pub struct EchoParams {
    pub instance_id: String,
    pub topic: String,
}

pub fn echo(ctx: &Arc<AppContext>, params: EchoParams) -> Result<()> {
    crate::commands::block_on(echo_async(ctx, params))
}

async fn echo_async(ctx: &Arc<AppContext>, params: EchoParams) -> Result<()> {
    let (echo, mut consumer) = subscribed_echo(ctx, &params).await?;
    echo.print(&mut consumer, &mut CtrlC::listen(), &mut std::io::stdout())
        .await
}

/// The echo `params` names, subscribed to its topic.
async fn subscribed_echo(
    ctx: &Arc<AppContext>,
    params: &EchoParams,
) -> Result<(TopicEcho, TopicConsumer)> {
    let conn = ctx.connect_to_daemon().await?;
    let interfaces = super::interfaces::fetch(&conn, &params.instance_id).await?;
    let selected = interfaces
        .select(&params.topic, &[InterfaceKind::Topic])
        .map_err(Error::ExecutionFailed)?;
    let echo = TopicEcho::new(&selected)?;
    let caller = super::interfaces::command_line_caller(&conn);
    let consumer = echo.subscribe(conn.messenger, &caller).await?;
    Ok((echo, consumer))
}

/// The first `count` messages that `peppy node echo` prints for `params`,
/// decoded: the subscription and the decoding the command makes, read
/// without a Ctrl-C to end them.
#[cfg(feature = "test-support")]
pub async fn echo_messages(
    ctx: &Arc<AppContext>,
    params: EchoParams,
    count: usize,
) -> Result<Vec<serde_json::Value>> {
    let (echo, mut consumer) = subscribed_echo(ctx, &params).await?;
    let mut messages = Vec::with_capacity(count);
    while messages.len() < count {
        let Some((_, message)) = consumer.next_message().await else {
            return Err(Error::ExecutionFailed(format!(
                "The subscription to `{}` ended",
                echo.name
            )));
        };
        messages.push(consumer.decode(&message).map_err(|error| {
            Error::ExecutionFailed(format!(
                "A message of `{}` does not convert: {error}",
                echo.name
            ))
        })?);
    }
    Ok(messages)
}

/// One topic of an instance, ready to subscribe to.
pub(crate) struct TopicEcho {
    /// The topic as `<qualifier>.<name>`, which the messages of the command
    /// name.
    name: String,
    topic: NativeEmittedTopic,
    address: MemberAddress,
    producer: ProducerRef,
    codec: MessageCodec,
}

impl TopicEcho {
    /// The echo of `selected`, its codec laid out from the format of the
    /// pinned manifest. A topic with no format carries no field, and each
    /// of its messages prints as an empty object.
    pub fn new(selected: &SelectedMember<'_>) -> Result<Self> {
        let name = selected.qualified_name.clone();
        let Member::Topic(topic) = selected.member else {
            return Err(Error::ExecutionFailed(format!(
                "`{name}` is a {}: `peppy node call` calls it",
                selected.member.kind()
            )));
        };
        let no_fields = MessageFormat::default();
        let format = topic.message_format.as_ref().unwrap_or(&no_fields);
        let codec = selected.codec("topic", format)?;
        Ok(Self {
            name,
            topic: topic.clone(),
            address: selected.address.clone(),
            producer: selected.producer.clone(),
            codec,
        })
    }

    /// Subscribes to the topic as `caller`.
    pub async fn subscribe(
        &self,
        messenger: &MessengerHandle,
        caller: &ConsumerIdentity,
    ) -> Result<TopicConsumer> {
        let subscribed = match &self.address {
            MemberAddress::Interface { target } => {
                TopicConsumer::of_producer(
                    messenger,
                    caller,
                    &self.binding(target),
                    &self.producer,
                    self.topic.qos_profile.clone(),
                    self.topic.retention.clone(),
                    self.codec.clone(),
                )
                .await
            }
            MemberAddress::Pairing { target, link_id } => {
                TopicConsumer::of_pairing_producer(
                    messenger,
                    caller,
                    &self.binding(target),
                    &self.producer,
                    link_id,
                    self.topic.qos_profile.clone(),
                    self.codec.clone(),
                )
                .await
            }
        };
        subscribed.map_err(|error| {
            Error::ExecutionFailed(format!("Cannot subscribe to `{}`: {error}", self.name))
        })
    }

    fn binding(&self, target: &peppylib::messaging::SenderTarget) -> MemberBinding {
        MemberBinding {
            target: target.clone(),
            member: self.topic.name.clone(),
        }
    }

    /// Writes each message `consumer` receives to `out` as a JSON line until
    /// the first Ctrl-C, or until the reader of `out` closes it. A message
    /// that does not convert is a warning.
    pub async fn print(
        &self,
        consumer: &mut TopicConsumer,
        interrupts: &mut impl Interrupts,
        out: &mut impl Write,
    ) -> Result<()> {
        loop {
            let message = tokio::select! {
                biased;
                () = interrupts.next() => return Ok(()),
                message = consumer.next_message() => message,
            };
            let Some((_, message)) = message else {
                return Err(Error::ExecutionFailed(format!(
                    "The subscription to `{}` ended",
                    self.name
                )));
            };
            let value = match consumer.decode(&message) {
                Ok(value) => value,
                Err(error) => {
                    warn!(
                        "A message of `{}` does not convert, and is not shown: {error}",
                        self.name
                    );
                    continue;
                }
            };
            match write_json_line(out, &value) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::BrokenPipe => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        }
    }
}

/// The echo of each kind of topic against publishers of the instance of
/// [`super::test_fixtures`] on the in-process mock messaging.
#[cfg(test)]
mod tests {
    use super::super::interfaces::InstanceInterfaces;
    use super::super::test_fixtures::{CORE_NODE, INSTANCE, documents, mock_messenger};
    use super::*;
    use crate::commands::action_poll::test_support::Presses;
    use config::node::{QoSProfile, TopicRetention};
    use peppylib::Payload;
    use peppylib::messaging::{SenderTarget, TopicMessenger, TopicPublisher};
    use serde_json::{Value, json};
    use tokio::sync::mpsc;

    /// A writer that hands each line it is given to the test.
    struct LineSink {
        line: Vec<u8>,
        lines: mpsc::UnboundedSender<Value>,
    }

    impl Write for LineSink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.line.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            let line = std::mem::take(&mut self.line);
            let text = std::str::from_utf8(&line).expect("UTF-8").trim_end();
            if !text.is_empty() {
                let value = serde_json::from_str(text).expect("a JSON line");
                let _ = self.lines.send(value);
            }
            Ok(())
        }
    }

    fn echo_of(topic: &str) -> TopicEcho {
        let interfaces = InstanceInterfaces::from_documents(&documents()).expect("the interfaces");
        let selected = interfaces
            .select(topic, &[InterfaceKind::Topic])
            .expect("the topic");
        TopicEcho::new(&selected).expect("the echo builds")
    }

    fn caller() -> ConsumerIdentity {
        ConsumerIdentity {
            core_node: "laptop".to_owned(),
            instance_id: "peppy-cli-test".to_owned(),
        }
    }

    fn payload(format: &str, value: Value) -> Payload {
        let codec = MessageCodec::new("test", serde_json5::from_str(format).expect("a format"))
            .expect("the format lays out");
        Payload::from(codec.encode(&value).expect("the value encodes"))
    }

    /// Prints what `echo` receives while `publisher` publishes `messages`,
    /// and stops at a press once it printed them all: the lines it printed.
    async fn echoed(
        echo: &TopicEcho,
        messenger: &MessengerHandle,
        publisher: &TopicPublisher,
        messages: Vec<Payload>,
    ) -> Vec<Value> {
        let mut consumer = echo
            .subscribe(messenger, &caller())
            .await
            .expect("the echo subscribes");
        let (lines_tx, mut lines_rx) = mpsc::unbounded_channel();
        let mut out = LineSink {
            line: Vec::new(),
            lines: lines_tx,
        };
        let (press, receiver) = mpsc::unbounded_channel();
        let mut presses = Presses::from_receiver(receiver);
        let count = messages.len();
        let (printed, lines) =
            tokio::join!(echo.print(&mut consumer, &mut presses, &mut out), async {
                for message in messages {
                    publisher
                        .publish(message)
                        .await
                        .expect("the message goes out");
                }
                let mut lines = Vec::new();
                while lines.len() < count {
                    lines.push(lines_rx.recv().await.expect("a printed line"));
                }
                press.send(()).expect("the echo reads the press");
                lines
            });
        printed.expect("a Ctrl-C ends the echo with success");
        lines
    }

    #[tokio::test]
    async fn the_echo_of_a_contract_topic_prints_each_message() {
        let messenger = mock_messenger().await;
        let publisher = TopicMessenger::declare_publisher(
            &messenger,
            CORE_NODE,
            INSTANCE,
            SenderTarget::contract("limb_motion", "v1").expect("a target"),
            None,
            "limb_states",
            QoSProfile::SensorData,
            TopicRetention::LiveOnly,
        )
        .await
        .expect("the publisher is declared");
        let format = r#"{ joints: { $type: "array", $items: "f64" } }"#;

        let lines = echoed(
            &echo_of("limb_states"),
            &messenger,
            &publisher,
            vec![
                payload(format, json!({"joints": [0.1]})),
                payload(format, json!({"joints": [0.2]})),
            ],
        )
        .await;

        assert_eq!(lines, [json!({"joints": [0.1]}), json!({"joints": [0.2]})]);
    }

    /// A topic of the node's own decodes with the format of its manifest:
    /// its `u64` field prints as a decimal string.
    #[tokio::test]
    async fn the_echo_of_an_own_topic_prints_each_message() {
        let messenger = mock_messenger().await;
        let publisher = TopicMessenger::declare_publisher(
            &messenger,
            CORE_NODE,
            INSTANCE,
            SenderTarget::node("arm", "v1").expect("a target"),
            None,
            "heartbeat",
            QoSProfile::Reliable,
            TopicRetention::LiveOnly,
        )
        .await
        .expect("the publisher is declared");

        let lines = echoed(
            &echo_of("arm.heartbeat"),
            &messenger,
            &publisher,
            vec![payload(
                r#"{ count: "u64" }"#,
                json!({"count": "18446744073709551615"}),
            )],
        )
        .await;

        assert_eq!(lines, [json!({"count": "18446744073709551615"})]);
    }

    /// A pairing topic is received as an observer of the pairs that the
    /// instance's slot holds, with no slot of a node of its own.
    #[tokio::test]
    async fn the_echo_of_a_pairing_topic_prints_each_message() {
        let messenger = mock_messenger().await;
        let publisher = TopicMessenger::declare_sole_peer_publisher(
            &messenger,
            CORE_NODE,
            INSTANCE,
            SenderTarget::pairing("arm_link", "v1").expect("a target"),
            "controller",
            "joint_states",
            QoSProfile::SensorData,
        )
        .await
        .expect("the publisher is declared");

        let lines = echoed(
            &echo_of("joint_states"),
            &messenger,
            &publisher,
            vec![payload(
                r#"{ positions: { $type: "array", $items: "f64" } }"#,
                json!({"positions": [1.0, 2.0]}),
            )],
        )
        .await;

        assert_eq!(lines, [json!({"positions": [1.0, 2.0]})]);
    }

    #[test]
    fn a_service_is_not_echoed() {
        let interfaces = InstanceInterfaces::from_documents(&documents()).expect("the interfaces");
        let selected = interfaces
            .select("check_move", &[InterfaceKind::Service])
            .expect("the service");
        let Err(error) = TopicEcho::new(&selected) else {
            panic!("a service has no echo");
        };
        assert_eq!(
            error.to_string(),
            "`limb_motion.check_move` is a service: `peppy node call` calls it"
        );
    }
}
