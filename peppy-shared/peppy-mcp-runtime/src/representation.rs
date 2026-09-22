//! Topic snapshot policies: image representation and result-size handling.
//!
//! [`apply_topic_policies`] runs after the update-rate gate admitted a
//! message and the bridge decoded it to canonical JSON. It transcodes
//! image-carrying snapshots to the declared codec, colour frames as they
//! are and `z16` depth frames as a greyscale picture, then enforces
//! `max_result_bytes` on the final serialized content, downscaling or
//! rejecting oversize snapshots as the exposure declares.

use crate::error::PublishError;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ExtendedColorType, GrayImage, ImageFormat, RgbImage};
use peppy_mcp_catalog::{
    DepthRange, ImageCodec, ImageFieldMap, ImageRepresentation, OversizePolicy, ResourcePolicies,
};
use serde_json::Value;
use std::borrow::Cow;

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
        transcode(representation, value)?;
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

/// Rewrites an uncompressed frame into JPEG in place: a colour frame as it
/// is, a `z16` depth frame as the greyscale picture the representation's
/// `depth_range` spans. Frames already labelled as JPEG pass through
/// untouched, so no decode cost is paid for producers that compress at the
/// source.
fn transcode(representation: &ImageRepresentation, value: &mut Value) -> Result<(), PublishError> {
    if representation.image == ImageCodec::Raw {
        return Ok(());
    }
    let fields = &representation.fields;
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
    let picture = decode_frame(encoding, bytes, width, height, representation.depth_range)?;
    let quality = representation
        .quality
        .map(|quality| quality.get())
        .unwrap_or(DEFAULT_JPEG_QUALITY);
    let jpeg = encode_jpeg(&picture, quality)?;
    set_field(value, &fields.data, Value::String(BASE64.encode(&jpeg)));
    set_field(
        value,
        &fields.encoding,
        Value::String(TRANSCODED_ENCODING.to_string()),
    );
    Ok(())
}

/// Halves the frame's dimensions until the serialized snapshot fits the
/// limit, rewriting the data, width, and height fields on each step. The
/// picture keeps its colour type, so a greyscale depth picture stays one
/// channel.
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
        let jpeg = encode_jpeg(&decoded, quality)?;
        set_field(value, &fields.data, Value::String(BASE64.encode(&jpeg)));
        set_field(value, &fields.width, Value::from(width / 2));
        set_field(value, &fields.height, Value::from(height / 2));
        serialized = serialize(value);
        if serialized.len() as u64 <= limit {
            return Ok(serialized);
        }
    }
}

/// Interprets raw frame bytes under their encoding label as the picture a
/// JPEG carries: `rgb8` and `bgr8` in colour, `z16` as the greyscale
/// picture `depth_range` spans.
fn decode_frame(
    encoding: &str,
    mut bytes: Vec<u8>,
    width: u32,
    height: u32,
    depth_range: Option<DepthRange>,
) -> Result<DynamicImage, PublishError> {
    match encoding {
        "rgb8" => pixels_as_rgb8(bytes, width, height, encoding).map(DynamicImage::ImageRgb8),
        "bgr8" => {
            bytes
                .as_chunks_mut::<3>()
                .0
                .iter_mut()
                .for_each(|pixel| pixel.swap(0, 2));
            pixels_as_rgb8(bytes, width, height, encoding).map(DynamicImage::ImageRgb8)
        }
        "z16" => {
            let range = depth_range.ok_or(PublishError::DepthRangeMissing)?;
            z16_as_gray8(&bytes, width, height, range, encoding).map(DynamicImage::ImageLuma8)
        }
        other => Err(PublishError::UnsupportedEncoding {
            encoding: other.to_string(),
        }),
    }
}

/// Refuses bytes that do not fill a `width` x `height` frame of
/// `bytes_per_pixel`. Declared dimensions are arbitrary `u32`s, so the
/// buffer size they ask for can exceed `usize`; that is a bad frame, not an
/// overflow.
fn check_frame_size(
    bytes: &[u8],
    width: u32,
    height: u32,
    bytes_per_pixel: usize,
    encoding: &str,
) -> Result<(), PublishError> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
        .ok_or_else(|| PublishError::BadFrame {
            detail: format!("{width}x{height} {encoding} does not fit in memory"),
        })?;
    if bytes.len() != expected {
        return Err(PublishError::BadFrame {
            detail: format!(
                "{} bytes do not match {width}x{height} {encoding} ({expected} expected)",
                bytes.len()
            ),
        });
    }
    Ok(())
}

