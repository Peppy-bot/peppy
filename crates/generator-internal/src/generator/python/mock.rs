//! Python renderer for the generated `mock` category: one typed `Mock` per
//! link under `mock/{deps,pairings,observed}/<link_id>.py`, playing the
//! dependency (or pairing peer / observed source) over the real wire under
//! the exact identity the harness seeds. Everything node-invariant lives in
//! `peppylib.testing`; this file emits only the typed veneer: identity
//! constants, per-interface member classes reusing the production
//! dataclasses, and the missing-direction codecs on the production schema
//! keys (so the capnp schema set gains zero files).
//!
//! Mirrors `rust/mock.rs` semantically; the flat-file shape (Python has no
//! nested modules inside one file) flattens the Rust sub-modules into
//! `<Camel(member)>Publisher` / `Service` / `Action` classes and
//! member-prefixed codec/​re-export names.

use super::super::naming::to_camel_case;
use super::super::testgen::{
    DepActionSpec, DepLinkSpec, DepServiceSpec, DepTopicSpec, MOCK_CORE_NODE, MOCK_PEER_LINK_ID,
    MOCK_SOURCE_LINK_ID, ObservedLinkSpec, PairTopicSpec, PairingLinkSpec, TargetSpec,
    TestGenRegistry, dep_member_name, mock_instance_id,
};
use super::code_builder::{PythonCodeBuilder, emit_format_as_dataclass};
use super::deserialization;
use super::scaffold::sanitize_python_module_name;
use super::serialization;
use super::topics::{capnp_loader_fn_name, emit_capnp_loader_fn, emit_capnp_preamble};
use super::type_mapping::qos_profile_python;
use super::{PythonGenerator, PythonSchemaInfo};
use crate::error::{Error, Result};
use crate::generator::types::{InterfaceArtifact, InterfaceKind};
use config::node::MessageFormat;
use std::collections::{HashMap, HashSet};

pub(super) fn render(generator: &mut PythonGenerator, registry: &TestGenRegistry) -> Result<()> {
    for (link_id, spec) in &registry.deps {
        let code = render_dep_link(generator, link_id, spec)?;
        generator.push_section(InterfaceArtifact {
            module_path: vec!["deps".to_string(), link_id.clone()],
            kind: InterfaceKind::Mock,
            code_output: code,
        });
    }
    for (link_id, spec) in &registry.pairings {
        let code = render_pairing_link(generator, link_id, spec)?;
        generator.push_section(InterfaceArtifact {
            module_path: vec!["pairings".to_string(), link_id.clone()],
            kind: InterfaceKind::Mock,
            code_output: code,
        });
    }
    for (link_id, spec) in &registry.observed {
        let code = render_observed_link(generator, link_id, spec)?;
        generator.push_section(InterfaceArtifact {
            module_path: vec!["observed".to_string(), link_id.clone()],
            kind: InterfaceKind::Mock,
            code_output: code,
        });
    }
    Ok(())
}

/// `peppylib.SenderTarget` expression for the identity a mock impersonates.
pub(super) fn target_python_expr(target: &TargetSpec) -> String {
    match target {
        TargetSpec::Node { name, tag } => {
            format!("peppylib.SenderTarget.node({name:?}, {tag:?})")
        }
        TargetSpec::Contract { name, tag } => {
            format!("peppylib.SenderTarget.contract({name:?}, {tag:?})")
        }
        TargetSpec::Pairing { name, tag } => {
            format!("peppylib.SenderTarget.pairing({name:?}, {tag:?})")
        }
    }
}

/// Dotted import path of a production generated module:
/// `peppygen.<category>.<segments…>`, each segment sanitized exactly like the
/// tree writer sanitizes the on-disk module names.
pub(super) fn production_module_path(category: &str, segments: &[&str]) -> String {
    let mut parts = vec!["peppygen".to_string(), category.to_string()];
    parts.extend(
        segments
            .iter()
            .map(|segment| sanitize_python_module_name(segment)),
    );
    parts.join(".")
}

/// `from <package> import <leaf> as <alias>` for a production module path.
pub(super) fn production_import_line(dotted: &str, alias: &str) -> String {
    let (package, leaf) = dotted
        .rsplit_once('.')
        .expect("production module path always has at least two segments");
    format!("from {package} import {leaf} as {alias}")
}

/// Claims `raw` as a unique member name (attribute + class/codec prefix)
/// inside one link mock; a collision (e.g. a topic and a service sharing a
/// name on one link) is a hard error naming both, mirroring the scaffold's
/// collision policy.

fn claim_member_name(
    link_id: &str,
    raw: &str,
    seen: &mut HashSet<String>,
    first_owner: &mut HashMap<String, String>,
) -> Result<String> {
    let sanitized = sanitize_python_module_name(raw);
    if !seen.insert(sanitized.clone()) {
        return Err(Error::ModuleNameCollision {
            category: "mock".to_string(),
            first: first_owner
                .get(&sanitized)
                .cloned()
                .unwrap_or_else(|| raw.to_string()),
            second: format!("{link_id}/{raw}"),
            sanitized,
        });
    }
    first_owner.insert(sanitized.clone(), format!("{link_id}/{raw}"));
    Ok(sanitized)
}

/// Registers `schema_key` (deduping onto the production capnp file) and
/// emits its `@lru_cache` loader once per output file.
fn register_with_loader(
    generator: &mut PythonGenerator,
    builder: &mut PythonCodeBuilder,
    emitted_loaders: &mut HashSet<String>,
    schema_key: &str,
    format: &MessageFormat,
) -> Result<PythonSchemaInfo> {
    let info = generator.register_schema(schema_key, format)?;
    emit_capnp_preamble(builder);
    if emitted_loaders.insert(info.file_stem.clone()) {
        emit_capnp_loader_fn(builder, &info);
    }
    Ok(info)
}

/// A `def <fn_name>(value: <value_type>) -> bytes` serializer over an
/// existing dataclass, registered on `schema_key` (dedupes onto the
/// production schema file).
pub(super) fn emit_value_serializer(
    generator: &mut PythonGenerator,
    builder: &mut PythonCodeBuilder,
    emitted_loaders: &mut HashSet<String>,
    fn_name: &str,
    schema_key: &str,
    format: &MessageFormat,
    value_type: &str,
) -> Result<()> {
    let info = register_with_loader(generator, builder, emitted_loaders, schema_key, format)?;
    builder.line(&format!("def {fn_name}(value: {value_type}) -> bytes:"));
    builder.indent();
    builder.line(&format!(
        "capnp_msg = {}().{}.new_message()",
        capnp_loader_fn_name(&info),
        info.struct_name
    ));
    let mut counter = 0u32;
    serialization::emit_capnp_assignments(builder, "capnp_msg", format, "value", &mut counter);
    builder.line("return capnp_msg.to_bytes()");
    builder.dedent();
    builder.blank_line();
    Ok(())
}

