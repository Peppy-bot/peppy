//! Python renderer for the generated `fixtures` category: the per-node
//! harness (`fixtures/harness.py` — ephemeral router + started mocks + seeded
//! `StandaloneConfig` + running node behind the readiness barriers) and typed
//! observation clients for the node's own surface (`emitted_topics`
//! subscriptions, `exposed_services` / `exposed_actions` identity-explicit
//! callers). Lifecycle semantics live in `peppylib.testing.HarnessCore`; this
//! file contributes only what is per-node: mock construction, seeding calls,
//! readiness lists, and typed codecs on the production schema keys.
//!
//! Skipped entirely when the backend was driven without
//! [`set_node_identity`](super::PythonGenerator::set_node_identity) — the
//! harness cannot pin the node's own targets without the manifest identity.

use super::super::naming::non_empty_str;
use super::super::testgen::{
    DepLinkSpec, EmittedSpec, ExposedActionSpec, ExposedServiceSpec, FIXTURE_CORE_NODE,
    FIXTURE_INSTANCE_ID, TargetSpec, TestGenRegistry,
};
use super::PythonGenerator;
use super::code_builder::PythonCodeBuilder;
use super::mock::{
    emit_local_message_with_deserializer, emit_value_deserializer, emit_value_serializer,
    production_import_line, production_module_path, target_python_expr,
};
use super::scaffold::sanitize_python_module_name;
use super::type_mapping::qos_profile_python;
use crate::error::{Error, Result};
use crate::generator::types::{InterfaceArtifact, InterfaceKind, scoped_schema_key};
use config::node::Cardinality;
use std::collections::HashSet;
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
    let mut seen = HashSet::new();
    for spec in &registry.own.emitted {
        let (artifact, member) = render_emitted_topic(generator, &node_name, &node_tag, spec)?;
        claim(&mut seen, &member.attr, "fixtures emitted", &spec.name)?;
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

    let node_dir = sync_time_node_dir(to_path);
    let harness = render_harness(
        &generator.parameters,
        registry,
        &node_name,
        &node_tag,
        &emitted_members,
        &node_dir,
    )?;
    generator.push_section(InterfaceArtifact {
        module_path: vec!["harness".to_string()],
        kind: InterfaceKind::Fixture,
        code_output: harness,
    });
    Ok(())
}

fn claim(seen: &mut HashSet<String>, sanitized: &str, category: &str, raw: &str) -> Result<()> {
    if !seen.insert(sanitized.to_string()) {
        return Err(Error::ModuleNameCollision {
            category: category.to_string(),
            first: sanitized.to_string(),
            second: raw.to_string(),
            sanitized: sanitized.to_string(),
        });
    }
    Ok(())
}

