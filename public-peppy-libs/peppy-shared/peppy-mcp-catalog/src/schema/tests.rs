use super::*;
use peppy_config_model::node::{FormatRuleViolation, MessageFormat};
use serde_json::json;
use std::path::Path;

/// Golden pairs: the format under `goldens/` and the committed schema the
/// canonical mapping must produce for it. Regenerate with
/// `UPDATE_CATALOG_GOLDENS=1 cargo test -p peppy-mcp-catalog` and review the
/// diff before committing.
const GOLDENS: &[(&str, &str, &str)] = &[
    (
        "all_primitives",
        include_str!("goldens/all_primitives.format.json5"),
        include_str!("goldens/all_primitives.schema.json"),
    ),
    (
        "optionals",
        include_str!("goldens/optionals.format.json5"),
        include_str!("goldens/optionals.schema.json"),
    ),
    (
        "nested_object",
        include_str!("goldens/nested_object.format.json5"),
        include_str!("goldens/nested_object.schema.json"),
    ),
    (
        "arrays",
        include_str!("goldens/arrays.format.json5"),
        include_str!("goldens/arrays.schema.json"),
    ),
];

fn parse_format(json5: &str) -> MessageFormat {
    serde_json5::from_str(json5).expect("golden format parses")
}

fn rendered_schema(format_json5: &str) -> String {
    let format = parse_format(format_json5);
    let schema = message_format_to_json_schema(&format).expect("golden format maps");
    let pretty = serde_json::to_string_pretty(&schema).expect("schema serializes");
    format!("{pretty}\n")
}

#[test]
fn golden_schemas_match_committed_output() {
    let update = std::env::var_os("UPDATE_CATALOG_GOLDENS").is_some();
    for (name, format_json5, committed) in GOLDENS {
        let rendered = rendered_schema(format_json5);
        if update {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("src/schema/goldens/{name}.schema.json"));
            std::fs::write(&path, &rendered).expect("write golden");
            continue;
        }
        assert_eq!(
            &rendered, committed,
            "schema for `{name}` drifted from its golden; run \
             `UPDATE_CATALOG_GOLDENS=1 cargo test -p peppy-mcp-catalog` and review the diff"
        );
    }
}

fn compiled_validator(format_json5: &str) -> jsonschema::Validator {
    let schema: serde_json::Value =
        serde_json::from_str(&rendered_schema(format_json5)).expect("schema parses back");
    jsonschema::validator_for(&schema).expect("the derived schema is a valid JSON Schema")
}

fn golden_format(name: &str) -> &'static str {
    GOLDENS
        .iter()
        .find(|(golden, _, _)| *golden == name)
        .map(|(_, format, _)| *format)
        .expect("known golden")
}

