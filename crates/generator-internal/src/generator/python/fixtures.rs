//! Python renderer for the generated `fixtures` category: the per-node
//! harness (`fixtures/harness.py`: ephemeral router + started mocks + seeded
//! `StandaloneConfig` + running node behind the readiness barriers) and typed
//! observation clients for the node's own surface (`emitted_topics`
//! subscriptions, `exposed_services` / `exposed_actions` identity-explicit
//! callers). Lifecycle semantics live in `peppylib.testing.HarnessCore`; this
//! file contributes only what is per-node: mock construction, seeding calls,
//! readiness lists, and typed codecs on the production schema keys.
//!
//! Skipped entirely when the backend was driven without
//! [`set_node_identity`](super::PythonGenerator::set_node_identity): the
//! harness cannot pin the node's own targets without the manifest identity.

use super::super::common::NodeTree;
use super::super::naming::non_empty_str;
use super::super::testgen::{
    DepLinkSpec, EmittedSpec, ExposedActionSpec, ExposedServiceSpec, FIXTURE_CORE_NODE,
    FIXTURE_INSTANCE_ID, TargetSpec, TestGenRegistry, claim_sanitized_name,
};
use super::PythonGenerator;
use super::code_builder::PythonCodeBuilder;
use super::mock::{
    emit_local_message_with_deserializer, emit_value_deserializer, emit_value_serializer,
    production_import_line, production_module_path, target_python_expr,
};
use super::scaffold::sanitize_python_module_name;
use super::type_mapping::qos_profile_python;
use crate::error::Result;
use crate::generator::types::{InterfaceArtifact, InterfaceKind, scoped_schema_key};
use config::node::Cardinality;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// One `PublisherReadiness(...)` entry's source lines: `link_id` only for
/// pairing-slot topics (the mock's own subscription link).
fn publisher_readiness_lines(target: &str, topic: &str, link_id: Option<&str>) -> Vec<String> {
    let mut lines = vec![
        "peppylib.testing.PublisherReadiness(".to_string(),
        format!("    target={target},"),
        format!("    topic={topic:?},"),
    ];
    if let Some(link_id) = link_id {
        lines.push(format!("    link_id={link_id:?},"));
    }
    lines.push("),".to_string());
    lines
}

