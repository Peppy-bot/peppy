//! Topic snapshot policies: image representation and result-size handling.
//!
//! [`apply_topic_policies`] runs after the update-rate gate admitted a
//! message and the bridge decoded it to canonical JSON. It transcodes
//! image-carrying snapshots to the declared codec, then enforces
//! `max_result_bytes` on the final serialized content, downscaling or
//! rejecting oversize snapshots as the exposure declares.

use crate::error::PublishError;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{ExtendedColorType, ImageFormat, RgbImage};
use peppy_mcp_catalog::{ImageCodec, ImageFieldMap, OversizePolicy, ResourcePolicies};
use serde_json::Value;

/// Quality used when an exposure declares a jpeg representation without an
/// explicit `quality`.
pub(crate) const DEFAULT_JPEG_QUALITY: u8 = 80;

/// Downscaling halves dimensions until the snapshot fits; below this edge
/// length it gives up and rejects instead of serving unrecognizable thumbnails.
const MIN_DOWNSCALE_EDGE: u32 = 16;

/// Encoding labels that already carry JPEG bytes and pass through untouched.
const JPEG_ENCODINGS: [&str; 2] = ["mjpeg", "jpeg"];

/// The encoding label written after a transcode, matching the label
/// pass-through recognizes.
const TRANSCODED_ENCODING: &str = "mjpeg";

/// Applies the representation policy and the size policy to a snapshot,
/// returning the final serialized content a read serves.
pub(crate) fn apply_topic_policies(
    policies: &ResourcePolicies,
    value: &mut Value,
) -> Result<String, PublishError> {
    if let Some(representation) = &policies.representation {
        transcode(
            representation.image,
            representation.quality.map(|q| q.get()),
            &representation.fields,
            value,
        )?;
    }
    let serialized = serialize(value);
    let Some(limit) = policies.max_result_bytes else {
        return Ok(serialized);
    };
    let limit = limit.get();
    if serialized.len() as u64 <= limit {
        return Ok(serialized);
    }
    let downscalable_fields = policies
        .representation
        .as_ref()
        .filter(|representation| representation.image == ImageCodec::Jpeg)
        .map(|representation| &representation.fields);
    match (policies.on_oversize, downscalable_fields) {
        (Some(OversizePolicy::Downscale), Some(fields)) => {
            let quality = policies
                .representation
                .as_ref()
                .and_then(|representation| representation.quality)
                .map(|quality| quality.get())
                .unwrap_or(DEFAULT_JPEG_QUALITY);
            downscale_to_fit(fields, quality, value, limit, serialized)
        }
        _ => Err(PublishError::Oversize {
            size: serialized.len() as u64,
            limit,
        }),
    }
}

/// Serializes the snapshot to the compact form reads serve and size limits
/// measure.
fn serialize(value: &Value) -> String {
    serde_json::to_string(value).expect("JSON value serializes")
}

/// Rewrites an uncompressed frame into JPEG in place. Frames already
/// labelled as JPEG pass through untouched, so no decode cost is paid for
/// producers that compress at the source.
fn transcode(
    codec: ImageCodec,
    quality: Option<u8>,
    fields: &ImageFieldMap,
    value: &mut Value,
) -> Result<(), PublishError> {
    if codec == ImageCodec::Raw {
        return Ok(());
    }
    let encoding = get_str(value, &fields.encoding, "encoding")?;
    if JPEG_ENCODINGS.contains(&encoding) {
        return Ok(());
    }
    let width = get_dimension(value, &fields.width, "width")?;
    let height = get_dimension(value, &fields.height, "height")?;
    let bytes = BASE64
        .decode(get_str(value, &fields.data, "data")?.as_bytes())
        .map_err(|_| PublishError::Field {
            role: "data",
            name: fields.data.clone(),
            problem: "is not valid base64".to_string(),
        })?;
    let rgb = pixels_as_rgb8(encoding, bytes, width, height)?;
    let jpeg = encode_jpeg(&rgb, quality.unwrap_or(DEFAULT_JPEG_QUALITY))?;
    set_field(value, &fields.data, Value::String(BASE64.encode(&jpeg)));
    set_field(
        value,
        &fields.encoding,
        Value::String(TRANSCODED_ENCODING.to_string()),
    );
    Ok(())
}

