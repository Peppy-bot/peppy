use super::*;
use crate::policy::{ImageCodec, OversizePolicy};
use serde::Deserialize;
use std::num::NonZeroU64;
use std::path::Path;

/// Source documents for the bundle golden. The exposure carries the literal
/// fingerprints of these exact contract bytes, matching a published
/// `mcp_exposure/v1` document.
const WALKTHROUGH_EXPOSURE: &str = include_str!("fixtures/camera_and_recording/exposure.json5");
const CAMERA_CONTRACT: &str =
    include_str!("fixtures/camera_and_recording/rgb_camera.contract.json5");
const RECORDING_CONTRACT: &str =
    include_str!("fixtures/camera_and_recording/episode_recording.contract.json5");

/// A `contract/v1` fixture parsed far enough to resolve it: its identity
/// and members, with the fingerprint of the bytes it was parsed from.
struct Fixture {
    sha256: ManifestFingerprint,
    document: ContractDocument,
}

#[derive(Deserialize)]
struct ContractDocument {
    manifest: ContractManifest,
    interfaces: ContractMembers,
}

#[derive(Deserialize)]
struct ContractManifest {
    name: String,
    tag: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ContractMembers {
    topics: Vec<NativeEmittedTopic>,
    services: Vec<NativeExposedService>,
    actions: Vec<NativeExposedAction>,
}

impl Fixture {
    fn members(&self) -> DeclaredMembers<'_> {
        DeclaredMembers {
            topics: &self.document.interfaces.topics,
            services: &self.document.interfaces.services,
            actions: &self.document.interfaces.actions,
        }
    }

    fn resolved(&self) -> ResolvedContract<'_> {
        ResolvedContract {
            name: &self.document.manifest.name,
            tag: &self.document.manifest.tag,
            sha256: &self.sha256,
            members: self.members(),
        }
    }

    /// The fixture as a daemon interface: the same identity and members,
    /// with no fingerprint.
    fn interface(&self) -> ResolvedInterface<'_> {
        ResolvedInterface {
            name: &self.document.manifest.name,
            tag: &self.document.manifest.tag,
            members: self.members(),
        }
    }
}

/// The contract slot behind an entry of a contract target.
fn slot<M>(bound: &BoundMember<M>) -> &BundleContractPin {
    match &bound.source {
        MemberSource::Slot(slot) => slot,
        MemberSource::Daemon(daemon) => panic!("expected a contract slot, got {daemon:?}"),
    }
}

fn fixture(contract_json5: &str) -> Fixture {
    Fixture {
        sha256: ManifestFingerprint::of_bytes(contract_json5.as_bytes()),
        document: serde_json5::from_str(contract_json5).expect("fixture parses"),
    }
}

fn sha_of(contract_json5: &str) -> String {
    ManifestFingerprint::of_bytes(contract_json5.as_bytes()).to_string()
}

fn parse_exposure(exposure_json5: &str) -> McpExposure {
    serde_json5::from_str(exposure_json5).expect("exposure parses")
}

fn validate(exposure_json5: &str, contracts: &[&Fixture]) -> ValidatedExposure {
    let resolved: Vec<ResolvedContract<'_>> = contracts.iter().map(|f| f.resolved()).collect();
    build_exposure_bundle(&parse_exposure(exposure_json5), &resolved, &[])
        .expect("exposure validates")
}

fn build(exposure_json5: &str, contracts: &[&Fixture]) -> ExposureBundle {
    validate(exposure_json5, contracts).bundle
}

fn violations_of(exposure_json5: &str, contracts: &[&Fixture]) -> Vec<String> {
    let resolved: Vec<ResolvedContract<'_>> = contracts.iter().map(|f| f.resolved()).collect();
    build_exposure_bundle(&parse_exposure(exposure_json5), &resolved, &[])
        .expect_err("expected the exposure to be refused")
        .violations
}

/// One-target exposure builder for the violation cases below.
fn camera_exposure(body: &str) -> String {
    format!(
        r#"{{
        peppy_schema: "mcp_exposure/v1",
        manifest: {{ name: "surface", tag: "v1" }},
        server: {{ title: "Surface" }},
        targets: {{
            front_camera: {{
                contract: {{ name: "rgb_camera", tag: "v1", sha256: "{camera_sha}" }},
                {body}
            }},
        }},
    }}"#,
        camera_sha = sha_of(CAMERA_CONTRACT),
    )
}

const INFO_TOOL: &str = r#"services: [
    {
        member: "video_stream_info",
        tool: "cam.info",
        description: "Report stream parameters.",
        operation: "read_only",
        deadline_ms: 2000,
    },
]"#;

#[test]
fn every_entry_is_bound_to_the_member_it_was_validated_against() {
    let validated = validate(
        WALKTHROUGH_EXPOSURE,
        &[&fixture(CAMERA_CONTRACT), &fixture(RECORDING_CONTRACT)],
    );
    let bundle = &validated.bundle;

    // Entry for entry: the slot is the entry's target, the member the one
    // the entry names.
    let bound: Vec<(&str, &str, &str, &str)> = validated
        .resources()
        .map(|(entry, bound)| {
            (
                entry.target.as_str(),
                slot(bound).link_id.as_str(),
                entry.member.as_str(),
                bound.member.name.as_str(),
            )
        })
        .chain(validated.tools().map(|(entry, bound)| {
            (
                entry.target.as_str(),
                slot(bound).link_id.as_str(),
                entry.member.as_str(),
                bound.member.name.as_str(),
            )
        }))
        .chain(validated.tasks().map(|(entry, bound)| {
            (
                entry.target.as_str(),
                slot(bound).link_id.as_str(),
                entry.member.as_str(),
                bound.member.name.as_str(),
            )
        }))
        .collect();
    assert_eq!(
        bound.len(),
        bundle.resources.len() + bundle.tools.len() + bundle.tasks.len(),
        "one bound member per entry"
    );
    for (target, link_id, member, bound_name) in &bound {
        assert_eq!(target, link_id);
        assert_eq!(member, bound_name);
    }

    // The bound member is the contract's own declaration, with the wire
    // format a server lays out, behind the slot with its resolved bytes.
    let (frame, topic) = validated.resources().next().expect("one resource");
    assert_eq!(frame.member, "video_stream");
    assert_eq!(slot(topic).name, "rgb_camera");
    assert_eq!(slot(topic).tag, "v1");
    assert_eq!(slot(topic).sha256, sha_of(CAMERA_CONTRACT));
    assert!(
        topic
            .member
            .message_format
            .as_ref()
            .is_some_and(|format| format.0.contains_key("frame")),
        "the topic's format is the contract's"
    );
    let (_, recording) = validated.tasks().next().expect("one task");
    assert_eq!(slot(recording).name, "episode_recording");
    assert_eq!(slot(recording).link_id, "recorder");
}