/// A `def <fn_name>(payload: bytes) -> <struct_prefix>` deserializer
/// constructing `<struct_prefix>(…)`. `struct_prefix` may be a dotted
/// `_alias.Class` reference into a production module: the nested-companion
/// references the emitter derives (`{struct_prefix}{CamelField}`) then
/// resolve to that module's own nested dataclass names.
pub(super) fn emit_value_deserializer(
    generator: &mut PythonGenerator,
    builder: &mut PythonCodeBuilder,
    emitted_loaders: &mut HashSet<String>,
    fn_name: &str,
    schema_key: &str,
    format: &MessageFormat,
    struct_prefix: &str,
) -> Result<()> {
    let info = register_with_loader(generator, builder, emitted_loaders, schema_key, format)?;
    deserialization::build_deserialize_fn(
        builder,
        &info,
        format,
        struct_prefix,
        &format!("{}()", capnp_loader_fn_name(&info)),
        fn_name,
    );
    builder.blank_line();
    Ok(())
}

/// A locally-defined dataclass (plus nested companions) and its deserializer,
/// for directions where production defines no consumer dataclass (topics the
/// node emits: the producer side only serializes).
pub(super) fn emit_local_message_with_deserializer(
    generator: &mut PythonGenerator,
    builder: &mut PythonCodeBuilder,
    emitted_loaders: &mut HashSet<String>,
    schema_key: &str,
    format: &MessageFormat,
    class_name: &str,
    fn_name: &str,
) -> Result<()> {
    let info = register_with_loader(generator, builder, emitted_loaders, schema_key, format)?;
    emit_format_as_dataclass(builder, class_name, format)?;
    deserialization::build_deserialize_fn(
        builder,
        &info,
        format,
        class_name,
        &format!("{}()", capnp_loader_fn_name(&info)),
        fn_name,
    );
    builder.blank_line();
    Ok(())
}

/// One per-interface member of a link mock.
struct MockMember {
    /// The sanitized attribute name on `Mock`.
    attr: String,
    /// The `await <Class>._declare(...)`-style construction statement placed
    /// in `Mock.start`, binding a local named `attr`.
    construct: String,
    /// `self.<attr>._stop()` for action members (disarm before drop).
    action_stop: Option<String>,
    /// `await self.<attr>._close()` for service members (pump teardown).
    service_close: Option<String>,
}

/// Emits the shared `Mock` aggregate class from its member list.
#[allow(clippy::too_many_arguments)]
fn emit_mock_class(
    builder: &mut PythonCodeBuilder,
    doc: &str,
    start_doc: &str,
    stop_doc: &str,
    start_params: &str,
    start_prologue: &[String],
    members: &[MockMember],
) {
    builder.line("class Mock:");
    builder.indent();
    builder.line(&format!("\"\"\"{doc}\"\"\""));
    builder.blank_line();

    let mut init_params = vec!["self".to_string(), "session".to_string()];
    init_params.extend(members.iter().map(|member| member.attr.clone()));
    builder.line(&format!("def __init__({}) -> None:", init_params.join(", ")));
    builder.indent();
    builder.line("self._session = session");
    builder.line("self._stopped = False");
    for member in members {
        builder.line(&format!("self.{attr} = {attr}", attr = member.attr));
    }
    builder.dedent();
    builder.blank_line();

    builder.line("@classmethod");
    builder.line(&format!("async def start({start_params}) -> \"Mock\":"));
    builder.indent();
    builder.line(&format!("\"\"\"{start_doc}\"\"\""));
    for line in start_prologue {
        builder.line(line);
    }
    builder.line("session = await router.connect()");
    for member in members {
        builder.line(&member.construct);
    }
    let mut construct_args = vec!["session".to_string()];
    construct_args.extend(members.iter().map(|member| member.attr.clone()));
    builder.line(&format!("return cls({})", construct_args.join(", ")));
    builder.dedent();
    builder.blank_line();

    builder.line("async def stop(self) -> None:");
    builder.indent();
    builder.line(&format!("\"\"\"{stop_doc}\"\"\""));
    builder.line("if self._stopped:");
    builder.indent();
    builder.line("return");
    builder.dedent();
    builder.line("self._stopped = True");
    // Rust ordering: live action goals are disarmed first, then everything
    // drops. Python adds the service pump teardown (no Drop hook exists), and
    // CPython refcounting releases the declarations and the session.
    for member in members {
        if let Some(stop) = &member.action_stop {
            builder.line(stop);
        }
    }
    for member in members {
        if let Some(close) = &member.service_close {
            builder.line(close);
        }
    }
    for member in members {
        builder.line(&format!("self.{} = None", member.attr));
    }
    builder.line("self._session = None");
    builder.dedent();
    builder.dedent();
    builder.blank_line();
}

// ---------------------------------------------------------------------------
// deps
// ---------------------------------------------------------------------------

