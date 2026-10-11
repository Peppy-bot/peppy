//! Byte-exact golden of every Rust renderer's output and of the Cap'n Proto
//! schema set it registers.
//!
//! The rest of this suite asserts on substrings or compiles the generated
//! crate, which proves that a behaviour is present and that the code builds,
//! but is blind to everything around it: a renderer could rename a local,
//! reorder two items, or drop a doc line and every check would still pass.
//! This test pins the whole emitted surface instead, so a refactor of the
//! token-building layer is provably output-preserving rather than plausibly
//! so.
//!
//! Regenerate after an intentional change:
//!
//! ```sh
//! UPDATE_RUST_GOLDEN=1 cargo test -p generator --lib rust::tests::golden
//! ```
//!
//! and read the resulting diff: that diff is the change.

use super::*;
use crate::generator::golden_support::{
    CONSUMED_ACTION, CONSUMED_SERVICE, CONSUMED_SERVICE_REQUEST, CONSUMED_SERVICE_RESPONSE,
    CONSUMED_TOPIC, CONSUMED_TOPIC_FORMAT, EMITTED, EXPOSED_ACTION_FEEDBACK_ONLY,
    EXPOSED_SERVICE_FULL, GoldenDocument, PEER_TOPIC, assert_matches_golden, full_action_messages,
    parse, peer, push_artifact_section, push_section, register_bodyless_dependency,
    register_own_emitted_topics, register_own_services_and_actions,
};
use crate::generator::types::ConsumedActionMessage;
use config::node::{
    Cardinality, ConsumedAction, ConsumedService, ConsumedTopic, MessageFormat, NativeEmittedTopic,
    NativeExposedAction, NativeExposedService, TopicRetention,
};

const GOLDEN: GoldenDocument = GoldenDocument {
    relative_path: "src/generator/rust/tests/golden/rust_renderers.txt",
    update_env_var: "UPDATE_RUST_GOLDEN",
    test_filter: "rust::tests::golden",
};

/// One field of each shape the codec layer reads and writes differently: a
/// scalar whose name needs normalizing, a string named by a Rust keyword,
/// bytes, time, an optional primitive, a nested object holding an object, an
/// optional object, fixed and dynamic `u8` and scalar arrays, text and data
/// lists, an optional array, and a dynamic array of objects. Only top-level
/// fields can be optional, and a fixed array holds scalars only.
const RICH_FORMAT: &str = r#"
{
  "max-speed": "f32",
  "type": "string",
  payload: "bytes",
  stamp: "time",
  note: { $type: "string", $optional: true },
  pose: {
    $type: "object",
    x: "f64",
    frame: { $type: "object", id: "u32", name: "string" }
  },
  calibration: { $type: "object", $optional: true, gain: "f32" },
  digest: { $type: "array", $items: "u8", $length: 4 },
  raw: { $type: "array", $items: "u8" },
  gains: { $type: "array", $items: "f32", $length: 3 },
  readings: { $type: "array", $items: "i16" },
  names: { $type: "array", $items: "string" },
  chunks: { $type: "array", $items: "bytes" },
  limits: { $type: "array", $items: "f64", $optional: true },
  points: {
    $type: "array",
    $items: { $type: "object", x: "f32", tag: "string" }
  }
}
"#;

const CARDINALITIES: [Cardinality; 4] = [
    Cardinality::One,
    Cardinality::ZeroOrOne,
    Cardinality::OneOrMore,
    Cardinality::ZeroOrMore,
];

fn cardinality_suffix(cardinality: Cardinality) -> &'static str {
    match cardinality {
        Cardinality::One => "one",
        Cardinality::ZeroOrOne => "zero_or_one",
        Cardinality::OneOrMore => "one_or_more",
        Cardinality::ZeroOrMore => "zero_or_more",
    }
}

/// The rendered artifacts of a generator after its mock and fixtures
/// surfaces are rendered, and the schema set it registered.
struct RenderedSurface {
    artifacts: Vec<InterfaceArtifact>,
    schemas: Vec<CapnpSchema>,
}

fn render_test_surfaces(mut generator: RustGenerator) -> RenderedSurface {
    let registry = std::mem::take(&mut generator.testgen);
    super::super::mock::render(&mut generator, &registry).unwrap();
    super::super::fixtures::render(&mut generator, &registry).unwrap();
    let mut schemas: Vec<CapnpSchema> = std::mem::take(&mut generator.schemas)
        .into_values()
        .collect();
    schemas.sort_by(|left, right| left.file_stem().cmp(right.file_stem()));
    RenderedSurface {
        artifacts: generator.into_artifacts(),
        schemas,
    }
}