#[test]
fn the_walkthrough_exposure_builds_its_bundle() {
    let bundle = build(
        WALKTHROUGH_EXPOSURE,
        &[&fixture(CAMERA_CONTRACT), &fixture(RECORDING_CONTRACT)],
    );

    assert_eq!(bundle.bundle_format, EXPOSURE_BUNDLE_FORMAT);
    assert_eq!(bundle.schema_mapping_version, SCHEMA_MAPPING_VERSION);
    assert_eq!(bundle.exposure.name, "camera_and_recording");
    assert_eq!(bundle.exposure.tag, "v1");
    let links: Vec<(&str, &str)> = bundle
        .surface
        .contracts()
        .iter()
        .map(|pin| (pin.link_id.as_str(), pin.name.as_str()))
        .collect();
    assert_eq!(
        links,
        [
            ("front_camera", "rgb_camera"),
            ("recorder", "episode_recording")
        ]
    );
    assert_eq!(
        bundle.surface.contracts()[0].sha256,
        sha_of(CAMERA_CONTRACT)
    );

    assert_eq!(bundle.resources.len(), 1);
    let frame = &bundle.resources[0];
    assert_eq!(frame.name, "front_camera.latest_frame");
    assert_eq!(frame.uri, "peppy://resource/front_camera.latest_frame");
    assert_eq!(frame.target, "front_camera");
    assert_eq!(frame.member, "video_stream");
    let frame_properties = frame.schema["properties"]
        .as_object()
        .expect("object schema");
    assert_eq!(
        frame_properties.keys().collect::<Vec<_>>(),
        ["header", "encoding", "width", "height"],
        "the document's schema keeps the format's declaration order and leaves out `frame`, \
         which the blob carries"
    );
    assert_eq!(
        frame.schema["required"],
        serde_json::json!(["header", "encoding", "width", "height"])
    );

    assert_eq!(bundle.pictures.len(), 1);
    let look = &bundle.pictures[0];
    assert_eq!(look.name, "front_camera.look");
    assert_eq!(
        look.description,
        "Look through the front-facing camera: the latest frame as a picture."
    );
    assert_eq!(look.target, "front_camera");
    assert_eq!(look.member, "video_stream");
    assert_eq!(look.resource, "front_camera.latest_frame");
    assert_eq!(
        look.input_schema,
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        "a picture tool of a fixed surface takes no argument"
    );
    assert_eq!(
        look.output_schema, frame.schema,
        "the tool answers with the document of its resource"
    );

    assert_eq!(bundle.tools.len(), 2);
    let brightness = &bundle.tools[1];
    assert_eq!(brightness.name, "front_camera.set_brightness");
    assert_eq!(
        brightness.input_schema["properties"]["value"]["minimum"],
        serde_json::json!(-64),
        "restrict bounds are reflected into the published input schema"
    );
    assert_eq!(
        brightness.input_schema["properties"]["value"]["maximum"],
        serde_json::json!(64)
    );

    assert_eq!(bundle.tasks.len(), 1);
    let record = &bundle.tasks[0];
    assert_eq!(record.name, "recorder.record_episode");
    assert!(record.confirmation_required);
    assert_eq!(
        record.input_schema["properties"]["task_name"],
        serde_json::json!({"type": "string"})
    );
    assert_eq!(
        record.output_schema["properties"]["frames_recorded"]["pattern"],
        serde_json::json!("^(0|[1-9][0-9]*)$"),
        "u64 result members are decimal strings"
    );
    let feedback = record.feedback_schema.as_ref().expect("feedback schema");
    assert_eq!(
        feedback["properties"]["frames_recorded"]["type"],
        serde_json::json!("string")
    );
}

/// The committed bundle golden pins the whole catalog at the byte level:
/// names, prose, policies, derived schemas, and pin fingerprints. Regenerate
/// with `UPDATE_CATALOG_GOLDENS=1 cargo test -p peppy-mcp-catalog` and
/// review the diff before committing.
#[test]
fn bundle_golden_matches_committed_output() {
    let bundle = build(
        WALKTHROUGH_EXPOSURE,
        &[&fixture(CAMERA_CONTRACT), &fixture(RECORDING_CONTRACT)],
    );
    let rendered = bundle.to_json_string();
    if std::env::var_os("UPDATE_CATALOG_GOLDENS").is_some() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/validate/goldens/camera_and_recording.bundle.json");
        std::fs::write(&path, &rendered).expect("write golden");
        return;
    }
    assert_eq!(
        rendered,
        include_str!("goldens/camera_and_recording.bundle.json"),
        "the bundle drifted from its golden; run `UPDATE_CATALOG_GOLDENS=1 cargo test -p \
         peppy-mcp-catalog` and review the diff"
    );
}

#[test]
fn a_missing_contract_is_reported() {
    let violations = violations_of(&camera_exposure(INFO_TOOL), &[&fixture(RECORDING_CONTRACT)]);
    assert_eq!(violations.len(), 1);
    assert!(
        violations[0].contains("rgb_camera:v1") && violations[0].contains("was not provided"),
        "{violations:?}"
    );
}

