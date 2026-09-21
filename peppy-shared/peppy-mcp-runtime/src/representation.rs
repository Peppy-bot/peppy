//! Topic snapshot policies: image representation and result-size handling.
//!
//! [`apply_topic_policies`] runs after the update-rate gate admitted a
//! message and the bridge decoded it to canonical JSON. It transcodes
//! image-carrying snapshots to the declared codec: under `jpeg`, colour
//! frames as they are and 16-bit depth frames as a greyscale picture; under
//! `png16`, 16-bit single-channel frames such as a depth map losslessly.
//! It then enforces `max_result_bytes` on the final serialized content,
//! downscaling or rejecting oversize snapshots as the exposure declares.

use crate::error::PublishError;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ExtendedColorType, GrayImage, ImageFormat, Luma, RgbImage};
use peppy_mcp_catalog::{
    DepthRange, ImageCodec, ImageFieldMap, ImageRepresentation, OversizePolicy, ResourcePolicies,
};
use serde_json::Value;
use std::borrow::Cow;
use std::io::Cursor;

/// Quality used when an exposure declares a jpeg representation without an
/// explicit `quality`.
pub(crate) const DEFAULT_JPEG_QUALITY: u8 = 80;

/// Downscaling halves dimensions until the snapshot fits; below this edge
/// length it gives up and rejects instead of serving unrecognizable thumbnails.
const MIN_DOWNSCALE_EDGE: u32 = 16;

/// Encoding labels that already carry JPEG bytes and pass through untouched.
const JPEG_ENCODINGS: [&str; 2] = ["mjpeg", "jpeg"];

/// The encoding label written after a transcode to JPEG, matching the label
/// pass-through recognizes.
const TRANSCODED_JPEG_ENCODING: &str = "mjpeg";

/// Encoding labels that already carry PNG bytes and pass through once they
/// read as 16-bit single-channel.
const PNG_ENCODINGS: [&str; 1] = ["png"];

/// The encoding label written after a transcode to PNG.
const TRANSCODED_PNG_ENCODING: &str = "png";

/// Encoding labels of one unsigned 16-bit little-endian sample per pixel: a
/// depth map in the unit its stream info reports.
const U16_ENCODINGS: [&str; 3] = ["z16", "mono16", "16UC1"];

/// A 16-bit image in memory, before it is encoded as PNG.
type Luma16Image = image::ImageBuffer<Luma<u16>, Vec<u16>>;