/// Three bytes per pixel, red first, as an RGB image.
fn pixels_as_rgb8(
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    encoding: &str,
) -> Result<RgbImage, PublishError> {
    check_frame_size(&bytes, width, height, 3, encoding)?;
    RgbImage::from_raw(width, height, bytes).ok_or_else(|| PublishError::BadFrame {
        detail: format!("{width}x{height} frame does not form an image"),
    })
}

/// One little-endian `u16` reading per pixel, as the greyscale picture
/// `range` spans.
fn z16_as_gray8(
    bytes: &[u8],
    width: u32,
    height: u32,
    range: DepthRange,
    encoding: &str,
) -> Result<GrayImage, PublishError> {
    check_frame_size(bytes, width, height, 2, encoding)?;
    let shades = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| z16_shade(u16::from_le_bytes(*pair), range))
        .collect();
    GrayImage::from_raw(width, height, shades).ok_or_else(|| PublishError::BadFrame {
        detail: format!("{width}x{height} frame does not form an image"),
    })
}

/// The shade one `z16` reading shows under `range`: white at `near` or
/// nearer, black at `far` or farther, linear between, and black for 0, a
/// pixel with no reading. Integer arithmetic, rounded to the nearest shade.
fn z16_shade(reading: u16, range: DepthRange) -> u8 {
    if reading == 0 {
        return 0;
    }
    let (near, far) = (u32::from(range.near()), u32::from(range.far()));
    let span = far - near;
    let distance_to_far = far - u32::from(reading).clamp(near, far);
    ((distance_to_far * 255 + span / 2) / span) as u8
}