#[test]
fn a_pin_that_does_not_match_the_resolved_bytes_is_reported() {
    let exposure =
        camera_exposure(INFO_TOOL).replace(&sha_of(CAMERA_CONTRACT), &sha_of(RECORDING_CONTRACT));
    let violations = violations_of(&exposure, &[&fixture(CAMERA_CONTRACT)]);
    assert_eq!(violations.len(), 1);
    assert!(violations[0].contains("fingerprint to"), "{violations:?}");
    assert!(
        violations[0].contains(&sha_of(CAMERA_CONTRACT)),
        "{violations:?}"
    );
}

#[test]
fn a_reference_without_a_pin_is_validated_against_the_resolved_bytes() {
    let exposure = camera_exposure(INFO_TOOL)
        .replace(&format!(r#", sha256: "{}""#, sha_of(CAMERA_CONTRACT)), "");
    assert!(!exposure.contains("sha256"), "the reference carries no pin");
    let bundle = build(&exposure, &[&fixture(CAMERA_CONTRACT)]);
    assert_eq!(
        bundle.surface.contracts()[0].sha256,
        sha_of(CAMERA_CONTRACT),
        "the bundle pins the bytes the exposure was validated against"
    );
    assert_eq!(bundle.tools.len(), 1);
}

#[test]
fn a_contract_provided_twice_is_reported() {
    let violations = violations_of(
        &camera_exposure(INFO_TOOL).replace(&sha_of(CAMERA_CONTRACT), &sha_of("{ }")),
        &[&fixture(CAMERA_CONTRACT), &fixture(CAMERA_CONTRACT)],
    );
    assert!(
        violations
            .iter()
            .any(|v| v.contains("provided more than once")),
        "{violations:?}"
    );
}

#[test]
fn selecting_a_member_of_the_wrong_kind_points_at_the_right_section() {
    let violations = violations_of(
        &camera_exposure(
            r#"topics: [
                {
                    member: "video_stream_info",
                    resource: "cam.info_snapshot",
                    description: "Not actually a topic.",
                    freshness: { max_age_ms: 1000 },
                    update: { max_hz: 1 },
                },
            ]"#,
        ),
        &[&fixture(CAMERA_CONTRACT)],
    );
    assert_eq!(violations.len(), 1);
    let violation = &violations[0];
    assert!(violation.contains("declares no such topic"), "{violation}");
    assert!(
        violation.contains("a service with that name exists, select it under `services`"),
        "{violation}"
    );
}

#[test]
fn a_missing_member_lists_what_the_contract_declares() {
    let violations = violations_of(
        &camera_exposure(&INFO_TOOL.replace("video_stream_info", "set_exposure")),
        &[&fixture(CAMERA_CONTRACT)],
    );
    assert_eq!(violations.len(), 1);
    assert!(
        violations[0].contains("declared services: `video_stream_info`, `set_brightness`, `seek`"),
        "{violations:?}"
    );
}

#[test]
fn an_unbounded_topic_without_a_size_policy_is_refused() {
    let violations = violations_of(
        &camera_exposure(
            r#"topics: [
                {
                    member: "video_stream",
                    resource: "cam.latest_frame",
                    description: "Latest frame.",
                    freshness: { max_age_ms: 2000 },
                    update: { max_hz: 2 },
                },
            ]"#,
        ),
        &[&fixture(CAMERA_CONTRACT)],
    );
    assert_eq!(violations.len(), 1);
    assert!(
        violations[0].contains("no static maximum"),
        "{violations:?}"
    );
    assert!(
        violations[0].contains("`max_result_bytes` and `on_oversize`"),
        "{violations:?}"
    );
}

#[test]
fn a_bounded_topic_with_on_oversize_is_refused() {
    let violations = violations_of(
        &camera_exposure(
            r#"topics: [
                {
                    member: "camera_status",
                    resource: "cam.status",
                    description: "Temperature and recording state.",
                    freshness: { max_age_ms: 5000 },
                    update: { max_hz: 1 },
                    max_result_bytes: 100,
                    on_oversize: "reject",
                },
            ]"#,
        ),
        &[&fixture(CAMERA_CONTRACT)],
    );
    assert_eq!(violations.len(), 1);
    assert!(
        violations[0].contains("`on_oversize` never applies"),
        "{violations:?}"
    );
}

#[test]
fn a_bounded_topic_over_its_size_limit_is_refused() {
    let violations = violations_of(
        &camera_exposure(
            r#"topics: [
                {
                    member: "camera_status",
                    resource: "cam.status",
                    description: "Temperature and recording state.",
                    freshness: { max_age_ms: 5000 },
                    update: { max_hz: 1 },
                    max_result_bytes: 10,
                },
            ]"#,
        ),
        &[&fixture(CAMERA_CONTRACT)],
    );
    assert_eq!(violations.len(), 1);
    assert!(
        violations[0].contains("exceeds `max_result_bytes` (10)"),
        "{violations:?}"
    );
}

#[test]
fn a_bounded_topic_within_its_size_limit_validates() {
    let bundle = build(
        &camera_exposure(
            r#"topics: [
                {
                    member: "camera_status",
                    resource: "cam.status",
                    description: "Temperature and recording state.",
                    freshness: { max_age_ms: 5000 },
                    update: { max_hz: 1 },
                    max_result_bytes: 100,
                },
            ]"#,
        ),
        &[&fixture(CAMERA_CONTRACT)],
    );
    assert_eq!(bundle.resources.len(), 1);
}