/// Halves the frame's dimensions until the serialized snapshot fits the
/// limit, rewriting the data, width, and height fields on each step.
fn downscale_to_fit(
    fields: &ImageFieldMap,
    quality: u8,
    value: &mut Value,
    limit: u64,
    mut serialized: String,
) -> Result<String, PublishError> {
    let mut decoded = {
        let bytes = BASE64
            .decode(get_str(value, &fields.data, "data")?.as_bytes())
            .map_err(|_| PublishError::Field {
                role: "data",
                name: fields.data.clone(),
                problem: "is not valid base64".to_string(),
            })?;
        image::load_from_memory_with_format(&bytes, ImageFormat::Jpeg).map_err(|error| {
            PublishError::BadFrame {
                detail: error.to_string(),
            }
        })?
    };
    loop {
        let (width, height) = (decoded.width(), decoded.height());
        if width / 2 < MIN_DOWNSCALE_EDGE || height / 2 < MIN_DOWNSCALE_EDGE {
            return Err(PublishError::Oversize {
                size: serialized.len() as u64,
                limit,
            });
        }
        decoded = decoded.resize_exact(width / 2, height / 2, FilterType::Triangle);
        let jpeg = encode_jpeg(&decoded.to_rgb8(), quality)?;
        set_field(value, &fields.data, Value::String(BASE64.encode(&jpeg)));
        set_field(value, &fields.width, Value::from(width / 2));
        set_field(value, &fields.height, Value::from(height / 2));
        serialized = serialize(value);
        if serialized.len() as u64 <= limit {
            return Ok(serialized);
        }
    }
}

/// Interprets raw pixel bytes under their encoding label as an RGB image.
fn pixels_as_rgb8(
    encoding: &str,
    mut bytes: Vec<u8>,
    width: u32,
    height: u32,
) -> Result<RgbImage, PublishError> {
    // Declared dimensions are arbitrary `u32`s, so the RGB8 buffer size they
    // ask for can exceed `usize`; that is a bad frame, not an overflow.
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or_else(|| PublishError::BadFrame {
            detail: format!("{width}x{height} {encoding} does not fit in memory"),
        })?;
    match encoding {
        "rgb8" => {}
        "bgr8" => bytes.chunks_exact_mut(3).for_each(|pixel| pixel.swap(0, 2)),
        other => {
            return Err(PublishError::UnsupportedEncoding {
                encoding: other.to_string(),
            });
        }
    }
    if bytes.len() != expected {
        return Err(PublishError::BadFrame {
            detail: format!(
                "{} bytes do not match {width}x{height} {encoding} ({expected} expected)",
                bytes.len()
            ),
        });
    }
    RgbImage::from_raw(width, height, bytes).ok_or_else(|| PublishError::BadFrame {
        detail: format!("{width}x{height} frame does not form an image"),
    })
}

fn encode_jpeg(image: &RgbImage, quality: u8) -> Result<Vec<u8>, PublishError> {
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, quality)
        .encode(
            image.as_raw(),
            image.width(),
            image.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|error| PublishError::BadFrame {
            detail: format!("jpeg encoding failed: {error}"),
        })?;
    Ok(jpeg)
}

fn get_str<'a>(value: &'a Value, name: &str, role: &'static str) -> Result<&'a str, PublishError> {
    field(value, name, role)?
        .as_str()
        .ok_or_else(|| PublishError::Field {
            role,
            name: name.to_string(),
            problem: "is not a string".to_string(),
        })
}

fn get_dimension(value: &Value, name: &str, role: &'static str) -> Result<u32, PublishError> {
    field(value, name, role)?
        .as_u64()
        .and_then(|dimension| u32::try_from(dimension).ok())
        .ok_or_else(|| PublishError::Field {
            role,
            name: name.to_string(),
            problem: "is not an unsigned integer dimension".to_string(),
        })
}

fn field<'a>(value: &'a Value, name: &str, role: &'static str) -> Result<&'a Value, PublishError> {
    value
        .as_object()
        .ok_or(PublishError::NotAnObject)?
        .get(name)
        .ok_or_else(|| PublishError::Field {
            role,
            name: name.to_string(),
            problem: "is absent from the snapshot".to_string(),
        })
}

fn set_field(value: &mut Value, name: &str, new_value: Value) {
    if let Some(object) = value.as_object_mut() {
        object.insert(name.to_string(), new_value);
    }
}