/// Drives the generator across every branch the renderers distinguish: both
/// origins (manifest and contract), every dependency, pairing and observer
/// cardinality, and each present/absent combination of request, response,
/// goal, feedback and result formats.
fn render_everything() -> RenderedSurface {
    let mut generator = RustGenerator::new();
    generator.set_node_identity("relay_node", "v1");

    // Own surface, manifest-rooted and contract-rooted, with a retaining
    // emitted topic.
    register_own_emitted_topics(&mut generator);
    let retained = NativeEmittedTopic {
        name: "camera_state".to_string(),
        retention: TopicRetention::latest(3).unwrap(),
        ..parse(EMITTED)
    };
    generator.add_emitted_topic(&retained, None).unwrap();
    register_own_services_and_actions(&mut generator);

    // Dependency slots, one per cardinality, each with a topic and a service
    // so the mock and the harness render the shape of every cardinality. The
    // multi-instance slot also carries an action, whose readiness the
    // harness probes once per mock instance.
    for cardinality in CARDINALITIES {
        let link_id = format!("camera_{}", cardinality_suffix(cardinality));
        let dependency = DependencyContext::native("uvc_camera", "v1", &link_id, cardinality);

        let mut topic: ConsumedTopic = parse(CONSUMED_TOPIC);
        topic.link_id = link_id.clone();
        generator
            .add_consumed_topic(
                &topic,
                parse(CONSUMED_TOPIC_FORMAT),
                TopicRetention::LiveOnly,
                &dependency,
            )
            .unwrap();

        let mut service: ConsumedService = parse(CONSUMED_SERVICE);
        service.link_id = link_id.clone();
        generator
            .add_consumed_service(
                &service,
                &parse::<MessageFormat>(CONSUMED_SERVICE_REQUEST),
                &parse::<MessageFormat>(CONSUMED_SERVICE_RESPONSE),
                &dependency,
            )
            .unwrap();

        if cardinality == Cardinality::OneOrMore {
            let mut action: ConsumedAction = parse(CONSUMED_ACTION);
            action.link_id = link_id.clone();
            generator
                .add_consumed_action(&action, &full_action_messages(), &dependency)
                .unwrap();
        }
    }

    // A contract-routed dependency with a retaining topic and an action,
    // exercising the other target flavour on the mock side.
    let contract_dependency =
        DependencyContext::contract("arm_control", "v1", "left_arm", Cardinality::One);
    let mut topic: ConsumedTopic = parse(CONSUMED_TOPIC);
    topic.link_id = "left_arm".to_string();
    generator
        .add_consumed_topic(
            &topic,
            parse(CONSUMED_TOPIC_FORMAT),
            TopicRetention::latest(3).unwrap(),
            &contract_dependency,
        )
        .unwrap();
    let mut action: ConsumedAction = parse(CONSUMED_ACTION);
    action.link_id = "left_arm".to_string();
    generator
        .add_consumed_action(&action, &full_action_messages(), &contract_dependency)
        .unwrap();

    // Pairing slots: the node emits on one slot of each cardinality, whose
    // publisher readiness the harness renders per cardinality, and consumes
    // on a required slot.
    let peer_topic: NativeEmittedTopic = parse(PEER_TOPIC);
    for cardinality in CARDINALITIES {
        let link_id = format!("controller_{}", cardinality_suffix(cardinality));
        generator
            .add_peer_emitted_topic(&peer_topic, &peer(&link_id, cardinality))
            .unwrap();
    }
    generator
        .add_peer_consumed_topic(&peer_topic, &peer("sensor_bus", Cardinality::One))
        .unwrap();

    // Observer slots, one per cardinality.
    for cardinality in CARDINALITIES {
        let link_id = format!("watchers_{}", cardinality_suffix(cardinality));
        generator
            .add_observed_topic(&peer_topic, &peer(&link_id, cardinality), cardinality)
            .unwrap();
    }

    render_test_surfaces(generator)
}