/// The absolute node directory baked at generation time: the canonicalized
/// output path minus the trailing `.peppy/libs/peppygen`. Last-resort
/// fallback for the harness's node-dir resolution (after the explicit
/// argument and the cwd walk).
fn sync_time_node_dir(to_path: &Path) -> String {
    let canonical = std::fs::canonicalize(to_path).unwrap_or_else(|_| to_path.to_path_buf());
    let node_dir = if canonical.ends_with(config::consts::PEPPYGEN_OUTPUT_PATH) {
        canonical
            .ancestors()
            .nth(Path::new(config::consts::PEPPYGEN_OUTPUT_PATH).components().count())
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
    let fn_name = crate::generator::rust::identifiers::prefixed_name(
        "",
        non_empty_str(topic_name),
        "topic",
    );
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

    builder.line("class Subscription:");
    builder.indent();
    builder.line("\"\"\"A held subscription to the node's emissions on this topic.\"\"\"");
    builder.blank_line();
    builder.line("def __init__(self, inner) -> None:");
    builder.indent();
    builder.line("self._inner = inner");
    builder.dedent();
    builder.blank_line();
    builder.line("async def next(self) -> Optional[Message]:");
    builder.indent();
    builder.line(
        "\"\"\"Awaits the node's next message on this topic; `None` once the fixture \
         session closes.\"\"\"",
    );
    builder.line("message = await self._inner.on_next_message()");
    builder.line("if message is None:");
    builder.indent();
    builder.line("return None");
    builder.dedent();
    builder.line("return _deserialize_message(message.payload)");
    builder.dedent();
    builder.dedent();
    builder.blank_line();

    builder.line("async def subscribe(session, node_instance_id: str) -> Subscription:");
    builder.indent();
    builder.line(
        "\"\"\"Opens the observation subscription, pinned to the node under test; the \
         harness calls this before the node boots and barriers on it, so the node's \
         very first publish is captured.\"\"\"",
    );
    builder.line("producer = peppylib.ProducerRef(peppylib.testing.STANDALONE_CORE_NODE, node_instance_id)");
    builder.line("inner = await peppylib.TopicMessenger.subscribe(");
    builder.indent();
    builder.line("session,");
    builder.line("FIXTURE_CORE_NODE,");
    builder.line("FIXTURE_INSTANCE_ID,");
    builder.line(&format!("{target},"));
    builder.line("TOPIC_NAME,");
    builder.line("producer,");
    builder.line(&format!("{qos},"));
    builder.dedent();
    builder.line(")");
    builder.line("return Subscription(inner)");
    builder.dedent();

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

    builder.line(&format!(
        "async def poll(harness{request_param}, timeout: float) -> {return_ty}:"
    ));
    builder.indent();
    builder.line(&format!(
        "\"\"\"Polls the node's exposed service `{service_name}` once from the fixture \
         session (a fresh, identity-explicit caller pinned to the node under test), \
         gating on reachability first. `timeout` bounds the reachability gate and the \
         poll individually.\"\"\""
    ));
    builder.line("producer = harness.node_producer_ref()");
    builder.line("await peppylib.testing.wait_service_reachable(");
    builder.indent();
    builder.line("harness.session,");
    builder.line("FIXTURE_CORE_NODE,");
    builder.line("FIXTURE_INSTANCE_ID,");
    builder.line(&format!("{target},"));
    builder.line("SERVICE_NAME,");
    builder.line("producer,");
    builder.line("timeout,");
    builder.dedent();
    builder.line(")");
    let request_payload = if spec.request.is_some() {
        "_serialize_request(request)"
    } else {
        "b\"\""
    };
    if spec.response.is_some() {
        builder.line("response_message = await peppylib.ServiceMessenger.poll(");
    } else {
        builder.line("await peppylib.ServiceMessenger.poll(");
    }
    builder.indent();
    builder.line("harness.session,");
    builder.line("FIXTURE_CORE_NODE,");
    builder.line("FIXTURE_INSTANCE_ID,");
    builder.line(&format!("{target},"));
    builder.line("SERVICE_NAME,");
    builder.line("producer,");
    builder.line(&format!("{request_payload},"));
    builder.line("timeout,");
    builder.dedent();
    builder.line(")");
    if spec.response.is_some() {
        builder.line("return _deserialize_response(response_message.payload)");
    } else {
        builder.line("return None");
    }
    builder.dedent();

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
    let goal_response_key =
        scoped_schema_key(spec.origin.as_ref(), &format!("{action_name}_goal_response"));
    let feedback_key = scoped_schema_key(spec.origin.as_ref(), &format!("{action_name}_feedback"));
    let result_key =
        scoped_schema_key(spec.origin.as_ref(), &format!("{action_name}_result_response"));
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
    builder.line("class CancelState(IntEnum):");
    builder.indent();
    builder.line("SIGNALLED = 0");
    builder.line("ALREADY_TERMINAL = 1");
    builder.line("UNKNOWN = 2");
    builder.dedent();
    builder.blank_line();
    builder.dataclass(
        "CancelResponse",
        &[
            ("core_node", "str"),
            ("instance_id", "str"),
            ("state", "CancelState"),
        ],
    );

    builder.line("class ResultStatus(IntEnum):");
    builder.indent();
    builder.line("COMPLETED = 0");
    builder.line("CANCELLED = 1");
    builder.line("ABANDONED = 2");
    builder.line("EXPIRED = 3");
    builder.dedent();
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

    builder.line("class GoalHandle:");
    builder.indent();
    builder.line(&format!(
        "\"\"\"A goal in flight against the node's action `{action_name}`; construct \
         with `send_goal`. Attributes: `accepted` (whether the node admitted the \
         goal), `reason` (optional human-readable rejection reason){}.\"\"\"",
        if spec.goal_response.is_some() {
            ", `data` (the decoded goal response; `None` when a rejection carried no payload)"
        } else {
            ""
        }
    ));
    builder.blank_line();
    if spec.feedback.is_some() {
        builder.line("async def on_next_feedback(self) -> Feedback:");
        builder.indent();
        builder.line("\"\"\"Receives the next decoded feedback message for this goal.");
        builder.blank_line();
        builder.line("Raises RuntimeError when the node closed the stream cleanly");
        builder.line("(end-of-stream sentinel) and ConnectionError when the node instance");
        builder.line("disappeared without closing it; in the latter case get_result");
        builder.line("resolves to ResultStatus.ABANDONED.");
        builder.line("\"\"\"");
        builder.line("feedback = await self._inner.on_next_feedback()");
        builder.line("return _deserialize_feedback(feedback.payload)");
        builder.dedent();
        builder.blank_line();
    }
    builder.line("async def cancel_goal(self, timeout: float) -> CancelResponse:");
    builder.indent();
    builder.line("\"\"\"Requests cancellation of this goal.\"\"\"");
    builder.line("reply = await peppylib.ActionMessenger.cancel_goal(");
    builder.indent();
    builder.line("self._messenger,");
    builder.line("self._inner,");
    builder.line("timeout,");
    builder.dedent();
    builder.line(")");
    builder.line("return CancelResponse(");
    builder.indent();
    builder.line("core_node=reply.core_node,");
    builder.line("instance_id=reply.instance_id,");
    builder.line("state=CancelState(reply.state),");
    builder.dedent();
    builder.line(")");
    builder.dedent();
    builder.blank_line();
    builder.line("async def get_result(self, timeout: float) -> ResultResponse:");
    builder.indent();
    builder.line("\"\"\"Retrieves the goal's terminal result.\"\"\"");
    builder.line("reply = await peppylib.ActionMessenger.request_result(");
    builder.indent();
    builder.line("self._messenger,");
    builder.line("self._inner,");
    builder.line("timeout,");
    builder.dedent();
    builder.line(")");
    builder.line("status = ResultStatus(reply.status)");
    if spec.result.is_some() {
        builder.line("data = None");
        builder.line("if status in (ResultStatus.COMPLETED, ResultStatus.CANCELLED):");
        builder.indent();
        builder.line("data = _deserialize_result(reply.body)");
        builder.dedent();
        builder.line("return ResultResponse(");
        builder.indent();
        builder.line("core_node=reply.core_node,");
        builder.line("instance_id=reply.instance_id,");
        builder.line("status=status,");
        builder.line("data=data,");
        builder.dedent();
        builder.line(")");
    } else {
        builder.line("return ResultResponse(");
        builder.indent();
        builder.line("core_node=reply.core_node,");
        builder.line("instance_id=reply.instance_id,");
        builder.line("status=status,");
        builder.dedent();
        builder.line(")");
    }
    builder.dedent();
    builder.dedent();
    builder.blank_line();

    let request_param = if spec.goal_request.is_some() {
        ", request: _production.GoalRequestData"
    } else {
        ""
    };
    builder.line(&format!(
        "async def send_goal(harness{request_param}, feedback_qos: peppylib.QoSProfile, \
         timeout: float) -> GoalHandle:"
    ));
    builder.indent();
    builder.line(
        "\"\"\"Sends a goal to the node from the fixture session (a fresh, \
         identity-explicit caller pinned to the node under test), gating on \
         reachability first, and decodes the admission reply.\"\"\"",
    );
    builder.line("producer = harness.node_producer_ref()");
    builder.line("await peppylib.testing.wait_action_reachable(");
    builder.indent();
    builder.line("harness.session,");
    builder.line("FIXTURE_CORE_NODE,");
    builder.line("FIXTURE_INSTANCE_ID,");
    builder.line(&format!("{target},"));
    builder.line("ACTION_NAME,");
    builder.line("producer,");
    builder.line("timeout,");
    builder.dedent();
    builder.line(")");
    if spec.goal_request.is_some() {
        builder.line("user_goal_payload = _serialize_goal_request(request)");
    } else {
        builder.line("user_goal_payload = b\"\"");
    }
    builder.line("inner = await peppylib.ActionMessenger.send_goal(");
    builder.indent();
    builder.line("harness.session,");
    builder.line("FIXTURE_CORE_NODE,");
    builder.line("FIXTURE_INSTANCE_ID,");
    builder.line(&format!("{target},"));
    builder.line("ACTION_NAME,");
    builder.line("producer,");
    builder.line("user_goal_payload,");
    builder.line("feedback_qos,");
    builder.line("timeout,");
    builder.dedent();
    builder.line(")");
    builder.line("handle = GoalHandle()");
    builder.line("handle._messenger = harness.session");
    builder.line("handle._inner = inner");
    builder.line("handle.accepted = inner.accepted");
    builder.line("handle.reason = inner.reason");
    if spec.goal_response.is_some() {
        // An empty body means no response was supplied (a declared response
        // serializes to a non-empty capnp message), which only a reject can
        // produce.
        builder.line("body = inner.goal_reply_body");
        builder.line("handle.data = _deserialize_goal_response(body) if body else None");
    }
    builder.line("return handle");
    builder.dedent();

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
/// Python; only the booleans differ).
fn python_json_literal(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Bool(true) => "True".to_string(),
        serde_json::Value::Bool(false) => "False".to_string(),
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
    builder.line(&format!("class {class_name}:"));
    builder.indent();
    builder.line(&format!("\"\"\"{doc}\"\"\""));
    if !attrs.is_empty() {
        builder.blank_line();
        let mut params = vec!["self".to_string()];
        params.extend(attrs.iter().cloned());
        builder.line(&format!("def __init__({}) -> None:", params.join(", ")));
        builder.indent();
        for attr in attrs {
            builder.line(&format!("self.{attr} = {attr}"));
        }
        builder.dedent();
    }
    builder.dedent();
    builder.blank_line();
}

#[allow(clippy::too_many_arguments)]
fn render_harness(
    parameters: &config::ParameterSchema,
    registry: &TestGenRegistry,
    node_name: &str,
    node_tag: &str,
    emitted_members: &[EmittedMember],
    node_dir: &str,
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
        mock_starts.push(format!("{local} = await {alias}.Mock.start(router, instance_id)"));
        if spec.optional {
            // The mock still starts when vacant: its pinned subscription is
            // what resolves the publisher-readiness barrier, and an unpaired
            // node's publishes on the slot are legal no-ops — so only the
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
    builder.line(&format!("_SYNC_TIME_NODE_DIR = {node_dir:?}"));
    match default_parameters_literal(parameters) {
        Some(literal) => builder.line(&format!("_DEFAULT_PARAMETERS = {literal}")),
        None => {
            builder.line("# At least one parameter has no schema default: `parameters=None`");
            builder.line("# defers to the node's own boot validation, which raises the");
            builder.line("# canonical missing-parameter error before `setup` runs.");
            builder.line("_DEFAULT_PARAMETERS = None");
        }
    }
    builder.blank_line();

    builder.line("def _resolve_node_dir(node_dir):");
    builder.indent();
    builder.line("\"\"\"The node directory holding peppy.json5, resolved from: the explicit");
    builder.line("`node_dir` argument, else the nearest peppy.json5 walking up from the");
    builder.line("current working directory, else the absolute path baked at sync time.\"\"\"");
    builder.line("if node_dir is not None:");
    builder.indent();
    builder.line("candidate = os.path.abspath(os.fspath(node_dir))");
    builder.line("if os.path.isfile(os.path.join(candidate, _NODE_CONFIG_FILE)):");
    builder.indent();
    builder.line("return candidate");
    builder.dedent();
    builder.line("raise RuntimeError(");
    builder.indent();
    builder.line("f\"node_dir {candidate!r} does not contain {_NODE_CONFIG_FILE}; pass \"");
    builder.line("\"the directory that holds the node's peppy.json5\"");
    builder.dedent();
    builder.line(")");
    builder.dedent();
    builder.line("current = os.getcwd()");
    builder.line("while True:");
    builder.indent();
    builder.line("if os.path.isfile(os.path.join(current, _NODE_CONFIG_FILE)):");
    builder.indent();
    builder.line("return current");
    builder.dedent();
    builder.line("parent = os.path.dirname(current)");
    builder.line("if parent == current:");
    builder.indent();
    builder.line("break");
    builder.dedent();
    builder.line("current = parent");
    builder.dedent();
    builder.line("if os.path.isfile(os.path.join(_SYNC_TIME_NODE_DIR, _NODE_CONFIG_FILE)):");
    builder.indent();
    builder.line("return _SYNC_TIME_NODE_DIR");
    builder.dedent();
    builder.line("raise RuntimeError(");
    builder.indent();
    builder.line("\"could not locate the node's peppy.json5 from any source: \"");
    builder.line("\"(1) no explicit node_dir= was passed to start() — pass the node \"");
    builder.line("\"directory explicitly; \"");
    builder.line("f\"(2) no {_NODE_CONFIG_FILE} was found walking up from the current \"");
    builder.line("f\"working directory {os.getcwd()!r} — run the tests from inside the \"");
    builder.line("\"node directory; \"");
    builder.line("f\"(3) the sync-time path {_SYNC_TIME_NODE_DIR!r} no longer holds one \"");
    builder.line("\"— the node has moved since generation; re-run peppy sync\"");
    builder.dedent();
    builder.line(")");
    builder.dedent();
    builder.blank_line();

    builder.line("def _hydrate_parameters(parameters):");
    builder.indent();
    builder.line("\"\"\"The typed Parameters `setup` receives: the caller's override");
    builder.line("verbatim, or the schema defaults hydrated the way `NodeBuilder().run`");
    builder.line("would hydrate them.\"\"\"");
    builder.line("if parameters is not None:");
    builder.indent();
    builder.line("return parameters");
    builder.dedent();
    builder.line("if _DEFAULT_PARAMETERS is None:");
    builder.indent();
    builder.line("return None");
    builder.dedent();
    builder.line("return _parameters.Parameters.from_dict(_DEFAULT_PARAMETERS)");
    builder.dedent();
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

    builder.line("class Mocks:");
    builder.indent();
    builder.line(
        "\"\"\"Every started mock, grouped by namespace (`deps` / `pairings` / \
         `observed`), one attribute per link. A test can consume an individual mock \
         (`await mock.stop()` is producer-loss) without giving up the harness.\"\"\"",
    );
    builder.blank_line();
    builder.line("def __init__(self, deps, pairings, observed) -> None:");
    builder.indent();
    builder.line("self.deps = deps");
    builder.line("self.pairings = pairings");
    builder.line("self.observed = observed");
    builder.dedent();
    builder.blank_line();
    builder.line("async def _stop_all(self) -> None:");
    builder.indent();
    builder.line("for group in (self.deps, self.pairings, self.observed):");
    builder.indent();
    builder.line("for value in vars(group).values():");
    builder.indent();
    builder.line("if value is None:");
    builder.indent();
    builder.line("continue");
    builder.dedent();
    builder.line("if isinstance(value, list):");
    builder.indent();
    builder.line("for mock in value:");
    builder.indent();
    builder.line("await mock.stop()");
    builder.dedent();
    builder.dedent();
    builder.line("else:");
    builder.indent();
    builder.line("await value.stop()");
    builder.dedent();
    builder.dedent();
    builder.dedent();
    builder.dedent();
    builder.dedent();
    builder.blank_line();

    emit_namespace_class(
        &mut builder,
        "Emitted",
        "Typed subscriptions to the node's emitted topics, opened before the node \
         booted (so its very first publish is captured).",
        &emitted_attrs,
    );

    builder.line("class Harness:");
    builder.indent();
    builder.line(
        "\"\"\"Generated test harness: ephemeral router + started mocks + seeded \
         `StandaloneConfig` + the node running in-process behind the readiness \
         barriers. Construct with `start(...)`; tear down with `await shutdown()` (or \
         let `async with start(...)` do it).\"\"\"",
    );
    builder.blank_line();
    builder.line("def __init__(self, core, mocks, emitted, session, router, instance_id) -> None:");
    builder.indent();
    builder.line("self._core = core");
    builder.line("#: Every started mock, one attribute-group per namespace.");
    builder.line("self.mocks = mocks");
    builder.line("#: Observation subscriptions to the node's own emissions.");
    builder.line("self.emitted = emitted");
    builder.line("#: The fixture caller/observer session (not the node's).");
    builder.line("self.session = session");
    builder.line("self._router = router");
    builder.line("#: The node-under-test's instance id.");
    builder.line("self.instance_id = instance_id");
    builder.dedent();
    builder.blank_line();
    builder.line("@property");
    builder.line("def node_runner(self):");
    builder.indent();
    builder.line("\"\"\"The running node, for calling its runtime surface directly.\"\"\"");
    builder.line("return self._core.node_runner");
    builder.dedent();
    builder.blank_line();
    builder.line("def node_producer_ref(self) -> peppylib.ProducerRef:");
    builder.indent();
    builder.line("\"\"\"The node-under-test's wire identity.\"\"\"");
    builder.line("return peppylib.ProducerRef(self._core.bound_core_node(), self.instance_id)");
    builder.dedent();
    builder.blank_line();
    builder.line("def setup_finished(self) -> bool:");
    builder.indent();
    builder.line("\"\"\"Whether the node's `setup` has returned.\"\"\"");
    builder.line("return self._core.setup_finished()");
    builder.dedent();
    builder.blank_line();
    builder.line("async def shutdown(self) -> None:");
    builder.indent();
    builder.line("\"\"\"Tears the fixture down in lifecycle order: node convergence");
    builder.line("(cancel -> bounded setup await -> shutdown hooks, propagating a setup");
    builder.line("error), then the fixture session and the mocks, then the router.\"\"\"");
    builder.line("try:");
    builder.indent();
    builder.line("await self._core.shutdown()");
    builder.dedent();
    builder.line("finally:");
    builder.indent();
    builder.line("self.emitted = None");
    builder.line("self.session = None");
    builder.line("mocks = self.mocks");
    builder.line("self.mocks = None");
    builder.line("if mocks is not None:");
    builder.indent();
    builder.line("await mocks._stop_all()");
    builder.dedent();
    builder.line("router = self._router");
    builder.line("self._router = None");
    builder.line("if router is not None:");
    builder.indent();
    builder.line("await router.shutdown()");
    builder.dedent();
    builder.dedent();
    builder.dedent();
    builder.dedent();
    builder.blank_line();

    builder.line("class _HarnessStart:");
    builder.indent();
    builder.line(
        "\"\"\"What `start` returns: awaitable (`harness = await start(...)`) and async \
         context manager (`async with start(...) as harness:` — shutdown runs even \
         when the test body fails).\"\"\"",
    );
    builder.blank_line();
    builder.line("def __init__(self, coro) -> None:");
    builder.indent();
    builder.line("self._coro = coro");
    builder.line("self._harness = None");
    builder.dedent();
    builder.blank_line();
    builder.line("def __await__(self):");
    builder.indent();
    builder.line("return self._coro.__await__()");
    builder.dedent();
    builder.blank_line();
    builder.line("async def __aenter__(self) -> Harness:");
    builder.indent();
    builder.line("self._harness = await self._coro");
    builder.line("return self._harness");
    builder.dedent();
    builder.blank_line();
    builder.line("async def __aexit__(self, exc_type, exc, tb) -> None:");
    builder.indent();
    builder.line("harness = self._harness");
    builder.line("self._harness = None");
    builder.line("if harness is not None:");
    builder.indent();
    builder.line("await harness.shutdown()");
    builder.dedent();
    builder.dedent();
    builder.dedent();
    builder.blank_line();

    // ---- start() + _start() -----------------------------------------
    let mut kwarg_params = String::new();
    let mut kwarg_args = String::new();
    for kwarg in &slot_kwargs {
        kwarg_params.push_str(&format!(", {}={}", kwarg.name, kwarg.default));
        kwarg_args.push_str(&format!(", {}", kwarg.name));
    }

    builder.line(&format!(
        "def start(setup, *, parameters=None, instance_id=None, node_dir=None{kwarg_params}):"
    ));
    builder.indent();
    builder.line("\"\"\"Boots the full fixture: ephemeral router + started mocks + seeded");
    builder.line("StandaloneConfig + the node running in-process behind the readiness");
    builder.line("barriers. `setup` is the node's real entry point (the exact");
    builder.line("`async def setup(params, node_runner)` shape `NodeBuilder().run`");
    builder.line("takes), spawned only after every readiness barrier passed.");
    builder.blank_line();
    builder.line("Usable both ways:");
    builder.blank_line();
    builder.line("    harness = await start(setup)   # then: await harness.shutdown()");
    builder.line("    async with start(setup) as harness: ...   # shutdown on exit");
    builder.blank_line();
    builder.line("`parameters` is the typed `peppygen.parameters.Parameters` override");
    builder.line("(`None` uses the schema defaults); `instance_id` overrides the unique");
    builder.line("generated one; `node_dir` points at the directory holding the node's");
    builder.line("peppy.json5 when neither the working directory nor the sync-time path");
    builder.line("resolves it.");
    if !slot_kwargs.is_empty() {
        builder.blank_line();
        for kwarg in &slot_kwargs {
            builder.line(&kwarg.doc);
        }
    }
    builder.line("\"\"\"");
    builder.line(&format!(
        "return _HarnessStart(_start(setup, parameters, instance_id, node_dir{kwarg_args}))"
    ));
    builder.dedent();
    builder.blank_line();

    builder.line(&format!(
        "async def _start(setup, parameters, instance_id, node_dir{kwarg_args}) -> Harness:"
    ));
    builder.indent();
    builder.line("node_dir = _resolve_node_dir(node_dir)");
    builder.line("if instance_id is None:");
    builder.indent();
    builder.line("instance_id = peppylib.testing.unique_test_instance_id()");
    builder.dedent();
    builder.line("router = await peppylib.testing.EphemeralRouter.start()");
    builder.line("try:");
    builder.indent();
    for line in &mock_starts {
        builder.line(line);
    }
    builder.line("session = await router.connect()");
    for line in &emitted_subscribes {
        builder.line(line);
    }
    builder.line("standalone = (");
    builder.indent();
    builder.line("peppylib.StandaloneConfig()");
    builder.line(".with_messaging(router.host, router.port)");
    builder.line(".with_instance_id(instance_id)");
    builder.dedent();
    builder.line(")");
    builder.line("if parameters is not None:");
    builder.indent();
    builder.line("standalone = standalone.with_parameters(parameters)");
    builder.dedent();
    for line in &seeding {
        builder.line(line);
    }
    if publisher_readiness.is_empty() {
        builder.line("publisher_readiness = []");
    } else {
        builder.line("publisher_readiness = [");
        builder.indent();
        for entry in &publisher_readiness {
            for line in entry {
                builder.line(line);
            }
        }
        builder.dedent();
        builder.line("]");
    }
    builder.line("service_readiness = []");
    for line in &service_readiness {
        builder.line(line);
    }
    builder.line("core = await peppylib.testing.HarnessCore.start(");
    builder.indent();
    builder.line("os.path.join(node_dir, _NODE_CONFIG_FILE),");
    builder.line("standalone,");
    builder.line("publisher_readiness,");
    builder.line("setup,");
    builder.line("parameters=_hydrate_parameters(parameters),");
    builder.line("service_readiness=service_readiness,");
    builder.dedent();
    builder.line(")");
    builder.line("return Harness(");
    builder.indent();
    builder.line("core=core,");
    builder.line("mocks=Mocks(");
    builder.indent();
    builder.line(&format!("deps=DepMocks({}),", mock_kwargs(&dep_attrs, "dep")));
    builder.line(&format!("pairings=PairingMocks({}),", mock_kwargs(&pairing_attrs, "pair")));
    builder.line(&format!("observed=ObservedMocks({}),", mock_kwargs(&observed_attrs, "obs")));
    builder.dedent();
    builder.line("),");
    builder.line(&format!("emitted=Emitted({}),", mock_kwargs(&emitted_attrs, "emitted")));
    builder.line("session=session,");
    builder.line("router=router,");
    builder.line("instance_id=instance_id,");
    builder.dedent();
    builder.line(")");
    builder.dedent();
    builder.line("except BaseException:");
    builder.indent();
    // A failed boot must still release the router (and its mesh-serialization
    // lock); Rust gets this from Drop, Python has no async drop hook.
    builder.line("await router.shutdown()");
    builder.line("raise");
    builder.dedent();
    builder.dedent();

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
            .chain(spec.actions.iter().map(|action| (action.name.as_str(), "action")))
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
            service_readiness
                .extend(per_instance_readiness(&format!("{alias}.MOCK_INSTANCE_ID"), ""));
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