/// Applies the representation policy and the size policy to a snapshot,
/// returning the final serialized content a read serves.
pub(crate) fn apply_topic_policies(
    policies: &ResourcePolicies,
    value: &mut Value,
) -> Result<String, PublishError> {
    if let Some(representation) = &policies.representation {
        match representation.image {
            ImageCodec::Raw => {}
            ImageCodec::Jpeg => transcode_jpeg(representation, value)?,
            ImageCodec::Png16 => transcode_png16(&representation.fields, value)?,
        }
    }
    let serialized = serialize(value);
    let Some(limit) = policies.max_result_bytes else {
        return Ok(serialized);
    };
    let limit = limit.get();
    if serialized.len() as u64 <= limit {
        return Ok(serialized);
    }
    let downscale = policies.representation.as_ref().and_then(Downscale::of);
    match (policies.on_oversize, downscale) {
        (Some(OversizePolicy::Downscale), Some((downscale, fields))) => {
            downscale_to_fit(&downscale, fields, value, limit, serialized)
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

/// The quality a `jpeg` representation encodes at.
fn jpeg_quality(representation: &ImageRepresentation) -> u8 {
    representation
        .quality
        .map(|quality| quality.get())
        .unwrap_or(DEFAULT_JPEG_QUALITY)
}

/// Rewrites an uncompressed frame into JPEG in place: a colour frame as it
/// is, a 16-bit depth frame as the greyscale picture the representation's
/// `depth_range` spans. A frame already labelled as JPEG passes through
/// untouched, so no decode cost is paid for producers that compress at the
/// source.
fn transcode_jpeg(
    representation: &ImageRepresentation,
    value: &mut Value,
) -> Result<(), PublishError> {
    let fields = &representation.fields;
    let encoding = get_str(value, &fields.encoding, "encoding")?;
    if JPEG_ENCODINGS.contains(&encoding) {
        return Ok(());
    }
    let (width, height, bytes) = read_frame(value, fields)?;
    let picture = decode_frame(encoding, bytes, width, height, representation.depth_range)?;
    let jpeg = encode_jpeg(&picture, jpeg_quality(representation))?;
    write_frame(value, fields, &jpeg, TRANSCODED_JPEG_ENCODING);
    Ok(())
}

/// Rewrites a 16-bit single-channel frame into a 16-bit PNG in place,
/// losslessly. A frame already labelled `png` passes through once it decodes
/// as 16-bit single-channel.
fn transcode_png16(fields: &ImageFieldMap, value: &mut Value) -> Result<(), PublishError> {
    let encoding = get_str(value, &fields.encoding, "encoding")?;
    if PNG_ENCODINGS.contains(&encoding) {
        return ensure_png16(&decode_data(value, fields)?);
    }
    let (width, height, bytes) = read_frame(value, fields)?;
    let png = encode_png16(&pixels_as_luma16(encoding, &bytes, width, height)?)?;
    write_frame(value, fields, &png, TRANSCODED_PNG_ENCODING);
    Ok(())
}

/// The frame's declared width and height, and its bytes.
fn read_frame(value: &Value, fields: &ImageFieldMap) -> Result<(u32, u32, Vec<u8>), PublishError> {
    let width = get_dimension(value, &fields.width, "width")?;
    let height = get_dimension(value, &fields.height, "height")?;
    Ok((width, height, decode_data(value, fields)?))
}

/// Writes the encoded frame back into the snapshot under its label.
fn write_frame(value: &mut Value, fields: &ImageFieldMap, encoded: &[u8], encoding: &str) {
    set_field(value, &fields.data, Value::String(BASE64.encode(encoded)));
    set_field(value, &fields.encoding, Value::String(encoding.to_string()));
}

/// How an oversize snapshot is shrunk under a codec that downscales.
enum Downscale {
    /// A JPEG picture, resampled and written at the representation's quality.
    Jpeg { quality: u8 },
    /// A 16-bit frame, keeping one source sample per output pixel.
    Png16,
}

impl Downscale {
    /// The downscale of `representation`, with the fields it reads, for a
    /// codec that downscales.
    fn of(representation: &ImageRepresentation) -> Option<(Self, &ImageFieldMap)> {
        let downscale = match representation.image {
            ImageCodec::Raw => return None,
            ImageCodec::Jpeg => Self::Jpeg {
                quality: jpeg_quality(representation),
            },
            ImageCodec::Png16 => Self::Png16,
        };
        Some((downscale, &representation.fields))
    }

    /// The container the snapshot's frame is decoded from.
    fn format(&self) -> ImageFormat {
        match self {
            Self::Jpeg { .. } => ImageFormat::Jpeg,
            Self::Png16 => ImageFormat::Png,
        }
    }

    /// The resampling that halves the picture.
    fn filter(&self) -> FilterType {
        match self {
            Self::Jpeg { .. } => FilterType::Triangle,
            Self::Png16 => FilterType::Nearest,
        }
    }

    /// Encodes the halved picture.
    fn encode(&self, picture: &DynamicImage) -> Result<Vec<u8>, PublishError> {
        match self {
            Self::Jpeg { quality } => encode_jpeg(picture, *quality),
            Self::Png16 => encode_png16(
                picture
                    .as_luma16()
                    .expect("a png16 snapshot holds a 16-bit single-channel PNG"),
            ),
        }
    }
}

/// Halves the frame's dimensions until the serialized snapshot fits the
/// limit, rewriting the data, width, and height fields on each step. A JPEG
/// picture is resampled and keeps its colour type, so a greyscale depth
/// picture stays one channel; a 16-bit frame keeps one source sample per
/// output pixel.
fn downscale_to_fit(
    downscale: &Downscale,
    fields: &ImageFieldMap,
    value: &mut Value,
    limit: u64,
    mut serialized: String,
) -> Result<String, PublishError> {
    let mut picture = load_image(&decode_data(value, fields)?, downscale.format())?;
    loop {
        let (width, height) = (picture.width(), picture.height());
        if width / 2 < MIN_DOWNSCALE_EDGE || height / 2 < MIN_DOWNSCALE_EDGE {
            return Err(PublishError::Oversize {
                size: serialized.len() as u64,
                limit,
            });
        }
        picture = picture.resize_exact(width / 2, height / 2, downscale.filter());
        set_field(
            value,
            &fields.data,
            Value::String(BASE64.encode(downscale.encode(&picture)?)),
        );
        set_field(value, &fields.width, Value::from(width / 2));
        set_field(value, &fields.height, Value::from(height / 2));
        serialized = serialize(value);
        if serialized.len() as u64 <= limit {
            return Ok(serialized);
        }
    }
}

/// The frame bytes the snapshot's data field carries.
fn decode_data(value: &Value, fields: &ImageFieldMap) -> Result<Vec<u8>, PublishError> {
    BASE64
        .decode(get_str(value, &fields.data, "data")?.as_bytes())
        .map_err(|_| PublishError::Field {
            role: "data",
            name: fields.data.clone(),
            problem: "is not valid base64".to_string(),
        })
}

/// Decodes compressed frame bytes in `format`.
fn load_image(bytes: &[u8], format: ImageFormat) -> Result<DynamicImage, PublishError> {
    image::load_from_memory_with_format(bytes, format).map_err(|error| PublishError::BadFrame {
        detail: error.to_string(),
    })
}

/// Interprets raw frame bytes under their encoding label as the picture a
/// JPEG carries: `rgb8` and `bgr8` in colour, a 16-bit depth frame as the
/// greyscale picture `depth_range` spans.
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
        depth if U16_ENCODINGS.contains(&depth) => {
            let range = depth_range.ok_or(PublishError::DepthRangeMissing)?;
            let samples = pixels_as_luma16(encoding, &bytes, width, height)?;
            Ok(DynamicImage::ImageLuma8(depth_as_gray8(&samples, range)))
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

/// One little-endian `u16` sample per pixel, as a 16-bit single-channel
/// image.
fn pixels_as_luma16(
    encoding: &str,
    bytes: &[u8],
    width: u32,
    height: u32,
) -> Result<Luma16Image, PublishError> {
    if !U16_ENCODINGS.contains(&encoding) {
        return Err(PublishError::UnsupportedEncoding {
            encoding: encoding.to_string(),
        });
    }
    check_frame_size(bytes, width, height, 2, encoding)?;
    let samples = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    Luma16Image::from_raw(width, height, samples).ok_or_else(|| PublishError::BadFrame {
        detail: format!("{width}x{height} frame does not form an image"),
    })
}

/// The greyscale picture `range` spans over 16-bit depth samples.
fn depth_as_gray8(samples: &Luma16Image, range: DepthRange) -> GrayImage {
    GrayImage::from_fn(samples.width(), samples.height(), |x, y| {
        Luma([z16_shade(samples.get_pixel(x, y).0[0], range)])
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

/// Encodes a 16-bit single-channel image as PNG.
fn encode_png16(image: &Luma16Image) -> Result<Vec<u8>, PublishError> {
    let mut png = Cursor::new(Vec::new());
    image
        .write_to(&mut png, ImageFormat::Png)
        .map_err(|error| PublishError::BadFrame {
            detail: format!("png encoding failed: {error}"),
        })?;
    Ok(png.into_inner())
}

/// Holds a producer's `png` frame to what `png16` serves: 16-bit
/// single-channel samples.
fn ensure_png16(bytes: &[u8]) -> Result<(), PublishError> {
    match load_image(bytes, ImageFormat::Png)? {
        DynamicImage::ImageLuma16(_) => Ok(()),
        other => Err(PublishError::BadFrame {
            detail: format!(
                "a `png` frame under `png16` holds 16-bit single-channel samples, and this one \
                 decodes as {:?}",
                other.color()
            ),
        }),
    }
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

    fn png16_policies(
        max_result_bytes: Option<u64>,
        on_oversize: Option<&str>,
    ) -> ResourcePolicies {
        let mut raw = json!({
            "freshness": { "max_age_ms": 2000 },
            "update": { "max_hz": 2.0 },
            "representation": {
                "image": "png16",
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

    /// A frame of little-endian u16 samples under `encoding`, each pixel a
    /// multiple of its index.
    fn depth_frame(encoding: &str, width: u32, height: u32) -> Value {
        let mut frame = z16_frame(
            (0..width * height).map(|index| (index * 7919) as u16),
            width,
            height,
        );
        frame["encoding"] = json!(encoding);
        frame
    }

    fn decode_snapshot_png16(value: &Value, fields: &ImageFieldMap) -> Luma16Image {
        let data = get_str(value, &fields.data, "data").expect("data field present");
        let bytes = BASE64
            .decode(data.as_bytes())
            .expect("data field is base64");
        match image::load_from_memory_with_format(&bytes, ImageFormat::Png)
            .expect("data field is a PNG")
        {
            DynamicImage::ImageLuma16(samples) => samples,
            other => panic!(
                "expected a 16-bit single-channel picture, got {:?}",
                other.color()
            ),
        }
    }

    #[test]
    fn png16_transcodes_u16_frames_losslessly() {
        for encoding in U16_ENCODINGS {
            let mut value = depth_frame(encoding, 8, 4);
            let serialized =
                apply_topic_policies(&png16_policies(None, None), &mut value).expect("transcodes");
            assert!(serialized.contains("\"encoding\":\"png\""));
            let decoded = decode_snapshot_png16(&value, &frame_fields());
            assert_eq!((decoded.width(), decoded.height()), (8, 4));
            for (index, pixel) in decoded.pixels().enumerate() {
                assert_eq!(
                    pixel.0[0],
                    (index as u32 * 7919) as u16,
                    "{encoding} pixel {index}"
                );
            }
        }
    }

    #[test]
    fn png16_passes_png_frames_through_and_refuses_color_frames() {
        let sixteen = BASE64
            .encode(encode_png16(&Luma16Image::from_pixel(1, 1, Luma([7u16]))).expect("encodes"));
        let mut png = json!({ "frame": sixteen, "encoding": "png", "width": 1, "height": 1 });
        apply_topic_policies(&png16_policies(None, None), &mut png)
            .expect("a 16-bit single-channel PNG passes through");
        assert_eq!(png["frame"], sixteen);

        let mut eight = Vec::new();
        RgbImage::from_pixel(1, 1, image::Rgb([1, 2, 3]))
            .write_to(&mut std::io::Cursor::new(&mut eight), ImageFormat::Png)
            .expect("encodes");
        let mut png = json!({
            "frame": BASE64.encode(&eight), "encoding": "png", "width": 1, "height": 1,
        });
        let error = apply_topic_policies(&png16_policies(None, None), &mut png)
            .expect_err("an 8-bit color PNG is not a depth frame");
        assert!(matches!(error, PublishError::BadFrame { .. }));

        let mut color = solid_frame("rgb8", [1, 2, 3], 2, 2);
        let error = apply_topic_policies(&png16_policies(None, None), &mut color)
            .expect_err("a color frame is not a 16-bit frame");
        assert!(
            matches!(error, PublishError::UnsupportedEncoding { encoding } if encoding == "rgb8")
        );

        let mut short = depth_frame("z16", 4, 4);
        short["frame"] = json!(BASE64.encode([0u8; 6]));
        let error = apply_topic_policies(&png16_policies(None, None), &mut short)
            .expect_err("a short buffer is a bad frame");
        assert!(matches!(error, PublishError::BadFrame { .. }));
    }

    #[test]
    fn png16_downscales_by_keeping_samples() {
        let mut value = depth_frame("z16", 64, 64);
        let full = apply_topic_policies(&png16_policies(None, None), &mut value.clone())
            .expect("transcodes");
        let limit = (full.len() / 2) as u64;
        let serialized =
            apply_topic_policies(&png16_policies(Some(limit), Some("downscale")), &mut value)
                .expect("downscales to fit");
        assert!(serialized.len() as u64 <= limit);
        assert_eq!(value["width"], json!(32));
        assert_eq!(value["height"], json!(32));
        let decoded = decode_snapshot_png16(&value, &frame_fields());
        let original = |x: u32, y: u32| ((y * 64 + x) * 7919) as u16;
        for (x, y, pixel) in decoded.enumerate_pixels() {
            let block =
                [(0, 0), (1, 0), (0, 1), (1, 1)].map(|(dx, dy)| original(x * 2 + dx, y * 2 + dy));
            assert!(
                block.contains(&pixel.0[0]),
                "sample {} at ({x}, {y}) is none of the original block {block:?}",
                pixel.0[0]
            );
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
