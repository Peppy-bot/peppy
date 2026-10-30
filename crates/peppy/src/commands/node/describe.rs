//! `peppy node describe <instance>`: prints the interfaces of a running
//! instance, read from its pinned documents: the members of its contracts,
//! the members of its node's own and the topics of its pairing slots, each
//! with its description, the message formats of its pinned manifest (the
//! descriptions of their fields included) and the name that `peppy node
//! call` and `echo` take, under the description of its document. `--json`
//! prints the same as one JSON document.

use super::interfaces::{InstanceInterfaces, Member, full_name};
use super::json_lines::is_closed_by_reader;
use crate::commands::prose;
use crate::context::AppContext;
use crate::error::{Error, Result};
use config::description::DocumentDescription;
use config::node::MessageFormat;
use std::fmt::Write as _;
use std::sync::Arc;

/// What `peppy node describe` prints.
pub struct DescribeParams {
    pub instance_id: String,
    pub json: bool,
}

pub fn describe(ctx: &Arc<AppContext>, params: DescribeParams) -> Result<()> {
    let text = crate::commands::block_on(describe_rendered(ctx, params))?;
    write_text(&mut std::io::stdout(), &text)
}

/// Writes `text` to `out`. A program that reads the output and closes it
/// before the end, such as `head`, ends the command as a success.
fn write_text(out: &mut impl std::io::Write, text: &str) -> Result<()> {
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Err(error) if is_closed_by_reader(&error) => Ok(()),
        written => Ok(written?),
    }
}

/// What `peppy node describe` prints for `params`.
pub async fn describe_rendered(ctx: &Arc<AppContext>, params: DescribeParams) -> Result<String> {
    let conn = ctx.connect_to_daemon().await?;
    let interfaces = super::interfaces::fetch(&conn, &params.instance_id).await?;
    if !params.json {
        return Ok(render(&interfaces, crate::terminal::stdout_width()));
    }
    let document = serde_json::to_string_pretty(&interfaces)
        .map_err(|error| Error::ExecutionFailed(format!("Cannot write the JSON: {error}")))?;
    Ok(format!("{document}\n"))
}

/// The text `peppy node describe` prints for `interfaces`, its prose
/// wrapped to `max_width` columns (`None` keeps each paragraph on one
/// line).
pub(crate) fn render(interfaces: &InstanceInterfaces, max_width: Option<usize>) -> String {
    let mut out = String::new();
    let node = &interfaces.node;
    let _ = writeln!(
        out,
        "Instance {} of {}:{} on core node {}",
        interfaces.instance_id, node.name, node.tag, interfaces.core_node
    );
    let _ = writeln!(out, "Manifest sha256: {}", node.sha256);
    write_description(&mut out, &node.description, max_width);
    for contract in &interfaces.contracts {
        let identity = &contract.contract;
        let _ = writeln!(
            out,
            "\nContract {}:{} through slot `{}` (sha256: {})",
            identity.name, identity.tag, contract.link_id, identity.sha256
        );
        write_description(&mut out, &identity.description, max_width);
        out.push('\n');
        write_members(
            &mut out,
            Some(&contract.link_id),
            &contract.members,
            max_width,
        );
    }
    if !interfaces.own_members.is_empty() {
        let _ = writeln!(out, "\nOwn members of {}:{}", node.name, node.tag);
        write_members(&mut out, None, &interfaces.own_members, max_width);
    }
    for pairing in &interfaces.pairings {
        let identity = &pairing.pairing;
        let _ = writeln!(
            out,
            "\nPairing {}:{} through slot `{}`, role `{}` (sha256: {})",
            identity.name, identity.tag, pairing.link_id, pairing.role, identity.sha256
        );
        write_description(&mut out, &identity.description, max_width);
        out.push('\n');
        write_members(&mut out, Some(&pairing.link_id), &pairing.topics, max_width);
    }
    if interfaces.contracts.is_empty()
        && interfaces.own_members.is_empty()
        && interfaces.pairings.is_empty()
    {
        let _ = writeln!(out, "\nThe instance serves no topic, service or action.");
    }
    out
}

/// The paragraphs of the description of a document, under its heading.
fn write_description(
    out: &mut String,
    description: &DocumentDescription,
    max_width: Option<usize>,
) {
    out.push_str(&prose::paragraphs(description, "  ", max_width));
}

/// Each member under the full name that `peppy node call` and `echo` take
/// for the members of the slot `slot`, or of the node's own, with its
/// description and its message formats.
fn write_members(
    out: &mut String,
    slot: Option<&str>,
    members: &[Member],
    max_width: Option<usize>,
) {
    for member in members {
        let name = full_name(slot, member.name());
        let description = prose::wrapped(member.description().as_str(), "    ", max_width);
        match member {
            Member::Topic(topic) => {
                let _ = writeln!(
                    out,
                    "  topic {name} (qos: {:?}{})",
                    topic.qos_profile,
                    super::info::retention_note(topic.retention)
                );
                out.push_str(&description);
                write_format(out, "message", topic.message_format.as_ref());
            }
            Member::Service(service) => {
                let _ = writeln!(out, "  service {name}");
                out.push_str(&description);
                write_format(
                    out,
                    "request",
                    service.service.request_message_format.as_ref(),
                );
                write_format(
                    out,
                    "response",
                    service.service.response_message_format.as_ref(),
                );
                write_failure_kinds(out, &service.failure_kinds);
            }
            Member::Action(action) => {
                let shape = &action.action;
                let _ = writeln!(out, "  action {name}");
                out.push_str(&description);
                write_format(
                    out,
                    "goal",
                    shape
                        .goal_service
                        .as_ref()
                        .and_then(|goal| goal.request_message_format.as_ref()),
                );
                write_format(
                    out,
                    "goal_response",
                    shape
                        .goal_service
                        .as_ref()
                        .and_then(|goal| goal.response_message_format.as_ref()),
                );
                write_format(
                    out,
                    "feedback",
                    shape
                        .feedback_topic
                        .as_ref()
                        .map(|feedback| &feedback.message_format),
                );
                write_format(
                    out,
                    "result",
                    shape
                        .result_service
                        .as_ref()
                        .and_then(|result| result.response_message_format.as_ref()),
                );
                write_failure_kinds(out, &action.failure_kinds);
            }
        }
    }
}

