use super::*;
use daemon_config::contract::PeppyContractParser;
use daemon_config::mcp_exposure::PeppyMcpExposureParser;
use std::path::Path;

/// Camera contract fixture: an unbounded image topic, a bounded telemetry
/// topic, and services covering every restrict shape (i32, u64, string,
/// f32).
const CAMERA_CONTRACT: &str = r#"{
    peppy_schema: "contract/v1",
    manifest: { name: "rgb_camera", tag: "v1" },
    interfaces: {
        topics: [
            {
                name: "video_stream",
                qos_profile: "sensor_data",
                message_format: {
                    header: { $type: "object", stamp: "time", frame_id: "u32" },
                    encoding: "string",
                    width: "u32",
                    height: "u32",
                    frame: { $type: "array", $items: "u8" },
                },
            },
            {
                name: "camera_status",
                message_format: {
                    temperature_c: "i16",
                    recording: "bool",
                },
            },
        ],
        services: [
            {
                name: "video_stream_info",
                response_message_format: {
                    width: "u32",
                    height: "u32",
                    frames_per_second: "u8",
                    encoding: "string",
                },
            },
            {
                name: "set_brightness",
                request_message_format: { value: "i32" },
                response_message_format: {
                    success: "bool",
                    message: "string",
                    current_value: "i32",
                },
            },
            {
                name: "seek",
                request_message_format: {
                    position: "u64",
                    label: "string",
                    speed: "f32",
                },
                response_message_format: { success: "bool" },
            },
        ],
    },
}"#;

/// Recorder contract fixture: one full action (goal payloads, feedback,
/// result) and one minimal action that stays unselected.
const RECORDING_CONTRACT: &str = r#"{
    peppy_schema: "contract/v1",
    manifest: { name: "episode_recording", tag: "v1" },
    interfaces: {
        actions: [
            {
                name: "record_episode",
                goal_service: {
                    request_message_format: {
                        task_name: "string",
                        episode_index: "u32",
                    },
                    response_message_format: { accepted: "bool" },
                },
                feedback_topic: {
                    message_format: { frames_recorded: "u64" },
                },
                result_service: {
                    response_message_format: {
                        success: "bool",
                        frames_recorded: "u64",
                    },
                },
            },
            {
                name: "finish_session",
                result_service: {
                    response_message_format: { success: "bool" },
                },
            },
        ],
    },
}"#;

fn resolved(contract_json5: &str) -> ResolvedContractDocument {
    ResolvedContractDocument {
        sha256: ManifestFingerprint::of_bytes(contract_json5.as_bytes()),
        document: PeppyContractParser::from_content(contract_json5).expect("fixture parses"),
    }
}

fn sha_of(contract_json5: &str) -> String {
    ManifestFingerprint::of_bytes(contract_json5.as_bytes()).to_string()
}

/// The design walkthrough surface against the fixture contracts, with the
/// pins computed from the fixture bytes.
fn walkthrough_exposure() -> String {
    format!(
        r#"{{
        peppy_schema: "mcp_exposure/v1",
        manifest: {{ name: "camera_and_recording", tag: "v1" }},
        server: {{
            title: "OpenArm camera and recording",
            instructions: "Observe the front camera and record teleoperation episodes on this robot.",
        }},
        targets: {{
            front_camera: {{
                contract: {{ name: "rgb_camera", tag: "v1", sha256: "{camera_sha}" }},
                topics: [
                    {{
                        member: "video_stream",
                        resource: "front_camera.latest_frame",
                        description: "Latest frame from the front-facing camera, JPEG encoded.",
                        freshness: {{ max_age_ms: 2000 }},
                        update: {{ max_hz: 2 }},
                        representation: {{
                            image: "jpeg",
                            quality: 80,
                            fields: {{
                                data: "frame",
                                encoding: "encoding",
                                width: "width",
                                height: "height",
                            }},
                        }},
                        max_result_bytes: 524288,
                        on_oversize: "downscale",
                    }},
                ],
                services: [
                    {{
                        member: "video_stream_info",
                        tool: "front_camera.info",
                        description: "Report the camera's resolution, frame rate, and encoding.",
                        operation: "read_only",
                        deadline_ms: 2000,
                    }},
                    {{
                        member: "set_brightness",
                        tool: "front_camera.set_brightness",
                        description: "Set the camera brightness in device units.",
                        operation: "mutating",
                        deadline_ms: 2000,
                        restrict: {{ value: {{ min: -64, max: 64 }} }},
                    }},
                ],
            }},
            recorder: {{
                contract: {{ name: "episode_recording", tag: "v1", sha256: "{recorder_sha}" }},
                actions: [
                    {{
                        member: "record_episode",
                        tool: "recorder.record_episode",
                        description: "Record one teleoperation episode to the local dataset. Long-running; returns a task handle.",
                        operation: "long_running",
                        safety_sensitive: true,
                        confirmation_required: true,
                        deadline_ms: 900000,
                    }},
                ],
            }},
        }},
    }}"#,
        camera_sha = sha_of(CAMERA_CONTRACT),
        recorder_sha = sha_of(RECORDING_CONTRACT),
    )
}

fn build(exposure_json5: &str, contracts: &[ResolvedContractDocument]) -> ExposureBundle {
    let exposure = PeppyMcpExposureParser::from_content(exposure_json5).expect("exposure parses");
    build_exposure_bundle(&exposure, contracts).expect("exposure validates")
}