/// One frame topic serves a JPEG picture for a model and a lossless PNG for
/// a program, each its own resource, both bound to the one contract topic.
#[test]
fn two_resources_read_one_topic_with_different_representations() {
    use crate::ImageCodec;

    let frame_fields =
        r#"fields: { data: "frame", encoding: "encoding", width: "width", height: "height" }"#;
    let validated = validate(
        &camera_exposure(&format!(
            r#"topics: [
                {{
                    member: "video_stream",
                    resource: "cam.latest_frame",
                    description: "Latest frame, JPEG encoded.",
                    freshness: {{ max_age_ms: 2000 }},
                    update: {{ max_hz: 2 }},
                    representation: {{ image: "jpeg", quality: 80, {frame_fields} }},
                    max_result_bytes: 524288,
                    on_oversize: "downscale",
                }},
                {{
                    member: "video_stream",
                    resource: "cam.frame_png",
                    description: "Latest frame, losslessly encoded.",
                    freshness: {{ max_age_ms: 2000 }},
                    update: {{ max_hz: 2 }},
                    representation: {{ image: "png16", {frame_fields} }},
                    max_result_bytes: 524288,
                    on_oversize: "downscale",
                }},
            ]"#
        )),
        &[&fixture(CAMERA_CONTRACT)],
    );
    let published: Vec<(&str, &str, Option<ImageCodec>)> = validated
        .resources()
        .map(|(entry, bound)| {
            (
                entry.name.as_str(),
                bound.member.name.as_str(),
                entry.policies.representation.as_ref().map(|r| r.image),
            )
        })
        .collect();
    assert_eq!(
        published,
        [
            ("cam.latest_frame", "video_stream", Some(ImageCodec::Jpeg)),
            ("cam.frame_png", "video_stream", Some(ImageCodec::Png16)),
        ]
    );
}

/// A frame topic of the walkthrough camera under `representation`, the
/// topic's `representation` entry or nothing.
fn frame_topic(representation: &str) -> String {
    camera_exposure(&format!(
        r#"topics: [
            {{
                member: "video_stream",
                resource: "cam.latest_frame",
                description: "Latest frame.",
                freshness: {{ max_age_ms: 2000 }},
                update: {{ max_hz: 2 }},
                {representation}
                max_result_bytes: 524288,
                on_oversize: "reject",
            }},
        ]"#
    ))
}

#[test]
fn the_schema_of_a_resource_leaves_out_the_member_its_blob_carries() {
    let camera = fixture(CAMERA_CONTRACT);
    let frame_fields =
        r#"fields: { data: "frame", encoding: "encoding", width: "width", height: "height" }"#;
    for codec in ["jpeg", "png16", "raw"] {
        let bundle = build(
            &frame_topic(&format!(
                r#"representation: {{ image: "{codec}", {frame_fields} }},"#
            )),
            &[&camera],
        );
        let schema = &bundle.resources[0].schema;
        assert_eq!(
            schema["properties"]
                .as_object()
                .expect("object schema")
                .keys()
                .collect::<Vec<_>>(),
            ["header", "encoding", "width", "height"],
            "{codec}"
        );
        assert_eq!(
            schema["required"],
            serde_json::json!(["header", "encoding", "width", "height"]),
            "{codec}"
        );
    }

    let whole = build(&frame_topic(""), &[&camera]);
    assert_eq!(
        whole.resources[0].schema["properties"]["frame"]["contentEncoding"], "base64",
        "a resource with no representation serves the whole message as its document"
    );
    assert!(whole.pictures.is_empty());
}

#[test]
fn representation_fields_must_name_real_members_with_the_right_types() {
    let violations = violations_of(
        &camera_exposure(
            r#"topics: [
                {
                    member: "video_stream",
                    resource: "cam.latest_frame",
                    description: "Latest frame.",
                    freshness: { max_age_ms: 2000 },
                    update: { max_hz: 2 },
                    representation: {
                        image: "jpeg",
                        fields: {
                            data: "encoding",
                            encoding: "no_such_member",
                            width: "width",
                            height: "frame",
                        },
                    },
                    max_result_bytes: 524288,
                    on_oversize: "reject",
                },
            ]"#,
        ),
        &[&fixture(CAMERA_CONTRACT)],
    );
    assert_eq!(violations.len(), 3, "{violations:?}");
    assert!(
        violations[0].contains("`data` names `encoding`")
            && violations[0].contains("must be `bytes` or an array of `u8`"),
        "{violations:?}"
    );
    assert!(
        violations[1].contains("no root member `no_such_member`"),
        "{violations:?}"
    );
    assert!(
        violations[2].contains("`height` names `frame`")
            && violations[2].contains("`u8`, `u16`, or `u32`"),
        "{violations:?}"
    );
}

#[test]
fn restrict_violations_are_collected_per_field() {
    let violations = violations_of(
        &camera_exposure(
            r#"services: [
                {
                    member: "seek",
                    tool: "cam.seek",
                    description: "Seek the stream.",
                    operation: "mutating",
                    deadline_ms: 2000,
                    restrict: {
                        position: { min: 0 },
                        label: { max: 10 },
                        no_such_field: { min: 1 },
                    },
                },
            ]"#,
        ),
        &[&fixture(CAMERA_CONTRACT)],
    );
    assert_eq!(violations.len(), 3, "{violations:?}");
    assert!(
        violations[0].contains("decimal-string schema"),
        "{violations:?}"
    );
    assert!(
        violations[1].contains("`label` is `string`"),
        "{violations:?}"
    );
    assert!(
        violations[2].contains("names no root member"),
        "{violations:?}"
    );
    // A `min` above its `max` is not in the list: it needs no contract to
    // spot, so the document model refuses it at parse time. See
    // `rejects_a_restrict_entry_whose_min_exceeds_its_max` in `document`.
}

#[test]
fn restrict_bounds_on_integers_must_be_integers_in_range() {
    for (bounds, expected) in [
        ("{ min: -64.5 }", "must be an integer"),
        ("{ min: -3000000000 }", "outside `i32`'s range"),
    ] {
        let violations = violations_of(
            &camera_exposure(&format!(
                r#"services: [
                    {{
                        member: "set_brightness",
                        tool: "cam.set_brightness",
                        description: "Set brightness.",
                        operation: "mutating",
                        deadline_ms: 2000,
                        restrict: {{ value: {bounds} }},
                    }},
                ]"#
            )),
            &[&fixture(CAMERA_CONTRACT)],
        );
        assert_eq!(violations.len(), 1, "{bounds}: {violations:?}");
        assert!(violations[0].contains(expected), "{bounds}: {violations:?}");
    }
}