fn render_dep_link(
    generator: &mut PythonGenerator,
    link_id: &str,
    spec: &DepLinkSpec,
) -> Result<String> {
    let mut builder = PythonCodeBuilder::new();
    builder.add_import("import peppylib");
    builder.add_import("import peppylib.testing");
    builder.add_import("from typing import Optional");
    let mut loaders = HashSet::new();
    let mut seen = HashSet::new();
    let mut owners = HashMap::new();
    let target = target_python_expr(&spec.target);
    let default_instance = mock_instance_id(link_id);

    builder.line(&format!("LINK_ID = {link_id:?}"));
    builder.line(&format!("MOCK_CORE_NODE = {MOCK_CORE_NODE:?}"));
    builder.line(&format!("MOCK_INSTANCE_ID = {default_instance:?}"));
    builder.blank_line();

    builder.line("def producer_ref() -> peppylib.ProducerRef:");
    builder.indent();
    builder.line("\"\"\"The default mock's wire identity, as the harness seeds it.\"\"\"");
    builder.line("return producer_ref_for(MOCK_INSTANCE_ID)");
    builder.dedent();
    builder.blank_line();

    builder.line("def producer_ref_for(instance_id: str) -> peppylib.ProducerRef:");
    builder.indent();
    builder.line(
        "\"\"\"`producer_ref` under an explicit instance id (multi-instance slots).\"\"\"",
    );
    builder.line("return peppylib.ProducerRef(MOCK_CORE_NODE, instance_id)");
    builder.dedent();
    builder.blank_line();

    let mut members: Vec<MockMember> = Vec::new();
    for topic in &spec.topics {
        let member = dep_member_name(link_id, &topic.module_link, &topic.name);
        let attr = claim_member_name(link_id, &member, &mut seen, &mut owners)?;
        members.push(render_dep_topic(
            generator,
            &mut builder,
            &mut loaders,
            link_id,
            topic,
            &target,
            attr,
        )?);
    }
    for service in &spec.services {
        let member = dep_member_name(link_id, &service.module_link, &service.name);
        let attr = claim_member_name(link_id, &member, &mut seen, &mut owners)?;
        members.push(render_dep_service(
            generator,
            &mut builder,
            &mut loaders,
            link_id,
            &spec.producer_name,
            service,
            &target,
            attr,
        )?);
    }
    for action in &spec.actions {
        let member = dep_member_name(link_id, &action.module_link, &action.name);
        let attr = claim_member_name(link_id, &member, &mut seen, &mut owners)?;
        members.push(render_dep_action(
            generator,
            &mut builder,
            &mut loaders,
            link_id,
            &spec.producer_name,
            action,
            &target,
            attr,
        )?);
    }

    let doc = format!(
        "Mock producer for the `{link_id}` dependency slot: plays the dependency over \
         the real wire under the identity the harness seeds into the slot's bound set. \
         One messaging session per mock, so `stop()` is a whole-producer loss."
    );
    emit_mock_class(
        &mut builder,
        &doc,
        "Connects a dedicated session and declares every interface of this dependency \
         now (publishers, queryables, action engines), under `MOCK_INSTANCE_ID` unless \
         an explicit instance id is given (multi-instance slots).",
        "Simulates this producer dying mid-flight, deterministically: live action \
         goals are disarmed (their eventual release emits no clean close), then every \
         declaration and the session are released, dropping the liveliness tokens \
         consumers latch on.",
        "cls, router: peppylib.testing.EphemeralRouter, instance_id: Optional[str] = None",
        &["if instance_id is None:".to_string(), "    instance_id = MOCK_INSTANCE_ID".to_string()],
        &members,
    );

    let module_doc = format!(
        "Mock producer for the `{link_id}` dependency slot (generated; see `Mock`)."
    );
    Ok(format!("\"\"\"{module_doc}\"\"\"\n\n{}", builder.build()))
}