/// `attr=local` kwargs for one mocks group: every local binding is the
/// group-prefixed attr (`dep_x = ...`), so the pair derives from the attr
/// alone instead of being tracked in a parallel vector.
fn mock_kwargs(attrs: &[String], prefix: &str) -> String {
    attrs
        .iter()
        .map(|attr| format!("{attr}={prefix}_{attr}"))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(super) fn render(
    generator: &mut PythonGenerator,
    registry: &TestGenRegistry,
    to_path: &Path,
) -> Result<()> {
    let Some((node_name, node_tag)) = registry.node_identity.clone() else {
        return Ok(());
    };

    let mut emitted_members: Vec<EmittedMember> = Vec::new();
    let mut owners: HashMap<String, String> = HashMap::new();
    for spec in &registry.own.emitted {
        let (artifact, member) = render_emitted_topic(generator, &node_name, &node_tag, spec)?;
        claim_sanitized_name(&mut owners, &member.attr, "fixtures emitted", &spec.name)?;
        generator.push_section(artifact);
        emitted_members.push(member);
    }

    for spec in &registry.own.services {
        let artifact = render_exposed_service(generator, &node_name, &node_tag, spec)?;
        generator.push_section(artifact);
    }

    for spec in &registry.own.actions {
        let artifact = render_exposed_action(generator, &node_name, &node_tag, spec)?;
        generator.push_section(artifact);
    }

    let node_dir = match generator.node_tree {
        NodeTree::Source => Some(sync_time_node_dir(to_path)),
        NodeTree::Staged => None,
    };
    let harness = render_harness(
        &generator.parameters,
        registry,
        &node_name,
        &node_tag,
        &emitted_members,
        node_dir.as_deref(),
    )?;
    generator.push_section(InterfaceArtifact {
        module_path: vec!["harness".to_string()],
        kind: InterfaceKind::Fixture,
        code_output: harness,
    });
    Ok(())
}

/// The absolute node directory baked at generation time into a
/// [`NodeTree::Source`] harness: the canonicalized output path minus the
/// trailing `.peppy/libs/peppygen`. Last-resort fallback for the harness's
/// node-dir resolution (after the explicit argument and the cwd walk).
fn sync_time_node_dir(to_path: &Path) -> String {
    let canonical = std::fs::canonicalize(to_path).unwrap_or_else(|_| to_path.to_path_buf());
    let node_dir = if canonical.ends_with(config::consts::PEPPYGEN_OUTPUT_PATH) {
        canonical
            .ancestors()
            .nth(
                Path::new(config::consts::PEPPYGEN_OUTPUT_PATH)
                    .components()
                    .count(),
            )
            .map(Path::to_path_buf)
            .unwrap_or_else(|| canonical.clone())
    } else {
        canonical
    };
    node_dir.to_string_lossy().into_owned()
}

/// The node's own publish/serve identity for a surface, baked at generation
/// time: its contract target when contract-routed, else the manifest target.
fn own_target_expr(
    node_name: &str,
    node_tag: &str,
    origin: Option<&crate::generator::types::ContractOrigin>,
) -> String {
    match origin {
        Some(origin) => target_python_expr(&TargetSpec::Contract {
            name: origin.contract_name.clone(),
            tag: origin.contract_tag.clone(),
        }),
        None => target_python_expr(&TargetSpec::Node {
            name: node_name.to_string(),
            tag: node_tag.to_string(),
        }),
    }
}

fn emit_fixture_identity(builder: &mut PythonCodeBuilder) {
    builder.line(&format!("FIXTURE_CORE_NODE = {FIXTURE_CORE_NODE:?}"));
    builder.line(&format!("FIXTURE_INSTANCE_ID = {FIXTURE_INSTANCE_ID:?}"));
    builder.blank_line();
}

// ---------------------------------------------------------------------------
// emitted topics
// ---------------------------------------------------------------------------

struct EmittedMember {
    /// Sanitized attribute name on `Emitted` (and `_emit_{attr}` alias).
    attr: String,
    /// Dotted import path of the fixtures module.
    module: String,
    topic: String,
    target_expr: String,
}

fn render_emitted_topic(
    generator: &mut PythonGenerator,
    node_name: &str,
    node_tag: &str,
    spec: &EmittedSpec,
) -> Result<(InterfaceArtifact, EmittedMember)> {
    let topic_name = spec.name.as_str();
    // Key derivation copied from the Rust fixtures renderer, which itself
    // copies the production emitted-topic key: both resolve to the same capnp
    // file stem, so no new schema file appears.
    let fn_name =
        crate::generator::rust::identifiers::prefixed_name("", non_empty_str(topic_name), "topic");
    let schema_key = scoped_schema_key(spec.origin.as_ref(), &fn_name);
    let target = own_target_expr(node_name, node_tag, spec.origin.as_ref());
    let qos = qos_profile_python(&spec.qos);

    let mut builder = PythonCodeBuilder::new();
    builder.add_import("import peppylib");
    builder.add_import("from typing import Optional");
    let mut loaders = HashSet::new();

    builder.line(&format!("TOPIC_NAME = {topic_name:?}"));
    emit_fixture_identity(&mut builder);

    // The emitted direction has no production consumer dataclass (the
    // node-side module only serializes), so the fixtures module defines its
    // own.
    emit_local_message_with_deserializer(
        generator,
        &mut builder,
        &mut loaders,
        &schema_key,
        &spec.format,
        "Message",
        "_deserialize_message",
    )?;

    builder.py(r#"
class Subscription:
    """A held subscription to the node's emissions on this topic."""

    def __init__(self, inner) -> None:
        self._inner = inner

    async def next(self) -> Optional[Message]:
        """Awaits the node's next message on this topic; `None` once the fixture session closes."""
        message = await self._inner.on_next_message()
        if message is None:
            return None
        return _deserialize_message(message.payload)
"#);
    builder.blank_line();

    builder.block(
        "async def subscribe(session, node_instance_id: str) -> Subscription:",
        |builder| {
            builder.docstring(
                "Opens the observation subscription, pinned to the node under test; the \
                 harness calls this before the node boots and barriers on it, so the node's \
                 very first publish is captured.",
            );
            builder.line(
                "producer = peppylib.ProducerRef(peppylib.testing.STANDALONE_CORE_NODE, node_instance_id)",
            );
            builder.call(
                "inner = await peppylib.TopicMessenger.subscribe(",
                &[
                    "session,",
                    "FIXTURE_CORE_NODE,",
                    "FIXTURE_INSTANCE_ID,",
                    &format!("{target},"),
                    "TOPIC_NAME,",
                    "producer,",
                    &format!("{qos},"),
                ],
                ")",
            );
            builder.line("return Subscription(inner)");
        },
    );

    let mut module_path = vec!["emitted_topics".to_string()];
    match &spec.origin {
        Some(origin) => module_path.extend(origin.module_path_for(topic_name)),
        None => module_path.push(topic_name.to_string()),
    }
    let module = {
        let segments: Vec<&str> = module_path.iter().map(String::as_str).collect();
        production_module_path("fixtures", &segments)
    };
    let doc = format!(
        "Typed observation of the node's emitted topic `{topic_name}`: the harness \
         subscribes before the node boots (pinned to the node's identity) and barriers \
         on the subscription, so the node's very first publish is captured."
    );
    let code = format!("\"\"\"{doc}\"\"\"\n\n{}", builder.build());
    Ok((
        InterfaceArtifact {
            module_path,
            kind: InterfaceKind::Fixture,
            code_output: code,
        },
        EmittedMember {
            // Contract-backed topics nest under their link_id in the tree;
            // the flat Emitted attr mirrors that scoping so same-name topics
            // from different links coexist.
            attr: match &spec.origin {
                Some(origin) => {
                    sanitize_python_module_name(&format!("{}_{}", origin.link_id, topic_name))
                }
                None => sanitize_python_module_name(topic_name),
            },
            module,
            topic: topic_name.to_string(),
            target_expr: target,
        },
    ))
}

// ---------------------------------------------------------------------------
// exposed services
// ---------------------------------------------------------------------------

fn render_exposed_service(
    generator: &mut PythonGenerator,
    node_name: &str,
    node_tag: &str,
    spec: &ExposedServiceSpec,
) -> Result<InterfaceArtifact> {
    let service_name = spec.name.as_str();
    // Same keys the Python production module registered, so the codecs land
    // on the identical capnp files.
    let request_key = scoped_schema_key(spec.origin.as_ref(), &format!("{service_name}_request"));
    let response_key = scoped_schema_key(spec.origin.as_ref(), &format!("{service_name}_response"));
    let target = own_target_expr(node_name, node_tag, spec.origin.as_ref());

    let mut module_path = vec!["exposed_services".to_string()];
    match &spec.origin {
        Some(origin) => module_path.extend(origin.module_path_for(service_name)),
        None => module_path.push(service_name.to_string()),
    }
    let production = {
        let segments: Vec<&str> = module_path[1..].iter().map(String::as_str).collect();
        production_module_path("exposed_services", &segments)
    };

    let mut builder = PythonCodeBuilder::new();
    builder.add_import("import peppylib");
    builder.add_import("import peppylib.testing");
    builder.add_import(&production_import_line(&production, "_production"));
    let mut loaders = HashSet::new();

    builder.line(&format!("SERVICE_NAME = {service_name:?}"));
    emit_fixture_identity(&mut builder);

    let request_param = if let Some(format) = &spec.request {
        emit_value_serializer(
            generator,
            &mut builder,
            &mut loaders,
            "_serialize_request",
            &request_key,
            format,
            "_production.RequestData",
        )?;
        builder.line("RequestData = _production.RequestData");
        builder.blank_line();
        ", request: _production.RequestData"
    } else {
        ""
    };
    let return_ty = if let Some(format) = &spec.response {
        emit_value_deserializer(
            generator,
            &mut builder,
            &mut loaders,
            "_deserialize_response",
            &response_key,
            format,
            "_production.Response",
        )?;
        builder.line("Response = _production.Response");
        builder.blank_line();
        "_production.Response"
    } else {
        "None"
    };

    builder.block(
        &format!("async def poll(harness{request_param}, timeout: float) -> {return_ty}:"),
        |builder| {
            builder.docstring(&format!(
                "Polls the node's exposed service `{service_name}` once from the fixture \
                 session (a fresh, identity-explicit caller pinned to the node under test), \
                 gating on reachability first. `timeout` bounds the reachability gate and the \
                 poll individually."
            ));
            builder.line("producer = harness.node_producer_ref()");
            builder.call(
                "await peppylib.testing.wait_service_reachable(",
                &[
                    "harness.session,",
                    "FIXTURE_CORE_NODE,",
                    "FIXTURE_INSTANCE_ID,",
                    &format!("{target},"),
                    "SERVICE_NAME,",
                    "producer,",
                    "timeout,",
                ],
                ")",
            );
            let request_payload = if spec.request.is_some() {
                "_serialize_request(request)"
            } else {
                "b\"\""
            };
            builder.call(
                if spec.response.is_some() {
                    "response_message = await peppylib.ServiceMessenger.poll("
                } else {
                    "await peppylib.ServiceMessenger.poll("
                },
                &[
                    "harness.session,",
                    "FIXTURE_CORE_NODE,",
                    "FIXTURE_INSTANCE_ID,",
                    &format!("{target},"),
                    "SERVICE_NAME,",
                    "producer,",
                    &format!("{request_payload},"),
                    "timeout,",
                ],
                ")",
            );
            builder.line(if spec.response.is_some() {
                "return _deserialize_response(response_message.payload)"
            } else {
                "return None"
            });
        },
    );

    let doc = format!(
        "Identity-explicit caller for the node's exposed service `{service_name}`: \
         polls from the fixture session, pinned to the node under test, gating on \
         reachability first (the fixture session is a fresh caller)."
    );
    Ok(InterfaceArtifact {
        module_path,
        kind: InterfaceKind::Fixture,
        code_output: format!("\"\"\"{doc}\"\"\"\n\n{}", builder.build()),
    })
}

// ---------------------------------------------------------------------------
// exposed actions
// ---------------------------------------------------------------------------

fn render_exposed_action(
    generator: &mut PythonGenerator,
    node_name: &str,
    node_tag: &str,
    spec: &ExposedActionSpec,
) -> Result<InterfaceArtifact> {
    let action_name = spec.name.as_str();
    // Same keys the Python production module registered.
    let goal_key = scoped_schema_key(spec.origin.as_ref(), &format!("{action_name}_goal_request"));
    let goal_response_key = scoped_schema_key(
        spec.origin.as_ref(),
        &format!("{action_name}_goal_response"),
    );
    let feedback_key = scoped_schema_key(spec.origin.as_ref(), &format!("{action_name}_feedback"));
    let result_key = scoped_schema_key(
        spec.origin.as_ref(),
        &format!("{action_name}_result_response"),
    );
    let target = own_target_expr(node_name, node_tag, spec.origin.as_ref());

    let mut module_path = vec!["exposed_actions".to_string()];
    match &spec.origin {
        Some(origin) => module_path.extend(origin.module_path_for(action_name)),
        None => module_path.push(action_name.to_string()),
    }
    let production = {
        let segments: Vec<&str> = module_path[1..].iter().map(String::as_str).collect();
        production_module_path("exposed_actions", &segments)
    };

    let mut builder = PythonCodeBuilder::new();
    builder.add_import("import peppylib");
    builder.add_import("import peppylib.testing");
    builder.add_import("from enum import IntEnum");
    builder.add_import(&production_import_line(&production, "_production"));
    let mut loaders = HashSet::new();

    builder.line(&format!("ACTION_NAME = {action_name:?}"));
    emit_fixture_identity(&mut builder);

    if let Some(format) = &spec.goal_request {
        emit_value_serializer(
            generator,
            &mut builder,
            &mut loaders,
            "_serialize_goal_request",
            &goal_key,
            format,
            "_production.GoalRequestData",
        )?;
        builder.line("GoalRequestData = _production.GoalRequestData");
        builder.blank_line();
    }
    if let Some(format) = &spec.goal_response {
        emit_value_deserializer(
            generator,
            &mut builder,
            &mut loaders,
            "_deserialize_goal_response",
            &goal_response_key,
            format,
            "_production.GoalResponse",
        )?;
        builder.line("GoalResponse = _production.GoalResponse");
        builder.blank_line();
    }

    // Cancel-ack states, mirroring the runtime's framework envelope (decoded
    // engine-side; no per-action schema).
    builder.py(r#"
class CancelState(IntEnum):
    SIGNALLED = 0
    ALREADY_TERMINAL = 1
    UNKNOWN = 2
"#);
    builder.blank_line();
    builder.dataclass(
        "CancelResponse",
        &[
            ("core_node", "str"),
            ("instance_id", "str"),
            ("state", "CancelState"),
        ],
    );

    builder.py(r#"
class ResultStatus(IntEnum):
    COMPLETED = 0
    CANCELLED = 1
    ABANDONED = 2
    EXPIRED = 3
"#);
    builder.blank_line();

    // Result data: fixtures deserialize; production defines no struct.
    if let Some(format) = &spec.result {
        emit_local_message_with_deserializer(
            generator,
            &mut builder,
            &mut loaders,
            &result_key,
            format,
            "ResultData",
            "_deserialize_result",
        )?;
        builder.add_import("from typing import Optional");
        builder.dataclass(
            "ResultResponse",
            &[
                ("core_node", "str"),
                ("instance_id", "str"),
                ("status", "ResultStatus"),
                ("data", "Optional[ResultData]"),
            ],
        );
    } else {
        builder.dataclass(
            "ResultResponse",
            &[
                ("core_node", "str"),
                ("instance_id", "str"),
                ("status", "ResultStatus"),
            ],
        );
    }

    // Feedback: fixtures deserialize; production defines no struct.
    if let Some(format) = &spec.feedback {
        emit_local_message_with_deserializer(
            generator,
            &mut builder,
            &mut loaders,
            &feedback_key,
            format,
            "Feedback",
            "_deserialize_feedback",
        )?;
    }

    builder.block("class GoalHandle:", |builder| {
        builder.docstring(&format!(
            "A goal in flight against the node's action `{action_name}`; construct \
             with `send_goal`. Attributes: `accepted` (whether the node admitted the \
             goal), `reason` (optional human-readable rejection reason){}.",
            if spec.goal_response.is_some() {
                ", `data` (the decoded goal response; `None` when a rejection carried no payload)"
            } else {
                ""
            }
        ));
        builder.blank_line();
        if spec.feedback.is_some() {
            builder.py(r#"
async def on_next_feedback(self) -> Feedback:
    """Receives the next decoded feedback message for this goal.

    Raises RuntimeError when the node closed the stream cleanly
    (end-of-stream sentinel) and ConnectionError when the node instance
    disappeared without closing it; in the latter case get_result
    resolves to ResultStatus.ABANDONED.
    """
    feedback = await self._inner.on_next_feedback()
    return _deserialize_feedback(feedback.payload)
"#);
            builder.blank_line();
        }
        builder.py(r#"
async def cancel_goal(self, timeout: float) -> CancelResponse:
    """Requests cancellation of this goal."""
    reply = await peppylib.ActionMessenger.cancel_goal(
        self._messenger,
        self._inner,
        timeout,
    )
    return CancelResponse(
        core_node=reply.core_node,
        instance_id=reply.instance_id,
        state=CancelState(reply.state),
    )
"#);
        builder.blank_line();
        builder.block(
            "async def get_result(self, timeout: float) -> ResultResponse:",
            |builder| {
                builder.py(r#"
"""Retrieves the goal's terminal result."""
reply = await peppylib.ActionMessenger.request_result(
    self._messenger,
    self._inner,
    timeout,
)
status = ResultStatus(reply.status)
"#);
                // A result-bearing action decodes the body, and only on the two
                // terminal states that carry one.
                if spec.result.is_some() {
                    builder.py(r#"
data = None
if status in (ResultStatus.COMPLETED, ResultStatus.CANCELLED):
    data = _deserialize_result(reply.body)
return ResultResponse(
    core_node=reply.core_node,
    instance_id=reply.instance_id,
    status=status,
    data=data,
)
"#);
                } else {
                    builder.py(r#"
return ResultResponse(
    core_node=reply.core_node,
    instance_id=reply.instance_id,
    status=status,
)
"#);
                }
            },
        );
    });
    builder.blank_line();

    let request_param = if spec.goal_request.is_some() {
        ", request: _production.GoalRequestData"
    } else {
        ""
    };
    builder.block(
        &format!(
            "async def send_goal(harness{request_param}, feedback_qos: peppylib.QoSProfile, \
             timeout: float) -> GoalHandle:"
        ),
        |builder| {
            builder.docstring(
                "Sends a goal to the node from the fixture session (a fresh, \
                 identity-explicit caller pinned to the node under test), gating on \
                 reachability first, and decodes the admission reply.",
            );
            builder.line("producer = harness.node_producer_ref()");
            builder.call(
                "await peppylib.testing.wait_action_reachable(",
                &[
                    "harness.session,",
                    "FIXTURE_CORE_NODE,",
                    "FIXTURE_INSTANCE_ID,",
                    &format!("{target},"),
                    "ACTION_NAME,",
                    "producer,",
                    "timeout,",
                ],
                ")",
            );
            builder.line(if spec.goal_request.is_some() {
                "user_goal_payload = _serialize_goal_request(request)"
            } else {
                "user_goal_payload = b\"\""
            });
            builder.call(
                "inner = await peppylib.ActionMessenger.send_goal(",
                &[
                    "harness.session,",
                    "FIXTURE_CORE_NODE,",
                    "FIXTURE_INSTANCE_ID,",
                    &format!("{target},"),
                    "ACTION_NAME,",
                    "producer,",
                    "user_goal_payload,",
                    "feedback_qos,",
                    "timeout,",
                ],
                ")",
            );
            builder.py(r#"
handle = GoalHandle()
handle._messenger = harness.session
handle._inner = inner
handle.accepted = inner.accepted
handle.reason = inner.reason
"#);
            if spec.goal_response.is_some() {
                // An empty body means no response was supplied (a declared
                // response serializes to a non-empty capnp message), which only
                // a reject can produce.
                builder.py(r#"
body = inner.goal_reply_body
handle.data = _deserialize_goal_response(body) if body else None
"#);
            }
            builder.line("return handle");
        },
    );

    let doc = format!(
        "Identity-explicit caller for the node's exposed action `{action_name}`: drives \
         the full goal lifecycle from the fixture session, pinned to the node under \
         test, gating on reachability first."
    );
    Ok(InterfaceArtifact {
        module_path,
        kind: InterfaceKind::Fixture,
        code_output: format!("\"\"\"{doc}\"\"\"\n\n{}", builder.build()),
    })
}

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

/// One extra `start(...)` keyword for a slot whose cardinality admits
/// overrides.
struct SlotKwarg {
    name: String,
    default: String,
    doc: String,
}

/// The `_DEFAULT_PARAMETERS` dict literal, synthesized from the schema's
/// declared defaults; `None` when any reachable leaf lacks one (the node's
/// own boot validation then raises the canonical missing-parameter error).
fn default_parameters_literal(parameters: &config::ParameterSchema) -> Option<String> {
    let mut parts = Vec::new();
    for (key, spec) in parameters {
        parts.push(format!(
            "{}: {}",
            serde_json::to_string(key).ok()?,
            default_spec_literal(spec)?
        ));
    }
    Some(format!("{{{}}}", parts.join(", ")))
}

fn default_spec_literal(spec: &config::ParameterSpec) -> Option<String> {
    match spec {
        config::ParameterSpec::Primitive {
            default: Some(default),
            ..
        } => {
            let value = serde_json::to_value(default).ok()?;
            Some(python_json_literal(&value))
        }
        config::ParameterSpec::Primitive { default: None, .. } => None,
        config::ParameterSpec::Array { .. } => None,
        config::ParameterSpec::Group(fields) => {
            let mut parts = Vec::new();
            for (key, sub) in fields {
                parts.push(format!(
                    "{}: {}",
                    serde_json::to_string(key).ok()?,
                    default_spec_literal(sub)?
                ));
            }
            Some(format!("{{{}}}", parts.join(", ")))
        }
    }
}

/// A JSON value as a Python literal (JSON string/number literals are valid
/// Python; the booleans and null differ).
fn python_json_literal(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Bool(true) => "True".to_string(),
        serde_json::Value::Bool(false) => "False".to_string(),
        serde_json::Value::Null => "None".to_string(),
        other => other.to_string(),
    }
}

/// Emits a namespace class holding one attribute per link.
fn emit_namespace_class(
    builder: &mut PythonCodeBuilder,
    class_name: &str,
    doc: &str,
    attrs: &[String],
) {
    builder.block(&format!("class {class_name}:"), |builder| {
        builder.docstring(doc);
        if !attrs.is_empty() {
            builder.blank_line();
            let mut params = vec!["self".to_string()];
            params.extend(attrs.iter().cloned());
            builder.block(
                &format!("def __init__({}) -> None:", params.join(", ")),
                |builder| {
                    builder.lines(attrs.iter().map(|attr| format!("self.{attr} = {attr}")));
                },
            );
        }
    });
    builder.blank_line();
}

#[allow(clippy::too_many_arguments)]
fn render_harness(
    parameters: &config::ParameterSchema,
    registry: &TestGenRegistry,
    node_name: &str,
    node_tag: &str,
    emitted_members: &[EmittedMember],
    node_dir: Option<&str>,
) -> Result<String> {
    let mut builder = PythonCodeBuilder::new();
    builder.add_import("import os");
    builder.add_import("import peppylib");
    builder.add_import("import peppylib.testing");
    builder.add_import("from peppygen import parameters as _parameters");

    let mut slot_kwargs: Vec<SlotKwarg> = Vec::new();
    let mut mock_starts: Vec<String> = Vec::new();
    let mut seeding: Vec<String> = Vec::new();
    let mut publisher_readiness: Vec<Vec<String>> = Vec::new();
    let mut service_readiness: Vec<String> = Vec::new();
    let mut dep_attrs: Vec<String> = Vec::new();
    let mut pairing_attrs: Vec<String> = Vec::new();
    let mut observed_attrs: Vec<String> = Vec::new();

    for (link_id, spec) in &registry.deps {
        let attr = sanitize_python_module_name(link_id);
        let alias = format!("_dep_{attr}");
        builder.add_import(&production_import_line(
            &production_module_path("mock", &["deps", link_id]),
            &alias,
        ));
        let local = format!("dep_{attr}");
        render_dep_harness_parts(
            link_id,
            spec,
            &attr,
            &alias,
            &local,
            &mut slot_kwargs,
            &mut mock_starts,
            &mut seeding,
            &mut service_readiness,
        );
        dep_attrs.push(attr);
    }

    for (link_id, spec) in &registry.pairings {
        let attr = sanitize_python_module_name(link_id);
        let alias = format!("_pair_{attr}");
        builder.add_import(&production_import_line(
            &production_module_path("mock", &["pairings", link_id]),
            &alias,
        ));
        let local = format!("pair_{attr}");
        mock_starts.push(format!(
            "{local} = await {alias}.Mock.start(router, instance_id)"
        ));
        if spec.optional {
            // The mock still starts when vacant: its pinned subscription is
            // what resolves the publisher-readiness barrier, and an unpaired
            // node's publishes on the slot are legal no-ops, so only the
            // pin seeding is withheld.
            let kwarg = format!("{attr}_vacant");
            slot_kwargs.push(SlotKwarg {
                name: kwarg.clone(),
                default: "False".to_string(),
                doc: format!(
                    "{kwarg}: boot with the optional `{link_id}` pairing slot \
                     unpaired (the peer pin is not seeded; the still-started \
                     mock's subscriptions stay silent)."
                ),
            });
            seeding.push(format!("if not {kwarg}:"));
            seeding.push(format!(
                "    standalone = standalone.with_peer_pin({link_id:?}, \
                 {alias}.MOCK_CORE_NODE, {alias}.MOCK_INSTANCE_ID, {alias}.PEER_LINK_ID)"
            ));
        } else {
            seeding.push(format!(
                "standalone = standalone.with_peer_pin({link_id:?}, {alias}.MOCK_CORE_NODE, \
                 {alias}.MOCK_INSTANCE_ID, {alias}.PEER_LINK_ID)"
            ));
        }
        // The mock subscribes to every topic the node emits on this slot;
        // barrier on each so the node's first publish routes.
        let pairing_target = target_python_expr(&TargetSpec::Pairing {
            name: spec.pairing_name.clone(),
            tag: spec.pairing_tag.clone(),
        });
        for topic in &spec.node_emits {
            publisher_readiness.push(publisher_readiness_lines(
                &pairing_target,
                &topic.name,
                Some(link_id),
            ));
        }
        pairing_attrs.push(attr);
    }

    for (link_id, spec) in &registry.observed {
        let attr = sanitize_python_module_name(link_id);
        let alias = format!("_obs_{attr}");
        builder.add_import(&production_import_line(
            &production_module_path("mock", &["observed", link_id]),
            &alias,
        ));
        let local = format!("obs_{attr}");
        match spec.cardinality {
            Cardinality::One => {
                mock_starts.push(format!("{local} = await {alias}.Mock.start(router)"));
                seeding.push(format!(
                    "standalone = standalone.with_observed_source({link_id:?}, \
                     {alias}.MOCK_CORE_NODE, {alias}.MOCK_INSTANCE_ID, {alias}.SOURCE_LINK_ID)"
                ));
            }
            Cardinality::ZeroOrOne => {
                let kwarg = format!("{attr}_vacant");
                slot_kwargs.push(SlotKwarg {
                    name: kwarg.clone(),
                    default: "False".to_string(),
                    doc: format!(
                        "{kwarg}: leave the `{link_id}` observer slot empty (its \
                         cardinality admits an empty set)."
                    ),
                });
                mock_starts.push(format!("{local} = None"));
                mock_starts.push(format!("if not {kwarg}:"));
                mock_starts.push(format!("    {local} = await {alias}.Mock.start(router)"));
                seeding.push(format!("if not {kwarg}:"));
                seeding.push(format!(
                    "    standalone = standalone.with_observed_source({link_id:?}, \
                     {alias}.MOCK_CORE_NODE, {alias}.MOCK_INSTANCE_ID, {alias}.SOURCE_LINK_ID)"
                ));
            }
            Cardinality::OneOrMore | Cardinality::ZeroOrMore => {
                let kwarg = format!("{attr}_instances");
                let default_count: usize = match spec.cardinality {
                    Cardinality::OneOrMore => 1,
                    _ => 0,
                };
                slot_kwargs.push(SlotKwarg {
                    name: kwarg.clone(),
                    default: default_count.to_string(),
                    doc: format!(
                        "{kwarg}: how many mock sources to start for the `{link_id}` \
                         observer slot."
                    ),
                });
                let ids_kwarg = format!("{attr}_instance_ids");
                slot_kwargs.push(SlotKwarg {
                    name: ids_kwarg.clone(),
                    default: "None".to_string(),
                    doc: format!(
                        "{ids_kwarg}: explicit instance ids for the `{link_id}` mock \
                         sources, overriding `{kwarg}` when given. For nodes that \
                         classify sources by instance name."
                    ),
                });
                let ids_local = format!("{local}_member_ids");
                mock_starts.push(format!(
                    "{ids_local} = list({ids_kwarg}) if {ids_kwarg} is not None else \
                     [f\"{{{alias}.MOCK_INSTANCE_ID}}-{{index}}\" for index in \
                     range({kwarg})]"
                ));
                mock_starts.push(format!("{local} = []"));
                mock_starts.push(format!("for member_instance_id in {ids_local}:"));
                mock_starts.push(format!(
                    "    {local}.append(await {alias}.Mock.start(router, member_instance_id))"
                ));
                seeding.push(format!("for member_instance_id in {ids_local}:"));
                seeding.push(format!(
                    "    standalone = standalone.with_observed_source({link_id:?}, \
                     {alias}.MOCK_CORE_NODE, member_instance_id, {alias}.SOURCE_LINK_ID)"
                ));
            }
        }
        observed_attrs.push(attr);
    }

    // Emitted-topic subscriptions: opened on the fixture session before the
    // node boots, then barriered on.
    let mut emitted_subscribes: Vec<String> = Vec::new();
    let mut emitted_attrs: Vec<String> = Vec::new();
    for member in emitted_members {
        let alias = format!("_emit_{}", member.attr);
        builder.add_import(&production_import_line(&member.module, &alias));
        let local = format!("emitted_{}", member.attr);
        emitted_subscribes.push(format!(
            "{local} = await {alias}.subscribe(session, instance_id)"
        ));
        publisher_readiness.push(publisher_readiness_lines(
            &member.target_expr,
            &member.topic,
            None,
        ));
        emitted_attrs.push(member.attr.clone());
    }

    // -----------------------------------------------------------------
    // Module body
    // -----------------------------------------------------------------

    builder.line(&format!("FIXTURE_CORE_NODE = {FIXTURE_CORE_NODE:?}"));
    builder.line(&format!("FIXTURE_INSTANCE_ID = {FIXTURE_INSTANCE_ID:?}"));
    builder.line(&format!("NODE_NAME = {node_name:?}"));
    builder.line(&format!("NODE_TAG = {node_tag:?}"));
    builder.line("_NODE_CONFIG_FILE = \"peppy.json5\"");
    builder.line(&match node_dir {
        Some(node_dir) => format!("_SYNC_TIME_NODE_DIR = {node_dir:?}"),
        None => "_SYNC_TIME_NODE_DIR = None".to_string(),
    });
    match default_parameters_literal(parameters) {
        Some(literal) => builder.line(&format!("_DEFAULT_PARAMETERS = {literal}")),
        None => builder.py(r#"
# At least one parameter has no schema default: `parameters=None`
# defers to the node's own boot validation, which raises the
# canonical missing-parameter error before `setup` runs.
_DEFAULT_PARAMETERS = None
"#),
    }
    builder.blank_line();

    builder.py(r#"
def _hydrate_parameters(parameters):
    """The typed Parameters `setup` receives: the caller's override
    verbatim, or the schema defaults hydrated the way `NodeBuilder().run`
    would hydrate them."""
    if parameters is not None:
        return parameters
    if _DEFAULT_PARAMETERS is None:
        return None
    return _parameters.Parameters.from_dict(_DEFAULT_PARAMETERS)
"#);
    builder.blank_line();

    emit_namespace_class(
        &mut builder,
        "DepMocks",
        "One attribute per mocked dependency slot.",
        &dep_attrs,
    );
    emit_namespace_class(
        &mut builder,
        "PairingMocks",
        "One attribute per mocked pairing slot.",
        &pairing_attrs,
    );
    emit_namespace_class(
        &mut builder,
        "ObservedMocks",
        "One attribute per mocked observer slot.",
        &observed_attrs,
    );

    builder.py(r#"
# Aggregates the three namespace classes above and tears the whole set down.
# Node-invariant, so it lives in peppylib.testing; bound here under its
# historical name so `harness.mocks`'s type stays importable from this module.
Mocks = peppylib.testing.Mocks
"#);
    builder.blank_line();

    emit_namespace_class(
        &mut builder,
        "Emitted",
        "Typed subscriptions to the node's emitted topics, opened before the node \
         booted (so its very first publish is captured).",
        &emitted_attrs,
    );

    builder.block("class Harness:", |builder| {
        builder.docstring(
            "Generated test harness: ephemeral router + started mocks + seeded \
             `StandaloneConfig` + the node running in-process behind the readiness \
             barriers. Construct with `start(...)`; tear down with `await shutdown()` (or \
             let `async with start(...)` do it).",
        );
        builder.blank_line();
        builder.py(r#"
def __init__(self, core, mocks, emitted, clock, session, router, instance_id) -> None:
    self._core = core
    #: Every started mock, one attribute-group per namespace.
    self.mocks = mocks
    #: Observation subscriptions to the node's own emissions.
    self.emitted = emitted
    #: The daemon-clock stand-in serving `peppylib.clock.synchronize` and
    #: the `clock` topic: wall mode by default (skewable via
    #: `set_offset_ns`), sim mode under `use_sim_time=True` (advanced with
    #: `await clock.tick(...)`).
    self.clock = clock
    #: The fixture caller/observer session (not the node's).
    self.session = session
    self._router = router
    #: The node-under-test's instance id.
    self.instance_id = instance_id

@property
def node_runner(self):
    """The running node, for calling its runtime surface directly."""
    return self._core.node_runner

def node_producer_ref(self) -> peppylib.ProducerRef:
    """The node-under-test's wire identity."""
    return peppylib.ProducerRef(self._core.bound_core_node(), self.instance_id)

def setup_finished(self) -> bool:
    """Whether the node's `setup` has returned."""
    return self._core.setup_finished()

async def shutdown(self) -> None:
    """Tears the fixture down in lifecycle order: node convergence
    (cancel -> bounded setup await -> shutdown hooks, propagating a setup
    error), then the clock stand-in, the fixture session and the mocks,
    then the router."""
    try:
        await self._core.shutdown()
    finally:
        clock = self.clock
        self.clock = None
        if clock is not None:
            await clock.close()
        self.emitted = None
        self.session = None
        mocks = self.mocks
        self.mocks = None
        if mocks is not None:
            await mocks.stop_all()
        router = self._router
        self._router = None
        if router is not None:
            await router.shutdown()
"#);
    });
    builder.blank_line();

    builder.block("class _HarnessStart:", |builder| {
        builder.docstring(
            "What `start` returns: awaitable (`harness = await start(...)`) and async \
             context manager (`async with start(...) as harness:`, shutdown runs even \
             when the test body fails).",
        );
        builder.blank_line();
        builder.py(r#"
def __init__(self, coro) -> None:
    self._coro = coro
    self._harness = None

def __await__(self):
    return self._coro.__await__()

async def __aenter__(self) -> Harness:
    self._harness = await self._coro
    return self._harness

async def __aexit__(self, exc_type, exc, tb) -> None:
    harness = self._harness
    self._harness = None
    if harness is not None:
        await harness.shutdown()
"#);
    });
    builder.blank_line();

    // ---- start() + _start() -----------------------------------------
    let mut kwarg_params = String::new();
    let mut kwarg_args = String::new();
    for kwarg in &slot_kwargs {
        kwarg_params.push_str(&format!(", {}={}", kwarg.name, kwarg.default));
        kwarg_args.push_str(&format!(", {}", kwarg.name));
    }

    builder.block(
        &format!(
            "def start(setup, *, parameters=None, instance_id=None, node_dir=None, \
             use_sim_time=False{kwarg_params}):"
        ),
        |builder| {
            builder.py(r#"
"""Boots the full fixture: ephemeral router + started mocks + seeded
StandaloneConfig + the node running in-process behind the readiness
barriers. `setup` is the node's real entry point (the exact
`async def setup(params, node_runner)` shape `NodeBuilder().run`
takes), spawned only after every readiness barrier passed.

Usable both ways:

    harness = await start(setup)   # then: await harness.shutdown()
    async with start(setup) as harness: ...   # shutdown on exit

`parameters` is the typed `peppygen.parameters.Parameters` override
(`None` uses the schema defaults); `instance_id` overrides the unique
generated one; `node_dir` points at the directory holding the node's
peppy.json5 when neither the working directory nor the sync-time path
resolves it; `use_sim_time=True` boots the node in sim time, as a
launcher's `framework: { use_sim_time: true }` would, with the harness
clock in sim mode so no time exists until the test advances it with
`await harness.clock.tick(...)`.
"#);
            // One paragraph per slot the deployment lets a test vary.
            if !slot_kwargs.is_empty() {
                builder.blank_line();
                builder.lines(slot_kwargs.iter().map(|kwarg| &kwarg.doc));
            }
            builder.line("\"\"\"");
            builder.line(&format!(
                "return _HarnessStart(_start(setup, parameters, instance_id, node_dir, \
                 use_sim_time{kwarg_args}))"
            ));
        },
    );
    builder.blank_line();

    builder.block(
        &format!(
            "async def _start(setup, parameters, instance_id, node_dir, \
             use_sim_time{kwarg_args}) -> Harness:"
        ),
        |builder| {
            builder.py(r#"
node_dir = peppylib.testing.resolve_node_dir(node_dir, _SYNC_TIME_NODE_DIR, _NODE_CONFIG_FILE)
if instance_id is None:
    instance_id = peppylib.testing.unique_test_instance_id()
router = await peppylib.testing.EphemeralRouter.start()
"#);
            builder.block("try:", |builder| {
                builder.lines(&mock_starts);
                builder.line("session = await router.connect()");
                builder.lines(&emitted_subscribes);
                builder.py(r#"
# The daemon-clock stand-in lives on the fixture session under the
# standalone core-node identity, where the node's `synchronize` polls
# and its clock subscription listens.
if use_sim_time:
    clock = await peppylib.testing.MockClock.start_sim(
        session,
        peppylib.testing.STANDALONE_CORE_NODE,
        peppylib.testing.MOCK_CLOCK_INSTANCE_ID,
    )
else:
    clock = await peppylib.testing.MockClock.start_wall(
        session,
        peppylib.testing.STANDALONE_CORE_NODE,
        peppylib.testing.MOCK_CLOCK_INSTANCE_ID,
    )
standalone = (
    peppylib.StandaloneConfig()
    .with_messaging(router.host, router.port)
    .with_instance_id(instance_id)
    .with_use_sim_time(use_sim_time)
)
if parameters is not None:
    standalone = standalone.with_parameters(parameters)
"#);
                builder.lines(&seeding);
                if publisher_readiness.is_empty() {
                    builder.line("publisher_readiness = []");
                } else {
                    builder.block("publisher_readiness = [", |builder| {
                        for entry in &publisher_readiness {
                            builder.lines(entry);
                        }
                    });
                    builder.line("]");
                }
                builder.line("service_readiness = [clock.readiness()]");
                builder.lines(&service_readiness);
                builder.call(
                    "core = await peppylib.testing.HarnessCore.start(",
                    &[
                        "os.path.join(node_dir, _NODE_CONFIG_FILE),",
                        "standalone,",
                        "publisher_readiness,",
                        "setup,",
                        "parameters=_hydrate_parameters(parameters),",
                        "service_readiness=service_readiness,",
                    ],
                    ")",
                );
                builder.block("return Harness(", |builder| {
                    builder.line("core=core,");
                    builder.call(
                        "mocks=Mocks(",
                        &[
                            &format!("deps=DepMocks({}),", mock_kwargs(&dep_attrs, "dep")),
                            &format!(
                                "pairings=PairingMocks({}),",
                                mock_kwargs(&pairing_attrs, "pair")
                            ),
                            &format!(
                                "observed=ObservedMocks({}),",
                                mock_kwargs(&observed_attrs, "obs")
                            ),
                        ],
                        "),",
                    );
                    builder.line(&format!(
                        "emitted=Emitted({}),",
                        mock_kwargs(&emitted_attrs, "emitted")
                    ));
                    builder.lines([
                        "clock=clock,",
                        "session=session,",
                        "router=router,",
                        "instance_id=instance_id,",
                    ]);
                });
                builder.line(")");
            });
            builder.block("except BaseException:", |builder| {
                // A failed boot must still release the router (and its
                // mesh-serialization lock); Rust gets this from Drop, Python has
                // no async drop hook.
                builder.py(r#"
await router.shutdown()
raise
"#);
            });
        },
    );

    let doc = "Generated test harness: ephemeral router + started mocks + seeded \
               StandaloneConfig + the node running in-process behind the readiness \
               barriers. Construct with `start(...)`.";
    Ok(format!("\"\"\"{doc}\"\"\"\n\n{}", builder.build()))
}

#[allow(clippy::too_many_arguments)]
fn render_dep_harness_parts(
    link_id: &str,
    spec: &DepLinkSpec,
    attr: &str,
    alias: &str,
    local: &str,
    slot_kwargs: &mut Vec<SlotKwarg>,
    mock_starts: &mut Vec<String>,
    seeding: &mut Vec<String>,
    service_readiness: &mut Vec<String>,
) {
    let target = target_python_expr(&spec.target);

    // One ServiceReadiness entry per served interface per mock instance,
    // probed from the node's session before its setup runs.
    let per_instance_readiness = |instance_expr: &str, indent: &str| -> Vec<String> {
        let mut entries = Vec::new();
        for (name, kind) in spec
            .services
            .iter()
            .map(|service| (service.name.as_str(), "service"))
            .chain(
                spec.actions
                    .iter()
                    .map(|action| (action.name.as_str(), "action")),
            )
        {
            entries.push(format!(
                "{indent}service_readiness.append(peppylib.testing.ServiceReadiness(\
                 target={target}, name={name:?}, producer={alias}.producer_ref_for(\
                 {instance_expr}), kind={kind:?}))"
            ));
        }
        entries
    };

    match spec.cardinality {
        Cardinality::One => {
            mock_starts.push(format!("{local} = await {alias}.Mock.start(router)"));
            seeding.push(format!(
                "standalone = standalone.with_bound_producer({link_id:?}, \
                 {alias}.MOCK_CORE_NODE, {alias}.MOCK_INSTANCE_ID)"
            ));
            service_readiness.extend(per_instance_readiness(
                &format!("{alias}.MOCK_INSTANCE_ID"),
                "",
            ));
        }
        Cardinality::ZeroOrOne => {
            let kwarg = format!("{attr}_vacant");
            slot_kwargs.push(SlotKwarg {
                name: kwarg.clone(),
                default: "False".to_string(),
                doc: format!(
                    "{kwarg}: leave the `{link_id}` dependency slot vacant (its \
                     cardinality admits an empty binding)."
                ),
            });
            mock_starts.push(format!("{local} = None"));
            mock_starts.push(format!("if not {kwarg}:"));
            mock_starts.push(format!("    {local} = await {alias}.Mock.start(router)"));
            seeding.push(format!("if {kwarg}:"));
            seeding.push(format!(
                "    standalone = standalone.with_vacant_producer_slot({link_id:?})"
            ));
            seeding.push("else:".to_string());
            seeding.push(format!(
                "    standalone = standalone.with_bound_producer({link_id:?}, \
                 {alias}.MOCK_CORE_NODE, {alias}.MOCK_INSTANCE_ID)"
            ));
            let entries = per_instance_readiness(&format!("{alias}.MOCK_INSTANCE_ID"), "    ");
            if !entries.is_empty() {
                service_readiness.push(format!("if not {kwarg}:"));
                service_readiness.extend(entries);
            }
        }
        Cardinality::OneOrMore | Cardinality::ZeroOrMore => {
            let kwarg = format!("{attr}_instances");
            let default_count: usize = match spec.cardinality {
                Cardinality::OneOrMore => 1,
                _ => 0,
            };
            slot_kwargs.push(SlotKwarg {
                name: kwarg.clone(),
                default: default_count.to_string(),
                doc: format!(
                    "{kwarg}: how many mock producer instances to start and bind for \
                     the `{link_id}` dependency slot."
                ),
            });
            let ids_kwarg = format!("{attr}_instance_ids");
            slot_kwargs.push(SlotKwarg {
                name: ids_kwarg.clone(),
                default: "None".to_string(),
                doc: format!(
                    "{ids_kwarg}: explicit instance ids for the `{link_id}` mock \
                     producers, overriding `{kwarg}` when given. For nodes that \
                     classify producers by instance name."
                ),
            });
            let ids_local = format!("{local}_member_ids");
            mock_starts.push(format!(
                "{ids_local} = list({ids_kwarg}) if {ids_kwarg} is not None else \
                 [f\"{{{alias}.MOCK_INSTANCE_ID}}-{{index}}\" for index in range({kwarg})]"
            ));
            mock_starts.push(format!("{local} = []"));
            mock_starts.push(format!("for member_instance_id in {ids_local}:"));
            mock_starts.push(format!(
                "    {local}.append(await {alias}.Mock.start(router, member_instance_id))"
            ));
            seeding.push(format!("for member_instance_id in {ids_local}:"));
            seeding.push(format!(
                "    standalone = standalone.with_bound_producer({link_id:?}, \
                 {alias}.MOCK_CORE_NODE, member_instance_id)"
            ));
            let entries = per_instance_readiness("member_instance_id", "    ");
            if !entries.is_empty() {
                service_readiness.push(format!("for member_instance_id in {ids_local}:"));
                service_readiness.extend(entries);
            }
        }
    }
}