#[test]
fn a_float_restriction_is_reflected_into_the_schema() {
    let bundle = build(
        &camera_exposure(
            r#"services: [
                {
                    member: "seek",
                    tool: "cam.seek",
                    description: "Seek the stream.",
                    operation: "mutating",
                    deadline_ms: 2000,
                    restrict: { speed: { min: 0.5, max: 2.5 } },
                },
            ]"#,
        ),
        &[&fixture(CAMERA_CONTRACT)],
    );
    let speed = &bundle.tools[0].input_schema["properties"]["speed"];
    assert_eq!(speed["type"], serde_json::json!("number"));
    assert_eq!(speed["minimum"], serde_json::json!(0.5));
    assert_eq!(speed["maximum"], serde_json::json!(2.5));
}

#[test]
fn a_one_sided_integer_restriction_keeps_the_type_range_on_the_other_side() {
    let bundle = build(
        &camera_exposure(
            r#"services: [
                {
                    member: "set_brightness",
                    tool: "cam.set_brightness",
                    description: "Set brightness.",
                    operation: "mutating",
                    deadline_ms: 2000,
                    restrict: { value: { min: 0 } },
                },
            ]"#,
        ),
        &[&fixture(CAMERA_CONTRACT)],
    );
    let value = &bundle.tools[0].input_schema["properties"]["value"];
    assert_eq!(value["minimum"], serde_json::json!(0));
    assert_eq!(value["maximum"], serde_json::json!(i32::MAX));
}

#[test]
fn violations_across_targets_are_all_reported() {
    let exposure = format!(
        r#"{{
        peppy_schema: "mcp_exposure/v1",
        manifest: {{ name: "surface", tag: "v1" }},
        server: {{ title: "Surface" }},
        targets: {{
            front_camera: {{
                contract: {{ name: "rgb_camera", tag: "v1", sha256: "{camera_sha}" }},
                services: [
                    {{
                        member: "set_exposure",
                        tool: "cam.set_exposure",
                        description: "Absent from the contract.",
                        operation: "mutating",
                        deadline_ms: 2000,
                    }},
                ],
            }},
            recorder: {{
                contract: {{ name: "episode_recording", tag: "v1", sha256: "{recorder_sha}" }},
                actions: [
                    {{
                        member: "resume_session",
                        tool: "recorder.resume_session",
                        description: "Also absent.",
                        operation: "long_running",
                        deadline_ms: 60000,
                    }},
                ],
            }},
        }},
    }}"#,
        camera_sha = sha_of(CAMERA_CONTRACT),
        recorder_sha = sha_of(RECORDING_CONTRACT),
    );
    let violations = violations_of(
        &exposure,
        &[&fixture(CAMERA_CONTRACT), &fixture(RECORDING_CONTRACT)],
    );
    assert_eq!(violations.len(), 2, "{violations:?}");
    assert!(violations[0].contains("set_exposure"), "{violations:?}");
    assert!(violations[1].contains("resume_session"), "{violations:?}");
}

/// One-action exposure of the recorder pinned to the `contract` source:
/// `member` published as a tool whose goal is bounded by the `bound` field.
fn recorder_exposure(contract: &str, member: &str, bound: &str) -> String {
    format!(
        r#"{{
        peppy_schema: "mcp_exposure/v1",
        manifest: {{ name: "surface", tag: "v1" }},
        server: {{ title: "Surface" }},
        targets: {{
            recorder: {{
                contract: {{ name: "episode_recording", tag: "v1", sha256: "{recorder_sha}" }},
                actions: [
                    {{
                        member: "{member}",
                        tool: "recorder.{member}",
                        description: "Drive the recorder.",
                        operation: "long_running",
                        {bound},
                    }},
                ],
            }},
        }},
    }}"#,
        recorder_sha = sha_of(contract),
    )
}

#[test]
fn an_action_without_optional_endpoints_gets_empty_schemas_and_no_feedback() {
    let bundle = build(
        &recorder_exposure(RECORDING_CONTRACT, "finish_session", "deadline_ms: 60000"),
        &[&fixture(RECORDING_CONTRACT)],
    );
    let task = &bundle.tasks[0];
    assert_eq!(task.input_schema, empty_object_schema());
    assert_eq!(task.feedback_schema, None);
    assert_eq!(
        task.output_schema["properties"]["success"],
        serde_json::json!({"type": "boolean"})
    );
}

#[test]
fn a_progress_bound_action_needs_a_feedback_topic() {
    let violations = violations_of(
        &recorder_exposure(
            RECORDING_CONTRACT,
            "finish_session",
            "progress_timeout_ms: 60000",
        ),
        &[&fixture(RECORDING_CONTRACT)],
    );
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert!(
        violations[0].contains("target `recorder` action `finish_session`: `progress_timeout_ms`")
            && violations[0].contains("declares no `feedback_topic`"),
        "{violations:?}"
    );
}

#[test]
fn a_progress_bound_action_without_feedback_has_its_other_violations_reported_too() {
    // `finish_session` declares no feedback topic, and its result now
    // carries a reserved field name: two independent violations.
    let contract = RECORDING_CONTRACT.replace(
        r#"response_message_format: { success: "bool" },"#,
        r#"response_message_format: { instance_id: "string" },"#,
    );
    let violations = violations_of(
        &recorder_exposure(&contract, "finish_session", "progress_timeout_ms: 60000"),
        &[&fixture(&contract)],
    );
    assert_eq!(violations.len(), 2, "{violations:?}");
    assert!(
        violations[0].contains("declares no `feedback_topic`"),
        "{violations:?}"
    );
    assert!(
        violations[1].contains("target `recorder` action `finish_session` result")
            && violations[1].contains("instance_id"),
        "{violations:?}"
    );
}

#[test]
fn a_progress_bound_action_publishes_its_window_in_the_catalog() {
    let bundle = build(
        &recorder_exposure(
            RECORDING_CONTRACT,
            "record_episode",
            "progress_timeout_ms: 60000",
        ),
        &[&fixture(RECORDING_CONTRACT)],
    );
    assert_eq!(
        bundle.tasks[0].bound,
        GoalBound::Progress {
            window_ms: std::num::NonZeroU64::new(60000).expect("nonzero")
        }
    );
    let task = serde_json::to_value(&bundle.tasks[0]).expect("serializes");
    assert_eq!(task["progress_timeout_ms"], 60000, "{task}");
    assert!(task.get("deadline_ms").is_none(), "{task}");
}