#[allow(clippy::too_many_arguments)]
fn render_dep_topic(
    generator: &mut PythonGenerator,
    builder: &mut PythonCodeBuilder,
    loaders: &mut HashSet<String>,
    _link_id: &str,
    spec: &DepTopicSpec,
    target: &str,
    attr: String,
) -> Result<MockMember> {
    let topic_name = spec.name.as_str();
    let production = production_module_path("consumed_topics", &[&spec.module_link, topic_name]);
    let alias = format!("_{attr}");
    builder.add_import(&production_import_line(&production, &alias));
    let camel = to_camel_case(&attr);
    let schema_key =
        crate::generator::naming::consumed_topic_schema_key(&spec.module_link, topic_name);
    let serialize_fn = format!("_serialize_{attr}_message");
    emit_value_serializer(
        generator,
        builder,
        loaders,
        &serialize_fn,
        &schema_key,
        &spec.format,
        &format!("{alias}.Message"),
    )?;

    // Re-export the production message dataclass under a member-prefixed name
    // (one flat file per link; the Rust veneer re-exports per sub-module).
    builder.line(&format!("{camel}Message = {alias}.Message"));
    builder.blank_line();

    builder.line(&format!("class {camel}Publisher:"));
    builder.indent();
    builder.line(&format!(
        "\"\"\"Typed mock publisher for the consumed topic `{topic_name}`: publishes over \
         the real wire as this mock's identity; the first publish waits for the node's \
         subscription to match (no sleeps), and no subscriber within the readiness \
         timeout is a loud error.\"\"\""
    ));
    builder.blank_line();
    builder.line("def __init__(self, core) -> None:");
    builder.indent();
    builder.line("self._core = core");
    builder.dedent();
    builder.blank_line();
    builder.line("@classmethod");
    builder.line("async def _declare(cls, session, instance_id):");
    builder.indent();
    builder.line("core = await peppylib.testing.TestTopicPublisher.declare(");
    builder.indent();
    builder.line("session,");
    builder.line("MOCK_CORE_NODE,");
    builder.line("instance_id,");
    builder.line(&format!("{target},"));
    builder.line(&format!("{topic_name:?},"));
    builder.line("peppylib.QoSProfile.Standard,");
    builder.dedent();
    builder.line(")");
    builder.line("return cls(core)");
    builder.dedent();
    builder.blank_line();
    builder.line(&format!(
        "async def publish(self, message: {alias}.Message) -> None:"
    ));
    builder.indent();
    builder.line(
        "\"\"\"Publishes one typed message; lazily waits for the node's subscription \
         before the first delivery.\"\"\"",
    );
    builder.line(&format!("await self._core.publish({serialize_fn}(message))"));
    builder.dedent();
    builder.blank_line();
    builder.line("async def wait_for_subscriber(self, timeout: float) -> bool:");
    builder.indent();
    builder.line(
        "\"\"\"Waits until the node's subscription for this topic is visible; returns \
         whether it matched within `timeout`.\"\"\"",
    );
    builder.line("return await self._core.wait_for_subscriber(timeout)");
    builder.dedent();
    builder.dedent();
    builder.blank_line();

    Ok(MockMember {
        construct: format!("{attr} = await {camel}Publisher._declare(session, instance_id)"),
        attr,
        action_stop: None,
        service_close: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn render_dep_service(
    generator: &mut PythonGenerator,
    builder: &mut PythonCodeBuilder,
    loaders: &mut HashSet<String>,
    _link_id: &str,
    producer_name: &str,
    spec: &DepServiceSpec,
    target: &str,
    attr: String,
) -> Result<MockMember> {
    let service_name = spec.name.as_str();
    let production =
        production_module_path("consumed_services", &[&spec.module_link, service_name]);
    let alias = format!("_{attr}");
    builder.add_import(&production_import_line(&production, &alias));
    let camel = to_camel_case(&attr);
    let request_key = crate::generator::naming::consumed_service_request_schema_key(
        producer_name,
        service_name,
    );
    let response_key = crate::generator::naming::consumed_service_response_schema_key(
        producer_name,
        service_name,
    );
    let deserialize_fn = format!("_deserialize_{attr}_request");
    let serialize_fn = format!("_serialize_{attr}_response");

    if let Some(format) = &spec.request {
        emit_value_deserializer(
            generator,
            builder,
            loaders,
            &deserialize_fn,
            &request_key,
            format,
            &format!("{alias}.Request"),
        )?;
        builder.line(&format!("{camel}Request = {alias}.Request"));
        builder.blank_line();
    }
    if let Some(format) = &spec.response {
        emit_value_serializer(
            generator,
            builder,
            loaders,
            &serialize_fn,
            &response_key,
            format,
            &format!("{alias}.ResponseData"),
        )?;
        builder.line(&format!("{camel}ResponseData = {alias}.ResponseData"));
        builder.blank_line();
    }

    let (response_param, response_payload) = if spec.response.is_some() {
        (
            format!("response: {alias}.ResponseData"),
            format!("{serialize_fn}(response)"),
        )
    } else {
        (String::new(), "b\"\"".to_string())
    };

    // Responder first, so the Service annotations referencing it resolve at
    // definition time.
    builder.line(&format!("class {camel}Responder:"));
    builder.indent();
    builder.line("\"\"\"Answers exactly one parked request; consumed by use.\"\"\"");
    builder.blank_line();
    builder.line("def __init__(self, inner) -> None:");
    builder.indent();
    builder.line("self._inner = inner");
    builder.dedent();
    builder.blank_line();
    if response_param.is_empty() {
        builder.line("async def respond(self) -> None:");
    } else {
        builder.line(&format!("async def respond(self, {response_param}) -> None:"));
    }
    builder.indent();
    builder.line("\"\"\"Sends the typed response for the parked request.\"\"\"");
    builder.line(&format!("await self._inner.respond({response_payload})"));
    builder.dedent();
    builder.blank_line();
    builder.line("async def respond_error(self, reason: str) -> None:");
    builder.indent();
    builder.line("\"\"\"Fails the parked request with a handler-error reason.\"\"\"");
    builder.line("await self._inner.respond_error(reason)");
    builder.dedent();
    builder.dedent();
    builder.blank_line();

    builder.line(&format!("class {camel}Service:"));
    builder.indent();
    builder.line(&format!(
        "\"\"\"Typed mock server for the consumed service `{service_name}`: a background \
         pump captures every request; scripted responses serve automatically, unscripted \
         requests park for `next_request`.\"\"\""
    ));
    builder.blank_line();
    builder.line("def __init__(self, core) -> None:");
    builder.indent();
    builder.line("self._core = core");
    builder.dedent();
    builder.blank_line();
    builder.line("@classmethod");
    builder.line("async def _listen(cls, session, instance_id):");
    builder.indent();
    builder.line("core = await peppylib.testing.MockServiceCore.listen(");
    builder.indent();
    builder.line("session,");
    builder.line("MOCK_CORE_NODE,");
    builder.line("instance_id,");
    builder.line(&format!("{target},"));
    builder.line(&format!("{service_name:?},"));
    builder.dedent();
    builder.line(")");
    builder.line("return cls(core)");
    builder.dedent();
    builder.blank_line();

    if spec.request.is_some() {
        builder.add_import("from typing import Tuple");
        builder.line(&format!(
            "async def next_request(self, timeout: float) -> Tuple[{alias}.Request, {camel}Responder]:"
        ));
        builder.indent();
        builder.line(
            "\"\"\"The next unscripted request, decoded, with the responder the test must \
             use to answer it. Errors after `timeout`.\"\"\"",
        );
        builder.line("context, responder = await self._core.next_request(timeout)");
        builder.line(&format!("request = {deserialize_fn}(context.payload)"));
        builder.line(&format!("return request, {camel}Responder(responder)"));
        builder.dedent();
        builder.blank_line();
    } else {
        builder.line(&format!(
            "async def next_request(self, timeout: float) -> {camel}Responder:"
        ));
        builder.indent();
        builder.line(
            "\"\"\"Parks until the node's next unscripted call, returning the responder \
             the test must use to answer it. Errors after `timeout`.\"\"\"",
        );
        builder.line("_context, responder = await self._core.next_request(timeout)");
        builder.line(&format!("return {camel}Responder(responder)"));
        builder.dedent();
        builder.blank_line();
    }

    if response_param.is_empty() {
        builder.line("def enqueue_response(self) -> None:");
    } else {
        builder.line(&format!("def enqueue_response(self, {response_param}) -> None:"));
    }
    builder.indent();
    builder.line(
        "\"\"\"Enqueues one response to be served automatically to the next inbound \
         request (FIFO across repeated calls); only unscripted requests park for \
         `next_request`.\"\"\"",
    );
    builder.line(&format!("self._core.enqueue_response({response_payload})"));
    builder.dedent();
    builder.blank_line();

    if spec.request.is_some() {
        builder.add_import("from typing import List");
        builder.line(&format!("def captured(self) -> List[{alias}.Request]:"));
        builder.indent();
        builder.line(
            "\"\"\"Every request received so far (scripted and manual alike), decoded, \
             in arrival order.\"\"\"",
        );
        builder.line("return [");
        builder.indent();
        builder.line(&format!("{deserialize_fn}(captured.payload)"));
        builder.line("for captured in self._core.captured()");
        builder.dedent();
        builder.line("]");
        builder.dedent();
        builder.blank_line();
    } else {
        builder.line("def captured_count(self) -> int:");
        builder.indent();
        builder.line("\"\"\"How many calls the node has made so far.\"\"\"");
        builder.line("return len(self._core.captured())");
        builder.dedent();
        builder.blank_line();
    }

    builder.line("async def _close(self) -> None:");
    builder.indent();
    builder.line("await self._core.close()");
    builder.dedent();
    builder.dedent();
    builder.blank_line();

    Ok(MockMember {
        construct: format!("{attr} = await {camel}Service._listen(session, instance_id)"),
        action_stop: None,
        service_close: Some(format!("await self.{attr}._close()")),
        attr,
    })
}

#[allow(clippy::too_many_arguments)]
fn render_dep_action(
    generator: &mut PythonGenerator,
    builder: &mut PythonCodeBuilder,
    loaders: &mut HashSet<String>,
    _link_id: &str,
    producer_name: &str,
    spec: &DepActionSpec,
    target: &str,
    attr: String,
) -> Result<MockMember> {
    let action_name = spec.name.as_str();
    let production =
        production_module_path("consumed_actions", &[&spec.module_link, action_name]);
    let alias = format!("_{attr}");
    builder.add_import(&production_import_line(&production, &alias));
    let camel = to_camel_case(&attr);
    let keys =
        crate::generator::naming::consumed_action_schema_keys(producer_name, action_name);
    let has_feedback = spec.messages.feedback.is_some();

    let deserialize_request_fn = format!("_deserialize_{attr}_goal_request");
    let serialize_response_fn = format!("_serialize_{attr}_goal_response");
    let serialize_feedback_fn = format!("_serialize_{attr}_feedback");
    let serialize_result_fn = format!("_serialize_{attr}_result");

    if let Some(format) = &spec.messages.goal_request {
        emit_value_deserializer(
            generator,
            builder,
            loaders,
            &deserialize_request_fn,
            &keys.goal_request,
            format,
            &format!("{alias}.GoalRequest"),
        )?;
        builder.line(&format!("{camel}GoalRequest = {alias}.GoalRequest"));
        builder.blank_line();
    }
    if let Some(format) = &spec.messages.goal_response {
        emit_value_serializer(
            generator,
            builder,
            loaders,
            &serialize_response_fn,
            &keys.goal_response,
            format,
            &format!("{alias}.GoalResponseData"),
        )?;
        builder.line(&format!("{camel}GoalResponseData = {alias}.GoalResponseData"));
        builder.blank_line();
    }
    if let Some(format) = &spec.messages.feedback {
        emit_value_serializer(
            generator,
            builder,
            loaders,
            &serialize_feedback_fn,
            &keys.feedback,
            format,
            &format!("{alias}.FeedbackMessage"),
        )?;
        builder.line(&format!("{camel}FeedbackMessage = {alias}.FeedbackMessage"));
        builder.blank_line();
    }
    if let Some(format) = &spec.messages.result_response {
        emit_value_serializer(
            generator,
            builder,
            loaders,
            &serialize_result_fn,
            &keys.result_response,
            format,
            &format!("{alias}.ResultResponseData"),
        )?;
        builder.line(&format!(
            "{camel}ResultResponseData = {alias}.ResultResponseData"
        ));
        builder.blank_line();
    }

    let (complete_param, complete_payload) = if spec.messages.result_response.is_some() {
        (
            format!("result: {alias}.ResultResponseData"),
            format!("{serialize_result_fn}(result)"),
        )
    } else {
        (String::new(), "b\"\"".to_string())
    };

    // ActiveGoal first, then PendingGoal, then Action: annotations resolve at
    // definition time without forward references.
    builder.line(&format!("class {camel}ActiveGoal:"));
    builder.indent();
    builder.line("\"\"\"An accepted goal: drives feedback and terminal completion.\"\"\"");
    builder.blank_line();
    builder.line("def __init__(self, context) -> None:");
    builder.indent();
    builder.line("self._context = context");
    builder.dedent();
    builder.blank_line();
    if has_feedback {
        builder.line(&format!(
            "async def publish_feedback(self, feedback: {alias}.FeedbackMessage) -> None:"
        ));
        builder.indent();
        builder.line("\"\"\"Publishes one typed feedback message for this goal.\"\"\"");
        builder.line(&format!(
            "await self._context.publish_feedback({serialize_feedback_fn}(feedback))"
        ));
        builder.dedent();
        builder.blank_line();
    }
    builder.line("async def cancel_signal(self) -> None:");
    builder.indent();
    builder.line("\"\"\"Resolves when the node requests cancellation of this goal.\"\"\"");
    builder.line("await self._context.cancel_signal()");
    builder.dedent();
    builder.blank_line();
    builder.line("def is_cancelled(self) -> bool:");
    builder.indent();
    builder.line("\"\"\"Whether cancellation has been requested for this goal.\"\"\"");
    builder.line("return self._context.is_cancelled()");
    builder.dedent();
    builder.blank_line();
    for method in ["complete", "complete_cancelled"] {
        let doc = if method == "complete" {
            "Completes the goal successfully."
        } else {
            "Completes the goal as cancelled."
        };
        if complete_param.is_empty() {
            builder.line(&format!("async def {method}(self) -> None:"));
        } else {
            builder.line(&format!("async def {method}(self, {complete_param}) -> None:"));
        }
        builder.indent();
        builder.line(&format!("\"\"\"{doc}\"\"\""));
        builder.line(&format!("await self._context.{method}({complete_payload})"));
        builder.dedent();
        builder.blank_line();
    }
    builder.dedent();
    builder.blank_line();

    builder.line(&format!("class {camel}PendingGoal:"));
    builder.indent();
    builder.line("\"\"\"A goal received by the mock, awaiting accept/reject.\"\"\"");
    builder.blank_line();
    if spec.messages.goal_request.is_some() {
        builder.line("def __init__(self, pending, request) -> None:");
        builder.indent();
        builder.line("self._pending = pending");
        builder.line("#: The decoded goal request.");
        builder.line("self.request = request");
        builder.dedent();
    } else {
        builder.line("def __init__(self, pending) -> None:");
        builder.indent();
        builder.line("self._pending = pending");
        builder.dedent();
    }
    builder.blank_line();
    builder.line("@property");
    builder.line("def goal_id(self) -> str:");
    builder.indent();
    builder.line("\"\"\"The client-generated correlation id for this goal.\"\"\"");
    builder.line("return self._pending.goal_id");
    builder.dedent();
    builder.blank_line();
    let (accept_param, accept_payload, reject_param, reject_payload) =
        if spec.messages.goal_response.is_some() {
            (
                format!(", response: {alias}.GoalResponseData"),
                format!("{serialize_response_fn}(response)"),
                format!(
                    ", reason: Optional[str] = None, response: Optional[{alias}.GoalResponseData] = None"
                ),
                format!(
                    "{serialize_response_fn}(response) if response is not None else b\"\""
                ),
            )
        } else {
            (
                String::new(),
                "b\"\"".to_string(),
                ", reason: Optional[str] = None".to_string(),
                "b\"\"".to_string(),
            )
        };
    builder.line(&format!(
        "async def accept(self{accept_param}) -> {camel}ActiveGoal:"
    ));
    builder.indent();
    builder.line(
        "\"\"\"Accepts the goal; the returned handle drives feedback and completion.\"\"\"",
    );
    builder.line(&format!(
        "context = await self._pending.accept({accept_payload})"
    ));
    builder.line(&format!("return {camel}ActiveGoal(context)"));
    builder.dedent();
    builder.blank_line();
    builder.line(&format!("async def reject(self{reject_param}) -> None:"));
    builder.indent();
    builder.line("\"\"\"Rejects the goal with an optional human-readable reason.\"\"\"");
    builder.line(&format!("await self._pending.reject(reason, {reject_payload})"));
    builder.dedent();
    builder.dedent();
    builder.blank_line();

    builder.line(&format!("class {camel}Action:"));
    builder.indent();
    builder.line(&format!(
        "\"\"\"Typed mock action server for the consumed action `{action_name}`, on the \
         real `ConcurrentAction` engine: identical goal lifecycle to a production \
         producer, plus deterministic producer-loss via the owning mock's `stop()`.\"\"\""
    ));
    builder.blank_line();
    builder.line("def __init__(self, core) -> None:");
    builder.indent();
    builder.line("self._core = core");
    builder.dedent();
    builder.blank_line();
    builder.line("@classmethod");
    builder.line("async def _expose(cls, session, instance_id):");
    builder.indent();
    builder.line("core = await peppylib.testing.MockActionServerCore.expose(");
    builder.indent();
    builder.line("session,");
    builder.line("MOCK_CORE_NODE,");
    builder.line("instance_id,");
    builder.line(&format!("{target},"));
    builder.line(&format!("{action_name:?},"));
    builder.line(if has_feedback { "True," } else { "False," });
    builder.dedent();
    builder.line(")");
    builder.line("return cls(core)");
    builder.dedent();
    builder.blank_line();
    builder.line(&format!(
        "async def next_goal(self, timeout: float) -> {camel}PendingGoal:"
    ));
    builder.indent();
    builder.line(
        "\"\"\"Parks until the node sends a goal, bounded by `timeout`; the returned \
         goal awaits the test's admission decision.\"\"\"",
    );
    builder.line("pending = await self._core.next_goal(timeout)");
    if spec.messages.goal_request.is_some() {
        builder.line(&format!(
            "request = {deserialize_request_fn}(pending.request_bytes)"
        ));
        builder.line(&format!("return {camel}PendingGoal(pending, request)"));
    } else {
        builder.line(&format!("return {camel}PendingGoal(pending)"));
    }
    builder.dedent();
    builder.blank_line();
    builder.line("def _stop(self) -> None:");
    builder.indent();
    builder.line("self._core.stop()");
    builder.dedent();
    builder.dedent();
    builder.blank_line();

    Ok(MockMember {
        construct: format!("{attr} = await {camel}Action._expose(session, instance_id)"),
        action_stop: Some(format!("self.{attr}._stop()")),
        service_close: None,
        attr,
    })
}

// ---------------------------------------------------------------------------
// pairings
// ---------------------------------------------------------------------------

fn render_pairing_link(
    generator: &mut PythonGenerator,
    link_id: &str,
    spec: &PairingLinkSpec,
) -> Result<String> {
    let mut builder = PythonCodeBuilder::new();
    builder.add_import("import peppylib");
    builder.add_import("import peppylib.testing");
    let mut loaders = HashSet::new();
    let mut seen = HashSet::new();
    let mut owners = HashMap::new();
    let pairing_target = target_python_expr(&TargetSpec::Pairing {
        name: spec.pairing_name.clone(),
        tag: spec.pairing_tag.clone(),
    });
    let default_instance = mock_instance_id(link_id);

    builder.line(&format!("LINK_ID = {link_id:?}"));
    builder.line(&format!("PAIRING_NAME = {:?}", spec.pairing_name));
    builder.line(&format!("PAIRING_TAG = {:?}", spec.pairing_tag));
    builder.line(&format!("PEER_LINK_ID = {MOCK_PEER_LINK_ID:?}"));
    builder.line(&format!("MOCK_CORE_NODE = {MOCK_CORE_NODE:?}"));
    builder.line(&format!("MOCK_INSTANCE_ID = {default_instance:?}"));
    builder.blank_line();

    builder.line("def producer_ref() -> peppylib.ProducerRef:");
    builder.indent();
    builder.line("\"\"\"The mock peer's wire identity.\"\"\"");
    builder.line("return peppylib.ProducerRef(MOCK_CORE_NODE, MOCK_INSTANCE_ID)");
    builder.dedent();
    builder.blank_line();

    builder.line("def peer_info() -> peppylib.PeerInfo:");
    builder.indent();
    builder.line(
        "\"\"\"The full pin identity the harness seeds for this slot: what the node's \
         `paired()` / `wait_paired()` resolve to.\"\"\"",
    );
    builder.line("return peppylib.PeerInfo(producer_ref(), PEER_LINK_ID)");
    builder.dedent();
    builder.blank_line();

    let mut members: Vec<MockMember> = Vec::new();
    // Topics the node consumes: the mock publishes as the paired peer.
    for topic in &spec.node_consumes {
        let attr = claim_member_name(link_id, &topic.name, &mut seen, &mut owners)?;
        members.push(render_pair_publisher(
            generator,
            &mut builder,
            &mut loaders,
            link_id,
            topic,
            &pairing_target,
            attr,
        )?);
    }
    // Topics the node emits: the mock subscribes, triple-pinned to the node.
    for topic in &spec.node_emits {
        let attr = claim_member_name(link_id, &topic.name, &mut seen, &mut owners)?;
        members.push(render_pair_subscription(
            generator,
            &mut builder,
            &mut loaders,
            link_id,
            topic,
            &pairing_target,
            attr,
        )?);
    }

    let doc = format!(
        "Mock peer for the `{link_id}` pairing slot: publishes the topics the node \
         consumes and holds triple-pinned subscriptions to the topics the node emits, \
         under the pin identity the harness seeds (`peer_info()`)."
    );
    emit_mock_class(
        &mut builder,
        &doc,
        "Connects a dedicated session, declares the peer publishers and opens the \
         pinned subscriptions to the node's emissions. `node_instance_id` is the \
         node-under-test's instance id (the harness passes its own).",
        "Simulates the peer disappearing: every declaration and the session are \
         released.",
        "cls, router: peppylib.testing.EphemeralRouter, node_instance_id: str",
        &[],
        &members,
    );

    let module_doc =
        format!("Mock peer for the `{link_id}` pairing slot (generated; see `Mock`).");
    Ok(format!("\"\"\"{module_doc}\"\"\"\n\n{}", builder.build()))
}

fn render_pair_publisher(
    generator: &mut PythonGenerator,
    builder: &mut PythonCodeBuilder,
    loaders: &mut HashSet<String>,
    link_id: &str,
    spec: &PairTopicSpec,
    pairing_target: &str,
    attr: String,
) -> Result<MockMember> {
    let topic_name = spec.name.as_str();
    let production = production_module_path("paired_topics", &[link_id, topic_name]);
    let alias = format!("_{attr}");
    builder.add_import(&production_import_line(&production, &alias));
    let camel = to_camel_case(&attr);
    let schema_key = crate::generator::naming::peer_schema_key(link_id, topic_name);
    let qos = qos_profile_python(&spec.qos);
    let serialize_fn = format!("_serialize_{attr}_message");
    emit_value_serializer(
        generator,
        builder,
        loaders,
        &serialize_fn,
        &schema_key,
        &spec.format,
        &format!("{alias}.Message"),
    )?;
    builder.line(&format!("{camel}Message = {alias}.Message"));
    builder.blank_line();

    builder.line(&format!("class {camel}Publisher:"));
    builder.indent();
    builder.line(&format!(
        "\"\"\"Typed peer publisher for `{topic_name}` (the node consumes this \
         direction): publishes under the mock peer's identity and slot id, so the \
         node's triple-pinned subscription receives it.\"\"\""
    ));
    builder.blank_line();
    builder.line("def __init__(self, core) -> None:");
    builder.indent();
    builder.line("self._core = core");
    builder.dedent();
    builder.blank_line();
    builder.line("@classmethod");
    builder.line("async def _declare(cls, session):");
    builder.indent();
    builder.line("core = await peppylib.testing.TestTopicPublisher.declare(");
    builder.indent();
    builder.line("session,");
    builder.line("MOCK_CORE_NODE,");
    builder.line("MOCK_INSTANCE_ID,");
    builder.line(&format!("{pairing_target},"));
    builder.line(&format!("{topic_name:?},"));
    builder.line(&format!("{qos},"));
    builder.line("link_id=PEER_LINK_ID,");
    builder.dedent();
    builder.line(")");
    builder.line("return cls(core)");
    builder.dedent();
    builder.blank_line();
    builder.line(&format!(
        "async def publish(self, message: {alias}.Message) -> None:"
    ));
    builder.indent();
    builder.line(
        "\"\"\"Publishes one typed message; lazily waits for the node's pinned \
         subscription before the first delivery.\"\"\"",
    );
    builder.line(&format!("await self._core.publish({serialize_fn}(message))"));
    builder.dedent();
    builder.blank_line();
    builder.line("async def wait_for_subscriber(self, timeout: float) -> bool:");
    builder.indent();
    builder.line(
        "\"\"\"Waits until the node's pinned subscription is visible; returns whether \
         it matched within `timeout`.\"\"\"",
    );
    builder.line("return await self._core.wait_for_subscriber(timeout)");
    builder.dedent();
    builder.dedent();
    builder.blank_line();

    Ok(MockMember {
        construct: format!("{attr} = await {camel}Publisher._declare(session)"),
        attr,
        action_stop: None,
        service_close: None,
    })
}

fn render_pair_subscription(
    generator: &mut PythonGenerator,
    builder: &mut PythonCodeBuilder,
    loaders: &mut HashSet<String>,
    link_id: &str,
    spec: &PairTopicSpec,
    pairing_target: &str,
    attr: String,
) -> Result<MockMember> {
    let topic_name = spec.name.as_str();
    let camel = to_camel_case(&attr);
    let schema_key = crate::generator::naming::peer_schema_key(link_id, topic_name);
    let qos = qos_profile_python(&spec.qos);
    let deserialize_fn = format!("_deserialize_{attr}_message");
    let message_class = format!("{camel}Message");
    // The node-emitted direction has no production consumer dataclass (the
    // production module only serializes), so the veneer defines its own.
    emit_local_message_with_deserializer(
        generator,
        builder,
        loaders,
        &schema_key,
        &spec.format,
        &message_class,
        &deserialize_fn,
    )?;
    builder.add_import("from typing import Optional");

    builder.line(&format!("class {camel}Subscription:"));
    builder.indent();
    builder.line(&format!(
        "\"\"\"Typed subscription to `{topic_name}` (the node emits this direction): \
         pinned to the node's identity and slot id exactly as a real paired peer \
         would be.\"\"\""
    ));
    builder.blank_line();
    builder.line("def __init__(self, inner) -> None:");
    builder.indent();
    builder.line("self._inner = inner");
    builder.dedent();
    builder.blank_line();
    builder.line("@classmethod");
    builder.line("async def _open(cls, session, node_instance_id):");
    builder.indent();
    builder.line("# The exact wire shape of a paired peer's subscription: node");
    builder.line("# identity, pairing target, and the node's own slot link_id all");
    builder.line("# pinned. No pin-following: the mock's peer (the node under test)");
    builder.line("# is known from construction.");
    builder.line("node = peppylib.ProducerRef(peppylib.testing.STANDALONE_CORE_NODE, node_instance_id)");
    builder.line("inner = await peppylib.TopicMessenger.subscribe_peer_pinned(");
    builder.indent();
    builder.line("session,");
    builder.line("MOCK_CORE_NODE,");
    builder.line("MOCK_INSTANCE_ID,");
    builder.line(&format!("{pairing_target},"));
    builder.line("node,");
    builder.line("LINK_ID,");
    builder.line(&format!("{topic_name:?},"));
    builder.line(&format!("{qos},"));
    builder.dedent();
    builder.line(")");
    builder.line("return cls(inner)");
    builder.dedent();
    builder.blank_line();
    builder.line(&format!(
        "async def next(self) -> Optional[{message_class}]:"
    ));
    builder.indent();
    builder.line(
        "\"\"\"Awaits the node's next message on this topic; `None` once the mock's \
         session closes.\"\"\"",
    );
    builder.line("message = await self._inner.on_next_message()");
    builder.line("if message is None:");
    builder.indent();
    builder.line("return None");
    builder.dedent();
    builder.line(&format!("return {deserialize_fn}(message.payload)"));
    builder.dedent();
    builder.dedent();
    builder.blank_line();

    Ok(MockMember {
        construct: format!("{attr} = await {camel}Subscription._open(session, node_instance_id)"),
        attr,
        action_stop: None,
        service_close: None,
    })
}

// ---------------------------------------------------------------------------
// observed
// ---------------------------------------------------------------------------

fn render_observed_link(
    generator: &mut PythonGenerator,
    link_id: &str,
    spec: &ObservedLinkSpec,
) -> Result<String> {
    let mut builder = PythonCodeBuilder::new();
    builder.add_import("import peppylib");
    builder.add_import("import peppylib.testing");
    builder.add_import("from typing import Optional");
    let mut loaders = HashSet::new();
    let mut seen = HashSet::new();
    let mut owners = HashMap::new();
    let pairing_target = target_python_expr(&TargetSpec::Pairing {
        name: spec.pairing_name.clone(),
        tag: spec.pairing_tag.clone(),
    });
    let default_instance = mock_instance_id(link_id);

    builder.line(&format!("LINK_ID = {link_id:?}"));
    builder.line(&format!("PAIRING_NAME = {:?}", spec.pairing_name));
    builder.line(&format!("PAIRING_TAG = {:?}", spec.pairing_tag));
    builder.line(&format!("SOURCE_LINK_ID = {MOCK_SOURCE_LINK_ID:?}"));
    builder.line(&format!("MOCK_CORE_NODE = {MOCK_CORE_NODE:?}"));
    builder.line(&format!("MOCK_INSTANCE_ID = {default_instance:?}"));
    builder.blank_line();

    builder.line("def source() -> peppylib.ObservedSource:");
    builder.indent();
    builder.line("\"\"\"The default mock source, as the harness seeds it.\"\"\"");
    builder.line("return source_for(MOCK_INSTANCE_ID)");
    builder.dedent();
    builder.blank_line();

    builder.line("def source_for(instance_id: str) -> peppylib.ObservedSource:");
    builder.indent();
    builder.line("\"\"\"`source` under an explicit instance id (multi-member slots).\"\"\"");
    builder.line("return peppylib.ObservedSource(");
    builder.indent();
    builder.line("peppylib.ProducerRef(MOCK_CORE_NODE, instance_id),");
    builder.line("SOURCE_LINK_ID,");
    builder.dedent();
    builder.line(")");
    builder.dedent();
    builder.blank_line();

    let mut members: Vec<MockMember> = Vec::new();
    for topic in &spec.topics {
        let attr = claim_member_name(link_id, &topic.name, &mut seen, &mut owners)?;
        members.push(render_observed_publisher(
            generator,
            &mut builder,
            &mut loaders,
            link_id,
            topic,
            &pairing_target,
            attr,
        )?);
    }

    let doc = format!(
        "Mock observed source for the `{link_id}` observer slot: a publish-only \
         pairing member under the source identity the harness seeds (`source()`); \
         multi-member slots start several instances with distinct instance ids."
    );
    emit_mock_class(
        &mut builder,
        &doc,
        "Connects a dedicated session and declares this source's publishers, under \
         `MOCK_INSTANCE_ID` unless an explicit instance id is given (multi-member \
         slots).",
        "Simulates this source disappearing: every declaration and the session are \
         released. (Standalone observation liveness never changes, so the node keeps \
         observing; messages simply stop.)",
        "cls, router: peppylib.testing.EphemeralRouter, instance_id: Optional[str] = None",
        &["if instance_id is None:".to_string(), "    instance_id = MOCK_INSTANCE_ID".to_string()],
        &members,
    );

    let module_doc = format!(
        "Mock observed source for the `{link_id}` observer slot (generated; see `Mock`)."
    );
    Ok(format!("\"\"\"{module_doc}\"\"\"\n\n{}", builder.build()))
}

fn render_observed_publisher(
    generator: &mut PythonGenerator,
    builder: &mut PythonCodeBuilder,
    loaders: &mut HashSet<String>,
    link_id: &str,
    spec: &PairTopicSpec,
    pairing_target: &str,
    attr: String,
) -> Result<MockMember> {
    let topic_name = spec.name.as_str();
    let production = production_module_path("paired_topics", &[link_id, topic_name]);
    let alias = format!("_{attr}");
    builder.add_import(&production_import_line(&production, &alias));
    let camel = to_camel_case(&attr);
    let schema_key = crate::generator::naming::peer_schema_key(link_id, topic_name);
    let qos = qos_profile_python(&spec.qos);
    let serialize_fn = format!("_serialize_{attr}_message");
    emit_value_serializer(
        generator,
        builder,
        loaders,
        &serialize_fn,
        &schema_key,
        &spec.format,
        &format!("{alias}.Message"),
    )?;
    builder.line(&format!("{camel}Message = {alias}.Message"));
    builder.blank_line();

    builder.line(&format!("class {camel}Publisher:"));
    builder.indent();
    builder.line(&format!(
        "\"\"\"Typed source publisher for the observed topic `{topic_name}`: publishes \
         under the mock source's identity and source link_id, so the node's \
         generation-checked observation subscription receives it.\"\"\""
    ));
    builder.blank_line();
    builder.line("def __init__(self, core) -> None:");
    builder.indent();
    builder.line("self._core = core");
    builder.dedent();
    builder.blank_line();
    builder.line("@classmethod");
    builder.line("async def _declare(cls, session, instance_id):");
    builder.indent();
    builder.line("core = await peppylib.testing.TestTopicPublisher.declare(");
    builder.indent();
    builder.line("session,");
    builder.line("MOCK_CORE_NODE,");
    builder.line("instance_id,");
    builder.line(&format!("{pairing_target},"));
    builder.line(&format!("{topic_name:?},"));
    builder.line(&format!("{qos},"));
    builder.line("link_id=SOURCE_LINK_ID,");
    builder.dedent();
    builder.line(")");
    builder.line("return cls(core)");
    builder.dedent();
    builder.blank_line();
    builder.line(&format!(
        "async def publish(self, message: {alias}.Message) -> None:"
    ));
    builder.indent();
    builder.line(
        "\"\"\"Publishes one typed message; lazily waits for the node's observation \
         subscription before the first delivery.\"\"\"",
    );
    builder.line(&format!("await self._core.publish({serialize_fn}(message))"));
    builder.dedent();
    builder.blank_line();
    builder.line("async def wait_for_subscriber(self, timeout: float) -> bool:");
    builder.indent();
    builder.line(
        "\"\"\"Waits until the node's observation subscription is visible; returns \
         whether it matched within `timeout`.\"\"\"",
    );
    builder.line("return await self._core.wait_for_subscriber(timeout)");
    builder.dedent();
    builder.dedent();
    builder.blank_line();

    Ok(MockMember {
        construct: format!("{attr} = await {camel}Publisher._declare(session, instance_id)"),
        attr,
        action_stop: None,
        service_close: None,
    })
}