/// Value-level cases: for each golden, instances the derived schema must
/// accept and instances it must reject, pinning the mapping's promises
/// (decimal strings, base64 lengths, integer ranges, required sets, closed
/// objects) at the JSON level.
#[test]
fn golden_schemas_judge_canonical_instances() {
    let all_primitives = compiled_validator(golden_format("all_primitives"));
    let full = json!({
        "flag": true,
        "label": "front",
        "blob": "aGVsbG8=",
        "stamp": "2026-08-11T12:34:56.123456789Z",
        "tiny": 255,
        "small": 65535,
        "medium": 4294967295u64,
        "big": "18446744073709551615",
        "tiny_signed": -128,
        "small_signed": -32768,
        "medium_signed": -2147483648i64,
        "big_signed": "-9223372036854775808",
        "ratio": 1.5,
        "precise": -2.5,
    });
    assert!(
        all_primitives.is_valid(&full),
        "the canonical full instance validates"
    );
    for (case, mutate) in [
        ("u64 as a JSON number", json!(18446744073709551615u64)),
        ("u64 with leading zeros", json!("007")),
        ("u64 negative", json!("-1")),
    ] {
        let mut instance = full.clone();
        instance["big"] = mutate;
        assert!(
            !all_primitives.is_valid(&instance),
            "{case} must be rejected"
        );
    }
    let mut negative_zero = full.clone();
    negative_zero["big_signed"] = json!("-0");
    assert!(
        !all_primitives.is_valid(&negative_zero),
        "-0 is not canonical decimal"
    );
    let mut out_of_range = full.clone();
    out_of_range["tiny"] = json!(256);
    assert!(
        !all_primitives.is_valid(&out_of_range),
        "u8 range is enforced"
    );
    let mut wide = full.clone();
    wide["medium_signed"] = json!(2147483648i64);
    assert!(!all_primitives.is_valid(&wide), "i32 range is enforced");
    let mut missing = full.as_object().expect("object").clone();
    missing.remove("flag");
    assert!(
        !all_primitives.is_valid(&serde_json::Value::Object(missing)),
        "every non-optional field is required"
    );
    let mut extra = full.clone();
    extra["surprise"] = json!(1);
    assert!(!all_primitives.is_valid(&extra), "objects are closed");

    let optionals = compiled_validator(golden_format("optionals"));
    assert!(
        optionals.is_valid(&json!({"required_label": "x"})),
        "optional fields may be omitted"
    );
    assert!(
        optionals.is_valid(&json!({
            "required_label": "x",
            "optional_label": "y",
            "optional_blob": "aGVsbG8=",
            "optional_stamp": "2026-08-11T00:00:00Z",
            "optional_extent": {"width": 640, "height": 480},
            "optional_samples": [1.0, 2.0],
        })),
        "optional fields may all be present"
    );
    assert!(
        !optionals.is_valid(&json!({
            "required_label": "x",
            "optional_extent": {"width": 640},
        })),
        "nested objects require all of their properties"
    );
    assert!(
        !optionals.is_valid(&json!({})),
        "required fields stay required"
    );

    let nested = compiled_validator(golden_format("nested_object"));
    assert!(nested.is_valid(&json!({
        "header": {
            "stamp": "2026-08-11T00:00:00Z",
            "frame_id": 7,
            "origin": {"x_m": 0.5, "y_m": -0.5},
        },
        "status_code": 0,
    })));
    assert!(
        !nested.is_valid(&json!({
            "header": {
                "stamp": "2026-08-11T00:00:00Z",
                "frame_id": 7,
                "origin": {"x_m": 0.5},
            },
            "status_code": 0,
        })),
        "deeply nested objects require all of their properties"
    );

    let arrays = compiled_validator(golden_format("arrays"));
    let good = json!({
        "readings": [],
        "corners": [1, 2, 3, 4],
        "frame": "aGVsbG8gd29ybGQ=",
        "crc_block": "aGVsbG8=",
        "payload": "cGVwcHk=",
        "poses": [{"joint_name": "elbow", "angle_rad": 1.25}],
    });
    assert!(
        arrays.is_valid(&good),
        "the canonical array instance validates"
    );
    let mut short = good.clone();
    short["corners"] = json!([1, 2, 3]);
    assert!(
        !arrays.is_valid(&short),
        "fixed-length arrays reject fewer items"
    );
    let mut long = good.clone();
    long["corners"] = json!([1, 2, 3, 4, 5]);
    assert!(
        !arrays.is_valid(&long),
        "fixed-length arrays reject extra items"
    );
    let mut wrong_len = good.clone();
    wrong_len["crc_block"] = json!("aGVsbG8");
    assert!(
        !arrays.is_valid(&wrong_len),
        "a fixed byte length pins the exact base64 text length"
    );
    let mut bad_pose = good.clone();
    bad_pose["poses"] = json!([{"joint_name": "elbow"}]);
    assert!(
        !arrays.is_valid(&bad_pose),
        "array item objects require their properties"
    );
}

#[test]
fn empty_object_schema_accepts_only_the_empty_object() {
    let validator = jsonschema::validator_for(&empty_object_schema()).expect("valid JSON Schema");
    assert!(validator.is_valid(&json!({})));
    assert!(!validator.is_valid(&json!({"anything": 1})));
}

#[test]
fn a_fixed_length_array_of_objects_is_refused() {
    let format = parse_format(
        r#"{
            poses: {
                $type: "array",
                $items: { $type: "object", x: "f64" },
                $length: 3,
            },
        }"#,
    );
    let err = message_format_to_json_schema(&format).expect_err("fixed arrays hold scalars only");
    assert!(
        matches!(
            err,
            FormatRuleViolation::UnsupportedFixedArrayItemType { .. }
        ),
        "{err}"
    );
}