#[test]
fn the_validation_error_renders_one_bullet_per_violation() {
    let error = ExposureValidationError {
        violations: vec!["first problem".to_string(), "second problem".to_string()],
    };
    assert_eq!(
        error.to_string(),
        "the exposure does not validate against its contracts and daemon interfaces:\n  - first \
         problem\n  - second problem"
    );
}

const STATUS_CONTRACT: &str = r#"{
    peppy_schema: "contract/v1",
    manifest: { name: "robot_status", tag: "v1" },
    interfaces: {
        topics: [
            {
                name: "status",
                qos_profile: "sensor_data",
                message_format: { battery: "u8", mode: "string" },
            },
        ],
        services: [
            {
                name: "get_identity",
                response_message_format: { robot: "string", model: "string" },
            },
            {
                name: "rename",
                request_message_format: { robot: "string" },
                response_message_format: { applied: "bool" },
            },
        ],
    },
}"#;

/// A per-robot surface over the status contract and the walkthrough camera.
fn per_robot_exposure(status_sha: &str, extra_status_services: &str) -> String {
    format!(
        r#"{{
        peppy_schema: "mcp_exposure/v1",
        manifest: {{ name: "robot_control", tag: "v1" }},
        server: {{ title: "Robots" }},
        robots: {{
            list: {{ tool: "robot.list", description: "The robots of the stack." }},
            describe: {{
                identity: {{ target: "status", service: "get_identity" }},
            }},
        }},
        targets: {{
            status: {{
                contract: {{ name: "robot_status", tag: "v1", sha256: "{status_sha}" }},
                topics: [
                    {{
                        member: "status",
                        resource: "robot.status",
                        description: "The robot's latest status.",
                        freshness: {{ max_age_ms: 2000 }},
                        update: {{ max_hz: 2 }},
                        max_result_bytes: 4096,
                        on_oversize: "reject",
                    }},
                ],
                services: [
                    {{
                        member: "get_identity",
                        tool: "robot.get_identity",
                        description: "Who the robot is.",
                        operation: "read_only",
                        deadline_ms: 2000,
                    }},
                    {extra_status_services}
                ],
            }},
            camera: {{
                contract: {{ name: "rgb_camera", tag: "v1", sha256: "{}" }},
                argument: "camera",
                services: [
                    {{
                        member: "set_brightness",
                        tool: "camera.set_brightness",
                        description: "Set the camera's brightness.",
                        operation: "mutating",
                        deadline_ms: 2000,
                    }},
                ],
            }},
        }},
    }}"#,
        sha_of(CAMERA_CONTRACT)
    )
}

#[test]
fn a_per_robot_bundle_adds_the_routing_arguments_and_resolves_its_listing() {
    let status = fixture(STATUS_CONTRACT);
    let camera = fixture(CAMERA_CONTRACT);
    let bundle = build(
        &per_robot_exposure(&sha_of(STATUS_CONTRACT), ""),
        &[&status, &camera],
    );

    let BundleSurface::PerRobot { robots, contracts } = &bundle.surface else {
        panic!("expected a per-robot bundle");
    };
    assert_eq!(robots.list.name, "robot.list");
    assert_eq!(
        robots.describe,
        vec![DescribeEntry {
            key: "identity".to_string(),
            target: "status".to_string(),
            tool: "robot.get_identity".to_string(),
        }]
    );
    let by_slot: BTreeMap<&str, Option<&str>> = contracts
        .iter()
        .map(|pin| (pin.pin.link_id.as_str(), pin.argument.as_deref()))
        .collect();
    assert_eq!(by_slot["status"], None);
    assert_eq!(by_slot["camera"], Some("camera"));

    let identity = &bundle.tools[0];
    assert_eq!(identity.name, "robot.get_identity");
    assert_eq!(
        identity.input_schema["required"],
        serde_json::json!(["robot"])
    );
    assert_eq!(
        identity.input_schema["properties"]["robot"],
        serde_json::json!({ "type": "string" })
    );
    let brightness = &bundle.tools[1];
    assert_eq!(brightness.name, "camera.set_brightness");
    assert_eq!(
        brightness.input_schema["required"],
        serde_json::json!(["value", "robot", "camera"])
    );
    assert_eq!(
        brightness.input_schema["additionalProperties"],
        serde_json::json!(false)
    );

    let reparsed = ExposureBundle::from_json_str(&bundle.to_json_string()).expect("round trips");
    assert_eq!(reparsed, bundle);
}

#[test]
fn a_picture_tool_of_a_per_robot_surface_takes_the_routing_arguments_alone() {
    let status = fixture(STATUS_CONTRACT);
    let camera = fixture(CAMERA_CONTRACT);
    let frame_topic = r#"argument: "camera",
        topics: [
            {
                member: "video_stream",
                resource: "camera.latest_frame",
                description: "The camera's latest frame.",
                freshness: { max_age_ms: 2000 },
                update: { max_hz: 2 },
                representation: {
                    image: "jpeg",
                    fields: { data: "frame", encoding: "encoding", width: "width", height: "height" },
                },
                max_result_bytes: 524288,
                on_oversize: "downscale",
                picture: { tool: "camera.look", description: "Look through the camera." },
            },
        ],"#;
    let bundle = build(
        &per_robot_exposure(&sha_of(STATUS_CONTRACT), "")
            .replace(r#"argument: "camera","#, frame_topic),
        &[&status, &camera],
    );

    assert_eq!(bundle.pictures.len(), 1);
    let look = &bundle.pictures[0];
    assert_eq!(look.name, "camera.look");
    assert_eq!(look.target, "camera");
    assert_eq!(look.resource, "camera.latest_frame");
    assert_eq!(
        look.input_schema,
        serde_json::json!({
            "type": "object",
            "properties": {
                "robot": { "type": "string" },
                "camera": { "type": "string" },
            },
            "required": ["robot", "camera"],
            "additionalProperties": false,
        })
    );
    assert_eq!(look.output_schema, bundle.resources[1].schema);
    assert!(look.output_schema["properties"].get("frame").is_none());

    let reparsed = ExposureBundle::from_json_str(&bundle.to_json_string()).expect("round trips");
    assert_eq!(reparsed, bundle);
}

#[test]
fn a_request_field_named_like_a_routing_argument_is_refused() {
    let status = fixture(STATUS_CONTRACT);
    let camera = fixture(CAMERA_CONTRACT);
    let rename = r#"{
        member: "rename",
        tool: "robot.rename",
        description: "Rename the robot.",
        operation: "mutating",
        deadline_ms: 2000,
    },"#;
    let violations = violations_of(
        &per_robot_exposure(&sha_of(STATUS_CONTRACT), rename),
        &[&status, &camera],
    );
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert!(
        violations[0].contains("service `rename`")
            && violations[0].contains("declares `robot`, the name the server adds"),
        "{violations:?}"
    );
}