fn violations_of(exposure_json5: &str, contracts: &[ResolvedContractDocument]) -> Vec<String> {
    let exposure = PeppyMcpExposureParser::from_content(exposure_json5).expect("exposure parses");
    build_exposure_bundle(&exposure, contracts)
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
fn the_walkthrough_exposure_builds_its_bundle() {
    let bundle = build(
        &walkthrough_exposure(),
        &[resolved(CAMERA_CONTRACT), resolved(RECORDING_CONTRACT)],
    );

    assert_eq!(bundle.bundle_format, EXPOSURE_BUNDLE_FORMAT);
    assert_eq!(bundle.schema_mapping_version, SCHEMA_MAPPING_VERSION);
    assert_eq!(bundle.exposure.name, "camera_and_recording");
    assert_eq!(bundle.node.name, "camera_and_recording_mcp");
    assert_eq!(bundle.node.tag, "v1");
    let links: Vec<(&str, &str)> = bundle
        .node
        .contracts
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
    assert_eq!(bundle.node.contracts[0].sha256, sha_of(CAMERA_CONTRACT));

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
        ["header", "encoding", "width", "height", "frame"],
        "schema properties keep the format's declaration order"
    );
    assert_eq!(
        frame.schema["properties"]["frame"]["contentEncoding"],
        "base64"
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
/// names, prose, policies, derived schemas, and pin fingerprints.
/// Regenerate with `UPDATE_EXPOSURE_GOLDENS=1 cargo test -p generator --lib
/// exposure` and review the diff before committing.
#[test]
fn bundle_golden_matches_committed_output() {
    let bundle = build(
        &walkthrough_exposure(),
        &[resolved(CAMERA_CONTRACT), resolved(RECORDING_CONTRACT)],
    );
    let rendered = bundle.to_json_string();
    if std::env::var_os("UPDATE_EXPOSURE_GOLDENS").is_some() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/generator/exposure/validate/goldens/camera_and_recording.bundle.json");
        std::fs::write(&path, &rendered).expect("write golden");
        return;
    }
    assert_eq!(
        rendered,
        include_str!("goldens/camera_and_recording.bundle.json"),
        "the bundle drifted from its golden; run `UPDATE_EXPOSURE_GOLDENS=1 cargo test -p \
         generator --lib exposure` and review the diff"
    );
}

#[test]
fn a_missing_contract_is_reported() {
    let violations = violations_of(&camera_exposure(INFO_TOOL), &[resolved(RECORDING_CONTRACT)]);
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
    let violations = violations_of(&exposure, &[resolved(CAMERA_CONTRACT)]);
    assert_eq!(violations.len(), 1);
    assert!(violations[0].contains("fingerprint to"), "{violations:?}");
    assert!(
        violations[0].contains(&sha_of(CAMERA_CONTRACT)),
        "{violations:?}"
    );
}

#[test]
fn a_contract_provided_twice_is_reported() {
    let violations = violations_of(
        &camera_exposure(INFO_TOOL).replace(&sha_of(CAMERA_CONTRACT), &sha_of("{ }")),
        &[resolved(CAMERA_CONTRACT), resolved(CAMERA_CONTRACT)],
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
        &[resolved(CAMERA_CONTRACT)],
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
        &[resolved(CAMERA_CONTRACT)],
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
        &[resolved(CAMERA_CONTRACT)],
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
        &[resolved(CAMERA_CONTRACT)],
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
        &[resolved(CAMERA_CONTRACT)],
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
        &[resolved(CAMERA_CONTRACT)],
    );
    assert_eq!(bundle.resources.len(), 1);
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
        &[resolved(CAMERA_CONTRACT)],
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
        &[resolved(CAMERA_CONTRACT)],
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
    // `rejects_a_restrict_entry_whose_min_exceeds_its_max` in daemon-config.
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
            &[resolved(CAMERA_CONTRACT)],
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
        &[resolved(CAMERA_CONTRACT)],
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
        &[resolved(CAMERA_CONTRACT)],
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
        &[resolved(CAMERA_CONTRACT), resolved(RECORDING_CONTRACT)],
    );
    assert_eq!(violations.len(), 2, "{violations:?}");
    assert!(violations[0].contains("set_exposure"), "{violations:?}");
    assert!(violations[1].contains("resume_session"), "{violations:?}");
}

#[test]
fn an_action_without_optional_endpoints_gets_empty_schemas_and_no_feedback() {
    let recorder_sha = sha_of(RECORDING_CONTRACT);
    let exposure = format!(
        r#"{{
        peppy_schema: "mcp_exposure/v1",
        manifest: {{ name: "surface", tag: "v1" }},
        server: {{ title: "Surface" }},
        targets: {{
            recorder: {{
                contract: {{ name: "episode_recording", tag: "v1", sha256: "{recorder_sha}" }},
                actions: [
                    {{
                        member: "finish_session",
                        tool: "recorder.finish_session",
                        description: "Finalize the open dataset session.",
                        operation: "long_running",
                        deadline_ms: 60000,
                    }},
                ],
            }},
        }},
    }}"#
    );
    let bundle = build(&exposure, &[resolved(RECORDING_CONTRACT)]);
    let task = &bundle.tasks[0];
    assert_eq!(task.input_schema, empty_object_schema());
    assert_eq!(task.feedback_schema, None);
    assert_eq!(
        task.output_schema["properties"]["success"],
        serde_json::json!({"type": "boolean"})
    );
}

#[test]
fn the_validation_error_renders_one_bullet_per_violation() {
    let error = ExposureValidationError {
        violations: vec!["first problem".to_string(), "second problem".to_string()],
    };
    assert_eq!(
        error.to_string(),
        "the exposure does not validate against its contracts:\n  - first problem\n  - second problem"
    );
}