/// Decodes the JPEG carried in the snapshot's data field, for tests and
/// diagnostics.
#[cfg(test)]
fn decode_snapshot_jpeg(value: &Value, fields: &ImageFieldMap) -> image::DynamicImage {
    let data = get_str(value, &fields.data, "data").expect("data field present");
    let bytes = BASE64
        .decode(data.as_bytes())
        .expect("data field is base64");
    image::load_from_memory_with_format(&bytes, ImageFormat::Jpeg).expect("data field is a JPEG")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policies(raw: Value) -> ResourcePolicies {
        serde_json::from_value(raw).expect("valid policies")
    }

    fn frame_fields() -> ImageFieldMap {
        serde_json::from_value(json!({
            "data": "frame", "encoding": "encoding", "width": "width", "height": "height"
        }))
        .expect("valid field map")
    }

    fn jpeg_policies(max_result_bytes: Option<u64>, on_oversize: Option<&str>) -> ResourcePolicies {
        let mut raw = json!({
            "freshness": { "max_age_ms": 2000 },
            "update": { "max_hz": 2.0 },
            "representation": {
                "image": "jpeg",
                "quality": 80,
                "fields": { "data": "frame", "encoding": "encoding", "width": "width", "height": "height" }
            }
        });
        if let Some(limit) = max_result_bytes {
            raw["max_result_bytes"] = json!(limit);
        }
        if let Some(policy) = on_oversize {
            raw["on_oversize"] = json!(policy);
        }
        policies(raw)
    }

    /// A frame whose pixel bytes repeat one color triple.
    fn solid_frame(encoding: &str, triple: [u8; 3], width: u32, height: u32) -> Value {
        let bytes: Vec<u8> = (0..width * height).flat_map(|_| triple).collect();
        json!({
            "frame": BASE64.encode(&bytes),
            "encoding": encoding,
            "width": width,
            "height": height,
        })
    }

    /// A deterministic high-detail frame that JPEG cannot compress well.
    fn noisy_frame(width: u32, height: u32) -> Value {
        let bytes: Vec<u8> = (0..width as usize * height as usize * 3)
            .map(|index| ((index * 97 + index / 3 * 31) % 256) as u8)
            .collect();
        json!({
            "frame": BASE64.encode(&bytes),
            "encoding": "rgb8",
            "width": width,
            "height": height,
        })
    }

    #[test]
    fn raw_codec_passes_frames_through_untouched() {
        let raw_policies = policies(json!({
            "freshness": { "max_age_ms": 2000 },
            "update": { "max_hz": 2.0 },
            "representation": {
                "image": "raw",
                "fields": { "data": "frame", "encoding": "encoding", "width": "width", "height": "height" }
            }
        }));
        let mut value = solid_frame("rgb8", [1, 2, 3], 4, 4);
        let original = value.clone();
        apply_topic_policies(&raw_policies, &mut value).expect("raw passes through");
        assert_eq!(value, original);
    }

    #[test]
    fn frames_already_jpeg_encoded_pass_through_without_transcoding() {
        let mut value = json!({
            "frame": BASE64.encode(b"not really a jpeg, and never decoded"),
            "encoding": "mjpeg",
            "width": 4,
            "height": 4,
        });
        let original = value.clone();
        apply_topic_policies(&jpeg_policies(None, None), &mut value).expect("mjpeg passes through");
        assert_eq!(value, original);
    }

    #[test]
    fn rgb8_frames_transcode_to_jpeg_and_rewrite_the_encoding() {
        let mut value = solid_frame("rgb8", [200, 30, 30], 8, 8);
        apply_topic_policies(&jpeg_policies(None, None), &mut value).expect("transcodes");
        assert_eq!(value["encoding"], "mjpeg");
        assert_eq!(value["width"], 8);
        assert_eq!(value["height"], 8);
        let decoded = decode_snapshot_jpeg(&value, &frame_fields()).to_rgb8();
        assert_eq!((decoded.width(), decoded.height()), (8, 8));
        let pixel = decoded.get_pixel(4, 4);
        assert!(
            pixel[0] > 150 && pixel[1] < 90 && pixel[2] < 90,
            "expected red-ish, got {pixel:?}"
        );
    }

    #[test]
    fn bgr8_frames_swap_channels_before_encoding() {
        let mut value = solid_frame("bgr8", [200, 30, 30], 8, 8);
        apply_topic_policies(&jpeg_policies(None, None), &mut value).expect("transcodes");
        let decoded = decode_snapshot_jpeg(&value, &frame_fields()).to_rgb8();
        let pixel = decoded.get_pixel(4, 4);
        assert!(
            pixel[2] > 150 && pixel[0] < 90 && pixel[1] < 90,
            "expected blue-ish, got {pixel:?}"
        );
    }

    #[test]
    fn unsupported_encodings_are_refused() {
        let mut value = solid_frame("yuyv", [1, 2, 3], 4, 4);
        let error = apply_topic_policies(&jpeg_policies(None, None), &mut value)
            .expect_err("yuyv is not transcodable");
        assert_eq!(
            error,
            PublishError::UnsupportedEncoding {
                encoding: "yuyv".to_string()
            }
        );
    }

    #[test]
    fn frames_with_wrong_byte_counts_are_refused() {
        let mut value = json!({
            "frame": BASE64.encode([1u8, 2, 3]),
            "encoding": "rgb8",
            "width": 4,
            "height": 4,
        });
        let error = apply_topic_policies(&jpeg_policies(None, None), &mut value)
            .expect_err("3 bytes are not a 4x4 frame");
        assert!(
            matches!(error, PublishError::BadFrame { .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn frames_whose_dimensions_overflow_the_buffer_size_are_refused() {
        let mut value = json!({
            "frame": BASE64.encode([1u8, 2, 3]),
            "encoding": "rgb8",
            "width": u32::MAX,
            "height": u32::MAX,
        });
        let error = apply_topic_policies(&jpeg_policies(None, None), &mut value)
            .expect_err("the RGB8 buffer those dimensions ask for exceeds usize");
        assert!(
            matches!(error, PublishError::BadFrame { .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn missing_and_mistyped_representation_fields_are_refused() {
        let mut missing = json!({ "encoding": "rgb8", "width": 4, "height": 4 });
        let error = apply_topic_policies(&jpeg_policies(None, None), &mut missing)
            .expect_err("data field is absent");
        assert_eq!(
            error,
            PublishError::Field {
                role: "data",
                name: "frame".to_string(),
                problem: "is absent from the snapshot".to_string(),
            }
        );

        let mut mistyped = json!({ "frame": 7, "encoding": "rgb8", "width": 4, "height": 4 });
        let error = apply_topic_policies(&jpeg_policies(None, None), &mut mistyped)
            .expect_err("data field is not a string");
        assert_eq!(
            error,
            PublishError::Field {
                role: "data",
                name: "frame".to_string(),
                problem: "is not a string".to_string(),
            }
        );
    }

    #[test]
    fn oversize_snapshots_without_a_downscale_policy_are_rejected() {
        let reject_policies = policies(json!({
            "freshness": { "max_age_ms": 2000 },
            "update": { "max_hz": 2.0 },
            "max_result_bytes": 32,
            "on_oversize": "reject",
        }));
        let mut value = json!({ "status": "x".repeat(64) });
        let error = apply_topic_policies(&reject_policies, &mut value)
            .expect_err("oversize snapshot should be rejected");
        assert!(
            matches!(error, PublishError::Oversize { limit: 32, .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn undersize_snapshots_pass_the_size_check() {
        let reject_policies = policies(json!({
            "freshness": { "max_age_ms": 2000 },
            "update": { "max_hz": 2.0 },
            "max_result_bytes": 1024,
            "on_oversize": "reject",
        }));
        let mut value = json!({ "status": "ok" });
        let serialized =
            apply_topic_policies(&reject_policies, &mut value).expect("small snapshot fits");
        assert_eq!(serialized, "{\"status\":\"ok\"}");
    }

    #[test]
    fn downscale_halves_dimensions_until_the_snapshot_fits() {
        let mut value = noisy_frame(128, 128);
        let full_size = {
            let mut probe = value.clone();
            apply_topic_policies(&jpeg_policies(None, None), &mut probe)
                .expect("transcodes")
                .len()
        };
        let limit = (full_size / 2) as u64;
        let serialized =
            apply_topic_policies(&jpeg_policies(Some(limit), Some("downscale")), &mut value)
                .expect("downscale fits the frame");
        assert!(serialized.len() as u64 <= limit);
        let width = value["width"].as_u64().expect("width is rewritten");
        let height = value["height"].as_u64().expect("height is rewritten");
        assert!(
            width < 128 && height < 128,
            "dimensions should shrink, got {width}x{height}"
        );
        assert_eq!(value["encoding"], "mjpeg");
        let decoded = decode_snapshot_jpeg(&value, &frame_fields());
        assert_eq!(
            (decoded.width() as u64, decoded.height() as u64),
            (width, height)
        );
    }

    #[test]
    fn downscale_gives_up_below_the_minimum_edge_and_rejects() {
        let mut value = noisy_frame(64, 64);
        let error = apply_topic_policies(&jpeg_policies(Some(24), Some("downscale")), &mut value)
            .expect_err("24 bytes can never fit a frame");
        assert!(
            matches!(error, PublishError::Oversize { limit: 24, .. }),
            "got {error:?}"
        );
    }
}