/// [`RICH_FORMAT`] through each entry point of the codec layer once: the
/// writer of an emitted topic and the reader of its fixture, the request
/// reader and response writer of an exposed service and the codecs of its
/// fixture, the subscription reader of a consumed topic and a pairing topic
/// with their mock publishers, the request writer and response reader of a
/// consumed service with its mock, and the feedback codecs of an exposed and
/// a consumed action. The wiring around each entry point is the same for
/// every format, so the scenarios above pin it with small formats.
fn render_rich_formats() -> RenderedSurface {
    let mut generator = RustGenerator::new();
    generator.set_node_identity("rich_node", "v1");
    let rich = || -> MessageFormat { parse(RICH_FORMAT) };

    let mut emitted: NativeEmittedTopic = parse(EMITTED);
    emitted.name = "rich_stream".to_string();
    emitted.message_format = Some(rich());
    generator.add_emitted_topic(&emitted, None).unwrap();

    let mut service: NativeExposedService = parse(EXPOSED_SERVICE_FULL);
    service.name = "rich_service".to_string();
    service.request_message_format = Some(rich());
    service.response_message_format = Some(rich());
    generator.add_exposed_service(&service, None).unwrap();

    let mut action: NativeExposedAction = parse(EXPOSED_ACTION_FEEDBACK_ONLY);
    action.name = "rich_action".to_string();
    action.feedback_topic.as_mut().unwrap().message_format = rich();
    generator.add_exposed_action(&action, None).unwrap();

    let dependency =
        DependencyContext::native("rich_producer", "v1", "rich_link", Cardinality::One);
    let mut topic: ConsumedTopic = parse(CONSUMED_TOPIC);
    topic.link_id = "rich_link".to_string();
    topic.name = "rich_stream".to_string();
    generator
        .add_consumed_topic(&topic, rich(), TopicRetention::LiveOnly, &dependency)
        .unwrap();

    let mut service: ConsumedService = parse(CONSUMED_SERVICE);
    service.link_id = "rich_link".to_string();
    service.name = "rich_service".to_string();
    generator
        .add_consumed_service(&service, &rich(), &rich(), &dependency)
        .unwrap();

    let mut action: ConsumedAction = parse(CONSUMED_ACTION);
    action.link_id = "rich_link".to_string();
    action.name = "rich_action".to_string();
    generator
        .add_consumed_action(
            &action,
            &ConsumedActionMessage {
                goal_request: None,
                goal_response: None,
                feedback: Some(rich()),
                result_response: None,
            },
            &dependency,
        )
        .unwrap();

    let mut peer_topic: NativeEmittedTopic = parse(PEER_TOPIC);
    peer_topic.name = "rich_states".to_string();
    peer_topic.message_format = Some(rich());
    generator
        .add_peer_consumed_topic(&peer_topic, &peer("rich_sensor_bus", Cardinality::One))
        .unwrap();

    render_test_surfaces(generator)
}

/// The surface of the dependency slot of [`register_bodyless_dependency`],
/// whose service and action declare no message bodies at all.
fn render_bodyless_surface() -> RenderedSurface {
    let mut generator = RustGenerator::new();
    generator.set_node_identity("relay_node", "v1");
    register_bodyless_dependency(&mut generator);
    render_test_surfaces(generator)
}

/// The harness of a node with no slots and no own surface at all.
fn render_bare_harness() -> RenderedSurface {
    let mut generator = RustGenerator::new();
    generator.set_node_identity("bare_node", "v1");
    render_test_surfaces(generator)
}

/// Every scenario of this file, in the order the golden document lists them.
fn render_all_scenarios() -> Vec<RenderedSurface> {
    vec![
        render_everything(),
        render_rich_formats(),
        render_bodyless_surface(),
        render_bare_harness(),
    ]
}

/// The rendered surfaces as one reviewable document: each artifact, then
/// each registered schema, scenario by scenario.
fn rendered_document(surfaces: Vec<RenderedSurface>) -> String {
    let mut document = String::new();
    for surface in surfaces {
        for artifact in &surface.artifacts {
            push_artifact_section(&mut document, artifact);
        }
        for schema in &surface.schemas {
            let heading = format!(
                "CapnpSchema :: {file_stem} (key {key}, module {module})",
                file_stem = schema.file_stem(),
                key = schema.schema_key(),
                module = schema.struct_module(),
            );
            push_section(&mut document, &heading, schema.schema());
        }
    }
    document
}

#[test]
fn rust_renderers_emit_the_golden_document() {
    assert_matches_golden(&GOLDEN, &rendered_document(render_all_scenarios()));
}