/// The listing calls a described service with the robot alone, so one
/// taking a request of its own cannot fill a listing field.
#[test]
fn a_described_service_takes_no_request_of_its_own() {
    // The contract's `rename` takes a request the routing argument does not
    // collide with, so the listing rule is the only one it breaks.
    let contract = STATUS_CONTRACT.replace(
        r#"request_message_format: { robot: "string" }"#,
        r#"request_message_format: { new_name: "string" }"#,
    );
    let rename = r#"{
        member: "rename",
        tool: "robot.rename",
        description: "Rename the robot.",
        operation: "mutating",
        deadline_ms: 2000,
    },"#;
    let violations = violations_of(
        &per_robot_exposure(&sha_of(&contract), rename)
            .replace(r#"service: "get_identity""#, r#"service: "rename""#),
        &[&fixture(&contract), &fixture(CAMERA_CONTRACT)],
    );
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert!(
        violations[0].contains("`robots.describe.identity`")
            && violations[0].contains("takes `new_name`")
            && violations[0].contains("takes no request"),
        "{violations:?}"
    );
}

#[test]
fn a_fixed_bundle_carries_no_robot_surface() {
    let bundle = build(
        WALKTHROUGH_EXPOSURE,
        &[&fixture(CAMERA_CONTRACT), &fixture(RECORDING_CONTRACT)],
    );
    assert!(
        matches!(bundle.surface, BundleSurface::Fixed { .. }),
        "a document without `robots` derives a fixed surface"
    );
    assert!(
        !bundle.to_json_string().contains("\"robots\""),
        "an absent surface is not written"
    );
}

/// A contract with one service that answers with a picture, as a
/// simulation's free viewpoint does.
const VIEW_CONTRACT: &str = r#"{
    peppy_schema: "contract/v1",
    manifest: { name: "scene_view", tag: "v1" },
    interfaces: {
        services: [
            {
                name: "render_view",
                request_message_format: {
                    position: { $type: "array", $items: "f64" },
                    target: { $type: "array", $items: "f64" },
                },
                response_message_format: {
                    success: "bool",
                    message: "string",
                    encoding: "string",
                    width: "u32",
                    height: "u32",
                    frame: { $type: "array", $items: "u8" },
                },
            },
        ],
    },
}"#;

/// An exposure of the view contract: its service as `scene.look` under
/// `policies`, and a record of the endpoint's calls.
fn view_exposure(policies: &str) -> String {
    format!(
        r#"{{
        peppy_schema: "mcp_exposure/v1",
        manifest: {{ name: "simulation", tag: "v1" }},
        server: {{ title: "Simulation" }},
        call_record: {{
            tool: "scene.recent_calls",
            description: "The last state-changing calls of this endpoint.",
            keep: 200,
        }},
        targets: {{
            view: {{
                contract: {{ name: "scene_view", tag: "v1", sha256: "{view_sha}" }},
                services: [
                    {{
                        member: "render_view",
                        tool: "scene.look",
                        description: "Look at the world from a free viewpoint.",
                        operation: "read_only",
                        deadline_ms: 10000,
                        {policies}
                    }},
                ],
            }},
        }},
    }}"#,
        view_sha = sha_of(VIEW_CONTRACT),
    )
}

#[test]
fn a_pictured_service_publishes_the_schema_of_its_document_and_the_bundle_carries_the_record() {
    let frame_fields =
        r#"fields: { data: "frame", encoding: "encoding", width: "width", height: "height" }"#;
    let bundle = build(
        &view_exposure(&format!(
            r#"representation: {{ image: "jpeg", quality: 80, {frame_fields} }},
               max_result_bytes: 524288,
               on_oversize: "downscale","#
        )),
        &[&fixture(VIEW_CONTRACT)],
    );
    let look = &bundle.tools[0];
    assert_eq!(look.name, "scene.look");
    assert_eq!(
        look.representation.as_ref().map(|r| r.image),
        Some(ImageCodec::Jpeg)
    );
    assert_eq!(look.max_result_bytes.map(NonZeroU64::get), Some(524288));
    assert_eq!(look.on_oversize, Some(OversizePolicy::Downscale));
    assert_eq!(
        look.output_schema["properties"]
            .as_object()
            .expect("object schema")
            .keys()
            .collect::<Vec<_>>(),
        ["success", "message", "encoding", "width", "height"],
        "the document leaves out the member the picture carries"
    );
    assert_eq!(
        look.output_schema["required"],
        serde_json::json!(["success", "message", "encoding", "width", "height"])
    );
    assert_eq!(
        look.input_schema["properties"]
            .as_object()
            .expect("object schema")
            .keys()
            .collect::<Vec<_>>(),
        ["position", "target"]
    );
    assert_eq!(
        bundle.call_record,
        Some(CallRecordEntry {
            name: "scene.recent_calls".to_string(),
            description: "The last state-changing calls of this endpoint.".to_string(),
            keep: 200,
        })
    );

    // Without a representation the whole response is the document, and the
    // record round-trips through the bundle's JSON.
    let plain = build(&view_exposure(""), &[&fixture(VIEW_CONTRACT)]);
    assert_eq!(
        plain.tools[0].output_schema["properties"]["frame"]["contentEncoding"],
        "base64"
    );
    assert!(plain.tools[0].representation.is_none());
    let reparsed =
        ExposureBundle::from_json_str(&plain.to_json_string()).expect("the bundle reparses");
    assert_eq!(reparsed.call_record, plain.call_record);
}