/// One message side, `label: <format as JSON5>`, the format indented under
/// the label; `label: none` for a side that carries no payload.
fn write_format(out: &mut String, label: &str, format: Option<&MessageFormat>) {
    let Some(format) = peppy_codec::payload_format(format) else {
        let _ = writeln!(out, "    {label}: none");
        return;
    };
    let text = json5_pretty::to_string_pretty(format)
        .unwrap_or_else(|error| format!("(the format cannot be written: {error})"));
    let mut lines = text.lines();
    let _ = writeln!(out, "    {label}: {}", lines.next().unwrap_or_default());
    for line in lines {
        let _ = writeln!(out, "    {line}");
    }
}

/// The kinds of its contract that a member gives, each with its meaning.
/// Every member can also give the kinds of peppy, which are not listed.
fn write_failure_kinds(out: &mut String, kinds: &config::failure_kind::MemberFailureKinds) {
    for (kind, meaning) in kinds.contract_kinds().iter() {
        let _ = writeln!(out, "    failure kind {kind}: {meaning}");
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_fixtures::documents;
    use super::*;

    /// Each member under the name `peppy node call` and `echo` take, with
    /// the formats of the pinned manifest, the refined goal included, and
    /// the kinds of its contract.
    #[test]
    fn the_text_names_each_member_as_call_and_echo_take_it_with_its_formats() {
        let documents = documents();
        let interfaces = InstanceInterfaces::from_documents(&documents).expect("the interfaces");
        let text = render(&interfaces, None);
        let expected = format!(
            r#"Instance arm_inst of arm:v1 on core node robot-7
Manifest sha256: {manifest}
  An arm on a bench.

Contract limb_motion:v1 through slot `motion` (sha256: {contract})
  The motion of a limb.

  Joints are in radians.

  topic motion.limb_states (qos: SensorData)
    The joints of the limb.
    message: {{
      joints: {{
        $type: "array",
        $items: "f64"
      }}
    }}
  service motion.check_move
    Check whether a move is reachable.
    request: {{
      joints: {{
        $type: "array",
        $items: "f64"
      }}
    }}
    response: {{
      reachable: "bool"
    }}
    failure kind unreachable: No plan to the target
  action motion.move_to
    Move the limb to the joints.
    goal: {{
      joints: {{
        $type: "array",
        $items: "f64",
        $length: 3
      }}
    }}
    goal_response: none
    feedback: {{
      progress: "f64"
    }}
    result: {{
      reached: "bool"
    }}
    failure kind unreachable: No plan to the target

Own members of arm:v1
  topic heartbeat (qos: Reliable)
    A count, once a second.
    message: {{
      count: "u64"
    }}
  service set_gain
    Set the gain of the arm.
    request: {{
      value: "i32"
    }}
    response: {{
      current_value: "i32"
    }}

Pairing arm_link:v1 through slot `controller`, role `arm` (sha256: {pairing})
  The link between an arm and its controller.

  topic controller.joint_states (qos: SensorData)
    The joints of the arm.
    message: {{
      positions: {{
        $type: "array",
        $items: "f64"
      }}
    }}
"#,
            manifest = documents.manifest.sha256(),
            contract = documents.contracts[0].sha256(),
            pairing = documents.pairings[0].sha256(),
        );
        assert_eq!(text, expected);
    }

    /// A writer whose reader closed it: each write fails as on a closed pipe.
    struct ClosedPipe;

    impl std::io::Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
    }

    /// `peppy node describe arm_inst | head -n 1` ends as a success: the
    /// reader closed the output once it had what it needed.
    #[test]
    fn an_output_that_its_reader_closes_ends_the_command_as_a_success() {
        assert!(write_text(&mut ClosedPipe, "Instance arm_inst\n").is_ok());

        let mut out = Vec::new();
        write_text(&mut out, "Instance arm_inst\n").expect("the text is written");
        assert_eq!(out, b"Instance arm_inst\n");
    }

    #[test]
    fn an_instance_with_no_interface_says_so() {
        let mut interfaces =
            InstanceInterfaces::from_documents(&documents()).expect("the interfaces");
        interfaces.contracts.clear();
        interfaces.own_members.clear();
        interfaces.pairings.clear();
        assert!(
            render(&interfaces, None)
                .ends_with("\nThe instance serves no topic, service or action.\n")
        );
    }
}