#[test]
fn a_fixed_length_array_of_strings_is_refused() {
    let format = parse_format(r#"{ names: { $type: "array", $items: "string", $length: 2 } }"#);
    let err = message_format_to_json_schema(&format).expect_err("fixed arrays hold scalars only");
    assert!(
        matches!(
            err,
            FormatRuleViolation::UnsupportedFixedArrayItemType { .. }
        ),
        "{err}"
    );
}

#[test]
fn a_reserved_field_name_is_refused() {
    let format = parse_format(r#"{ instance_id: "u32" }"#);
    let err = message_format_to_json_schema(&format).expect_err("reserved names are refused");
    assert!(
        matches!(err, FormatRuleViolation::ReservedFieldName { .. }),
        "{err}"
    );
}

/// The size bound is exact for closed shapes: serializing the worst-case
/// instance of a bounded format lands exactly on the computed maximum.
#[test]
fn max_size_is_exact_for_bounded_formats() {
    let format = parse_format(r#"{ a: "bool" }"#);
    assert_eq!(
        max_serialized_json_bytes(&format),
        MaxSerializedSize::Bounded(11)
    );
    assert_eq!(
        serde_json::to_string(&json!({"a": false}))
            .expect("serializes")
            .len(),
        11
    );

    let format = parse_format(r#"{ id: "u32", pair: { $type: "object", x: "i8", y: "i8" } }"#);
    assert_eq!(
        max_serialized_json_bytes(&format),
        MaxSerializedSize::Bounded(44)
    );
    let worst = json!({"id": 4294967295u64, "pair": {"x": -128, "y": -128}});
    assert_eq!(serde_json::to_string(&worst).expect("serializes").len(), 44);

    let format = parse_format(r#"{ crc: { $type: "array", $items: "u8", $length: 5 } }"#);
    assert_eq!(
        max_serialized_json_bytes(&format),
        MaxSerializedSize::Bounded(18)
    );
    assert_eq!(
        serde_json::to_string(&json!({"crc": "aGVsbG9z"}))
            .expect("serializes")
            .len(),
        18
    );

    let format = parse_format(r#"{ corners: { $type: "array", $items: "u16", $length: 4 } }"#);
    // Braces + quoted name + colon + brackets + four 5-digit items + commas.
    assert_eq!(
        max_serialized_json_bytes(&format),
        MaxSerializedSize::Bounded(37)
    );
    assert_eq!(
        serde_json::to_string(&json!({"corners": [65535, 65535, 65535, 65535]}))
            .expect("serializes")
            .len(),
        37
    );
}

/// An `f32` is widened to `f64` before it is serialized, so its worst case
/// is the `f64` rendering: `0.1f32` reaches JSON as `0.10000000149011612`,
/// not as `0.1`. The bound has to cover what the runtime actually emits.
#[test]
fn max_size_covers_an_f32_widened_to_f64() {
    let format_json5 = r#"{ ratio: "f32" }"#;
    let MaxSerializedSize::Bounded(max) = max_serialized_json_bytes(&parse_format(format_json5))
    else {
        panic!("an f32 member has a static maximum");
    };
    let emitted = serde_json::to_string(&json!({"ratio": f64::from(0.1f32)})).expect("serializes");
    assert_eq!(emitted, r#"{"ratio":0.10000000149011612}"#);
    assert!(
        emitted.len() as u64 <= max,
        "the widened rendering ({} bytes) must fit the bound ({max} bytes)",
        emitted.len()
    );
    assert!(
        compiled_validator(format_json5).is_valid(&json!({
            "ratio": f64::from(0.1f32),
        })),
        "the derived schema accepts the value the runtime emits"
    );
}

#[test]
fn max_size_reports_unbounded_members() {
    for format_json5 in [
        r#"{ label: "string" }"#,
        r#"{ payload: "bytes" }"#,
        r#"{ frame: { $type: "array", $items: "u8" } }"#,
        r#"{ readings: { $type: "array", $items: "f32" } }"#,
        r#"{ header: { $type: "object", note: "string" } }"#,
    ] {
        let format = parse_format(format_json5);
        assert_eq!(
            max_serialized_json_bytes(&format),
            MaxSerializedSize::Unbounded,
            "{format_json5}"
        );
    }
}