#[test]
fn a_pictured_service_needs_its_representation_fields_in_the_response() {
    let violations = violations_of(
        &view_exposure(
            r#"representation: {
                image: "jpeg",
                fields: { data: "picture", encoding: "encoding", width: "width", height: "message" },
            },"#,
        ),
        &[&fixture(VIEW_CONTRACT)],
    );
    assert_eq!(violations.len(), 2, "{violations:?}");
    assert!(
        violations[0].contains("target `view` service `render_view`")
            && violations[0].contains("no root member `picture`"),
        "{violations:?}"
    );
    assert!(
        violations[1].contains("`height` names `message`")
            && violations[1].contains("`u8`, `u16`, or `u32`"),
        "{violations:?}"
    );
}

/// `recorder_exposure` with its target drawing from the daemon interface of
/// the recording fixture's identity instead of the contract.
fn daemon_recorder_exposure(member: &str, bound: &str) -> String {
    let contract_reference = format!(
        r#"contract: {{ name: "episode_recording", tag: "v1", sha256: "{}" }}"#,
        sha_of(RECORDING_CONTRACT)
    );
    recorder_exposure(RECORDING_CONTRACT, member, bound).replace(
        &contract_reference,
        r#"daemon: { name: "episode_recording", tag: "v1" }"#,
    )
}

fn validate_daemon(exposure_json5: &str, interface: &Fixture) -> ValidatedExposure {
    build_exposure_bundle(
        &parse_exposure(exposure_json5),
        &[],
        &[interface.interface()],
    )
    .expect("exposure validates")
}

#[test]
fn a_daemon_target_derives_its_entries_as_a_contract_target_does() {
    let recording = fixture(RECORDING_CONTRACT);
    let from_contract = build(
        &recorder_exposure(
            RECORDING_CONTRACT,
            "record_episode",
            "progress_timeout_ms: 60000",
        ),
        &[&recording],
    );
    let validated = validate_daemon(
        &daemon_recorder_exposure("record_episode", "progress_timeout_ms: 60000"),
        &recording,
    );
    assert_eq!(
        validated.bundle.tasks, from_contract.tasks,
        "one derivation: the same members give the same entries"
    );

    // The bundle marks the target as a daemon target, apart from the slots.
    assert!(validated.bundle.surface.contracts().is_empty());
    assert_eq!(
        validated.bundle.surface.daemon_targets(),
        [BundleDaemonTarget {
            target: "recorder".to_string(),
            name: "episode_recording".to_string(),
            tag: "v1".to_string(),
        }]
    );
    let (_, bound) = validated.tasks().next().expect("one task");
    assert_eq!(
        bound.source,
        MemberSource::Daemon(validated.bundle.surface.daemon_targets()[0].clone())
    );
    assert_eq!(bound.member.name, "record_episode");
}

#[test]
fn a_daemon_target_follows_the_member_rules_of_a_contract_target() {
    let recording = fixture(RECORDING_CONTRACT);
    let violations = build_exposure_bundle(
        &parse_exposure(&daemon_recorder_exposure(
            "no_such_action",
            "deadline_ms: 60000",
        )),
        &[],
        &[recording.interface()],
    )
    .expect_err("the interface declares no such action")
    .violations;
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert!(
        violations[0].contains(
            "target `recorder` selects action member `no_such_action`, but daemon interface \
             `episode_recording:v1` declares no such action"
        ),
        "{violations:?}"
    );

    let violations = build_exposure_bundle(
        &parse_exposure(&daemon_recorder_exposure(
            "finish_session",
            "progress_timeout_ms: 60000",
        )),
        &[],
        &[recording.interface()],
    )
    .expect_err("a progress window needs a feedback topic")
    .violations;
    assert!(
        violations[0]
            .contains("daemon interface `episode_recording:v1` declares no `feedback_topic`"),
        "{violations:?}"
    );
}

#[test]
fn a_daemon_target_needs_its_interface() {
    let violations = build_exposure_bundle(
        &parse_exposure(&daemon_recorder_exposure(
            "record_episode",
            "deadline_ms: 60000",
        )),
        &[fixture(RECORDING_CONTRACT).resolved()],
        &[],
    )
    .expect_err("a contract of the same identity is not the interface")
    .violations;
    assert_eq!(
        violations,
        [
            "target `recorder` references daemon interface `episode_recording:v1`, which was not \
          provided"
        ]
    );
}

#[test]
fn a_daemon_target_on_a_per_robot_surface_is_a_violation() {
    // The document refuses the pair when it parses; a value built in code
    // meets the same rule when it validates.
    let mut exposure = parse_exposure(&per_robot_exposure(&sha_of(STATUS_CONTRACT), ""));
    let ExposureSurface::PerRobot { targets, .. } = &mut exposure.surface else {
        panic!("a per-robot surface");
    };
    targets["status"].selection.source = TargetSource::Daemon(crate::DaemonInterfaceRef {
        name: peppy_config_model::runtime::Name::new("robot_status").expect("a name"),
        tag: "v1".to_string(),
    });
    let status = fixture(STATUS_CONTRACT);
    let violations = build_exposure_bundle(
        &exposure,
        &[fixture(CAMERA_CONTRACT).resolved()],
        &[status.interface()],
    )
    .expect_err("a daemon target sits on a fixed surface")
    .violations;
    assert!(
        violations[0].contains(
            "target `status` names daemon interface `robot_status:v1` on a per-robot surface"
        ),
        "{violations:?}"
    );
}