/// Encodes a picture as JPEG in its own colour type: greyscale stays one
/// channel, anything else goes as RGB.
fn encode_jpeg(picture: &DynamicImage, quality: u8) -> Result<Vec<u8>, PublishError> {
    let (pixels, color_type): (Cow<'_, [u8]>, ExtendedColorType) = match picture {
        DynamicImage::ImageLuma8(gray) => (Cow::Borrowed(gray.as_raw()), ExtendedColorType::L8),
        DynamicImage::ImageRgb8(rgb) => (Cow::Borrowed(rgb.as_raw()), ExtendedColorType::Rgb8),
        other => (
            Cow::Owned(other.to_rgb8().into_raw()),
            ExtendedColorType::Rgb8,
        ),
    };
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, quality)
        .encode(&pixels, picture.width(), picture.height(), color_type)
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

    /// A jpeg representation with the depth range the chest camera's
    /// exposure declares: 0.1 m white to 10 m black, in millimetres.
    fn depth_policies(
        max_result_bytes: Option<u64>,
        on_oversize: Option<&str>,
    ) -> ResourcePolicies {
        let mut raw = json!({
            "freshness": { "max_age_ms": 2000 },
            "update": { "max_hz": 2.0 },
            "representation": {
                "image": "jpeg",
                "quality": 80,
                "fields": { "data": "frame", "encoding": "encoding", "width": "width", "height": "height" },
                "depth_range": { "near": 100, "far": 10000 }
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

    fn z16_frame(readings: impl Iterator<Item = u16>, width: u32, height: u32) -> Value {
        let bytes: Vec<u8> = readings.flat_map(u16::to_le_bytes).collect();
        json!({
            "frame": BASE64.encode(&bytes),
            "encoding": "z16",
            "width": width,
            "height": height,
        })
    }

    /// A depth frame whose every pixel carries one reading.
    fn solid_depth_frame(reading: u16, width: u32, height: u32) -> Value {
        z16_frame(
            std::iter::repeat_n(reading, (width * height) as usize),
            width,
            height,
        )
    }

    /// A deterministic depth frame of readings all over the range, which
    /// JPEG cannot compress well.
    fn noisy_depth_frame(width: u32, height: u32) -> Value {
        let readings = (0..width as usize * height as usize)
            .map(|index| ((index * 97 + index / 3 * 31) % 9900 + 100) as u16);
        z16_frame(readings, width, height)
    }

    fn decoded_gray(value: &Value) -> GrayImage {
        match decode_snapshot_jpeg(value, &frame_fields()) {
            DynamicImage::ImageLuma8(gray) => gray,
            other => panic!("expected a one-channel picture, got {:?}", other.color()),
        }
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
    fn z16_shades_run_white_at_near_to_black_at_far_and_black_for_no_reading() {
        let range = DepthRange::new(100, 10000).expect("valid range");
        assert_eq!(z16_shade(0, range), 0);
        assert_eq!(z16_shade(100, range), 255);
        assert_eq!(z16_shade(10000, range), 0);
        // Halfway between the ends is the middle shade, rounded to nearest.
        assert_eq!(z16_shade(5050, range), 128);
        assert_eq!(z16_shade(2575, range), 191);
        // Readings beyond either end clamp to it.
        assert_eq!(z16_shade(1, range), 255);
        assert_eq!(z16_shade(u16::MAX, range), 0);
        // The full span of the wire format is a valid range.
        let widest = DepthRange::new(1, u16::MAX).expect("valid range");
        assert_eq!(z16_shade(1, widest), 255);
        assert_eq!(z16_shade(u16::MAX, widest), 0);
    }

    #[test]
    fn z16_frames_transcode_to_a_greyscale_jpeg_spanning_the_depth_range() {
        for (reading, expected_shade) in [(100u16, 255u8), (10000, 0), (0, 0), (5050, 128)] {
            let mut value = solid_depth_frame(reading, 8, 8);
            apply_topic_policies(&depth_policies(None, None), &mut value).expect("transcodes");
            assert_eq!(value["encoding"], "mjpeg");
            assert_eq!(value["width"], 8);
            assert_eq!(value["height"], 8);
            let gray = decoded_gray(&value);
            assert_eq!((gray.width(), gray.height()), (8, 8));
            let shade = gray.get_pixel(4, 4)[0];
            assert!(
                shade.abs_diff(expected_shade) <= 3,
                "reading {reading} should render as shade {expected_shade}, got {shade}"
            );
        }
    }

    #[test]
    fn z16_frames_without_a_depth_range_are_refused() {
        let mut value = solid_depth_frame(500, 4, 4);
        let error = apply_topic_policies(&jpeg_policies(None, None), &mut value)
            .expect_err("no scale to render readings on");
        assert_eq!(error, PublishError::DepthRangeMissing);
    }

    #[test]
    fn a_depth_range_leaves_colour_frames_untouched() {
        let mut value = solid_frame("rgb8", [200, 30, 30], 8, 8);
        apply_topic_policies(&depth_policies(None, None), &mut value).expect("transcodes");
        let decoded = decode_snapshot_jpeg(&value, &frame_fields()).to_rgb8();
        let pixel = decoded.get_pixel(4, 4);
        assert!(
            pixel[0] > 150 && pixel[1] < 90 && pixel[2] < 90,
            "expected red-ish, got {pixel:?}"
        );
    }

    #[test]
    fn z16_frames_with_wrong_byte_counts_are_refused() {
        let mut value = json!({
            "frame": BASE64.encode([1u8, 2, 3]),
            "encoding": "z16",
            "width": 4,
            "height": 4,
        });
        let error = apply_topic_policies(&depth_policies(None, None), &mut value)
            .expect_err("3 bytes are not a 4x4 depth frame");
        assert!(
            matches!(error, PublishError::BadFrame { .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn downscale_keeps_a_depth_picture_one_channel() {
        let mut value = noisy_depth_frame(128, 128);
        let full_size = {
            let mut probe = value.clone();
            apply_topic_policies(&depth_policies(None, None), &mut probe)
                .expect("transcodes")
                .len()
        };
        let limit = (full_size / 2) as u64;
        let serialized =
            apply_topic_policies(&depth_policies(Some(limit), Some("downscale")), &mut value)
                .expect("downscale fits the frame");
        assert!(serialized.len() as u64 <= limit);
        let width = value["width"].as_u64().expect("width is rewritten");
        let height = value["height"].as_u64().expect("height is rewritten");
        assert!(
            width < 128 && height < 128,
            "dimensions should shrink, got {width}x{height}"
        );
        let gray = decoded_gray(&value);
        assert_eq!((gray.width() as u64, gray.height() as u64), (width, height));
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
