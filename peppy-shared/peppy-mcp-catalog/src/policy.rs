//! Operational policies shared by the `mcp_exposure/v1` document model and
//! the exposure bundle: freshness, update rate, image representation, size
//! handling, and operation kinds.

use serde::{Deserialize, Deserializer, Serialize, de};
use std::num::NonZeroU64;

/// How old a snapshot may grow before a read reports it as stale, in
/// milliseconds.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FreshnessPolicy {
    pub max_age_ms: NonZeroU64,
}

/// Cap on how often the published snapshot refreshes and notifies
/// subscribers. Messages arriving faster than this are dropped before any
/// decoding or transcoding runs.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UpdatePolicy {
    pub max_hz: MaxHz,
}

/// A positive, finite rate in hertz.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(transparent)]
pub struct MaxHz(f64);

impl MaxHz {
    /// Accepts a finite value greater than zero.
    pub fn new(value: f64) -> Result<Self, String> {
        if !value.is_finite() || value <= 0.0 {
            return Err("`max_hz` must be a finite value greater than zero".to_string());
        }
        Ok(Self(value))
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for MaxHz {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(f64::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// What the runtime does when a snapshot's final serialized content exceeds
/// `max_result_bytes`: re-encode the image small enough to fit, or report
/// the read as failed.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OversizePolicy {
    Downscale,
    Reject,
}

/// Interpret an image-carrying topic through named members of its derived
/// schema and publish it in the declared codec. Frames whose encoding
/// already matches the codec pass through without transcoding. A `jpeg`
/// representation renders colour frames (`rgb8`, `bgr8`) as they are and
/// 16-bit depth frames as the greyscale picture its `depth_range` spans; a
/// `png16` representation keeps 16-bit single-channel frames losslessly;
/// `raw` publishes the frame bytes untouched.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(try_from = "RawImageRepresentation")]
pub struct ImageRepresentation {
    pub image: ImageCodec,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<JpegQuality>,
    pub fields: ImageFieldMap,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth_range: Option<DepthRange>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawImageRepresentation {
    image: ImageCodec,
    #[serde(default)]
    quality: Option<JpegQuality>,
    fields: ImageFieldMap,
    #[serde(default)]
    depth_range: Option<DepthRange>,
}

impl TryFrom<RawImageRepresentation> for ImageRepresentation {
    type Error = String;

    /// `quality` and `depth_range` belong to `jpeg`, the one codec that
    /// encodes at a quality and renders a picture from a range: `png16` is
    /// lossless and `raw` has no encode step.
    fn try_from(raw: RawImageRepresentation) -> Result<Self, String> {
        if raw.quality.is_some() && raw.image != ImageCodec::Jpeg {
            return Err("`quality` applies only to the `jpeg` image representation".to_string());
        }
        if raw.depth_range.is_some() && raw.image != ImageCodec::Jpeg {
            return Err(
                "`depth_range` applies only to the `jpeg` image representation".to_string(),
            );
        }
        Ok(Self {
            image: raw.image,
            quality: raw.quality,
            fields: raw.fields,
            depth_range: raw.depth_range,
        })
    }
}

/// The published encoding of an image resource. `jpeg` transcodes
/// uncompressed color frames; `png16` transcodes 16-bit single-channel
/// frames (a depth map) losslessly; `raw` passes frame bytes through
/// untouched and is the explicit opt-in for uncompressed data.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ImageCodec {
    Jpeg,
    Png16,
    Raw,
}

impl ImageCodec {
    /// Whether the runtime can re-encode a frame of this codec at a smaller
    /// size, which `on_oversize: "downscale"` asks for.
    pub fn downscales(self) -> bool {
        matches!(self, Self::Jpeg | Self::Png16)
    }
}

/// JPEG quality between 1 and 100.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct JpegQuality(u8);

impl JpegQuality {
    /// Accepts a quality between 1 and 100 inclusive.
    pub fn new(value: u8) -> Result<Self, String> {
        if !(1..=100).contains(&value) {
            return Err(format!("`quality` must be between 1 and 100, got {value}"));
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u8 {
        self.0
    }
}

impl<'de> Deserialize<'de> for JpegQuality {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u8::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// The readings a `z16` depth frame's greyscale picture spans, in the
/// frame's own counts (the stream's depth unit per count). A reading at
/// `near` or nearer renders white, one at `far` or farther black, readings
/// between them in linear proportion, and 0, a pixel with no reading,
/// black. Fixed by the exposure rather than stretched per frame, so one
/// shade means one distance on every frame a client reads.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "RawDepthRange")]
pub struct DepthRange {
    near: u16,
    far: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDepthRange {
    near: u16,
    far: u16,
}

impl DepthRange {
    /// Accepts a `near` above 0, the no-reading value, and below `far`.
    pub fn new(near: u16, far: u16) -> Result<Self, String> {
        if near == 0 {
            return Err(
                "`depth_range.near` must be above 0, the value of a pixel with no reading"
                    .to_string(),
            );
        }
        if near >= far {
            return Err(format!(
                "`depth_range.near` must be below `depth_range.far`, got {near} and {far}"
            ));
        }
        Ok(Self { near, far })
    }

    pub fn near(self) -> u16 {
        self.near
    }

    pub fn far(self) -> u16 {
        self.far
    }
}

impl TryFrom<RawDepthRange> for DepthRange {
    type Error = String;

    fn try_from(raw: RawDepthRange) -> Result<Self, String> {
        Self::new(raw.near, raw.far)
    }
}

/// Which members of the derived schema carry the frame bytes, encoding
/// label, and dimensions. Validation against the contract checks that each
/// names a real member with the right type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "RawImageFieldMap")]
pub struct ImageFieldMap {
    pub data: String,
    pub encoding: String,
    pub width: String,
    pub height: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawImageFieldMap {
    data: String,
    encoding: String,
    width: String,
    height: String,
}

impl TryFrom<RawImageFieldMap> for ImageFieldMap {
    type Error = String;

    fn try_from(raw: RawImageFieldMap) -> Result<Self, String> {
        for (role, value) in [
            ("data", &raw.data),
            ("encoding", &raw.encoding),
            ("width", &raw.width),
            ("height", &raw.height),
        ] {
            if value.trim().is_empty() {
                return Err(format!("representation field `{role}` cannot be empty"));
            }
        }
        Ok(Self {
            data: raw.data,
            encoding: raw.encoding,
            width: raw.width,
            height: raw.height,
        })
    }
}

/// Whether the tool observes or changes the system. Long-running work is an
/// action, so a service is either `read_only` or `mutating`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServiceOperation {
    ReadOnly,
    Mutating,
}

/// Actions are long-running by definition; the field states it explicitly in
/// the document.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActionOperation {
    LongRunning,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_hz_accepts_positive_finite_rates() {
        let parsed: MaxHz = serde_json::from_str("2.5").expect("valid rate");
        assert_eq!(parsed.get(), 2.5);
        assert_eq!(MaxHz::new(0.25).expect("valid rate").get(), 0.25);
    }

    #[test]
    fn max_hz_rejects_zero_negative_and_non_finite_rates() {
        for raw in ["0", "-1"] {
            let error = serde_json::from_str::<MaxHz>(raw)
                .expect_err("rate should be rejected")
                .to_string();
            assert!(
                error.contains("`max_hz` must be a finite value greater than zero"),
                "unexpected error for {raw}: {error}"
            );
        }
        MaxHz::new(f64::INFINITY).expect_err("non-finite rates should be rejected");
        MaxHz::new(f64::NAN).expect_err("non-finite rates should be rejected");
    }

    #[test]
    fn jpeg_quality_accepts_the_full_range_and_rejects_outside_it() {
        assert_eq!(JpegQuality::new(1).expect("valid quality").get(), 1);
        assert_eq!(JpegQuality::new(100).expect("valid quality").get(), 100);
        for raw in ["0", "101"] {
            let error = serde_json::from_str::<JpegQuality>(raw)
                .expect_err("quality should be rejected")
                .to_string();
            assert!(
                error.contains("`quality` must be between 1 and 100"),
                "unexpected error for {raw}: {error}"
            );
        }
    }

    #[test]
    fn image_representation_accepts_quality_only_for_jpeg() {
        let fields =
            r#""fields": {"data": "frame", "encoding": "encoding", "width": "w", "height": "h"}"#;
        let jpeg: ImageRepresentation =
            serde_json::from_str(&format!(r#"{{"image": "jpeg", "quality": 80, {fields}}}"#))
                .expect("jpeg carries a quality");
        assert_eq!(jpeg.quality.map(JpegQuality::get), Some(80));
        let raw: ImageRepresentation =
            serde_json::from_str(&format!(r#"{{"image": "raw", {fields}}}"#))
                .expect("raw without a quality is the normal raw representation");
        assert_eq!(raw.quality, None);

        for codec in ["raw", "png16"] {
            let error = serde_json::from_str::<ImageRepresentation>(&format!(
                r#"{{"image": "{codec}", "quality": 80, {fields}}}"#
            ))
            .expect_err("only jpeg encodes at a quality")
            .to_string();
            assert!(
                error.contains("`quality` applies only to the `jpeg`"),
                "{codec}: unexpected error: {error}"
            );
        }
    }

    #[test]
    fn depth_range_accepts_a_near_above_zero_and_below_far() {
        let parsed: DepthRange =
            serde_json::from_str(r#"{"near": 100, "far": 10000}"#).expect("valid range");
        assert_eq!((parsed.near(), parsed.far()), (100, 10000));
        assert_eq!(parsed, DepthRange::new(100, 10000).expect("valid range"));
    }

    #[test]
    fn depth_range_rejects_a_zero_near_and_a_near_not_below_far() {
        let error = serde_json::from_str::<DepthRange>(r#"{"near": 0, "far": 10}"#)
            .expect_err("0 is the no-reading value")
            .to_string();
        assert!(
            error.contains("`depth_range.near` must be above 0"),
            "unexpected error: {error}"
        );
        for raw in [r#"{"near": 10, "far": 10}"#, r#"{"near": 11, "far": 10}"#] {
            let error = serde_json::from_str::<DepthRange>(raw)
                .expect_err("near must be below far")
                .to_string();
            assert!(
                error.contains("`depth_range.near` must be below `depth_range.far`"),
                "unexpected error for {raw}: {error}"
            );
        }
        serde_json::from_str::<DepthRange>(r#"{"near": 1, "far": 10, "unit": 0.001}"#)
            .expect_err("a range carries only its two ends");
    }

    #[test]
    fn image_representation_accepts_depth_range_only_for_jpeg() {
        let fields =
            r#""fields": {"data": "frame", "encoding": "encoding", "width": "w", "height": "h"}"#;
        let range = r#""depth_range": {"near": 100, "far": 10000}"#;
        let jpeg: ImageRepresentation =
            serde_json::from_str(&format!(r#"{{"image": "jpeg", {range}, {fields}}}"#))
                .expect("jpeg carries a depth range");
        assert_eq!(
            jpeg.depth_range,
            Some(DepthRange::new(100, 10000).expect("valid range"))
        );
        let without: ImageRepresentation =
            serde_json::from_str(&format!(r#"{{"image": "jpeg", {fields}}}"#))
                .expect("a colour-only representation declares no range");
        assert_eq!(without.depth_range, None);

        for codec in ["raw", "png16"] {
            let error = serde_json::from_str::<ImageRepresentation>(&format!(
                r#"{{"image": "{codec}", {range}, {fields}}}"#
            ))
            .expect_err("only jpeg renders a picture a range could span")
            .to_string();
            assert!(
                error.contains("`depth_range` applies only to the `jpeg`"),
                "{codec}: unexpected error: {error}"
            );
        }
    }

    #[test]
    fn image_field_map_rejects_blank_roles() {
        let error = serde_json::from_str::<ImageFieldMap>(
            r#"{"data": "frame", "encoding": " ", "width": "w", "height": "h"}"#,
        )
        .expect_err("blank role should be rejected")
        .to_string();
        assert!(error.contains("representation field `encoding` cannot be empty"));
    }

    #[test]
    fn operations_serialize_in_snake_case() {
        assert_eq!(
            serde_json::to_string(&ServiceOperation::ReadOnly).expect("serializes"),
            "\"read_only\""
        );
        assert_eq!(
            serde_json::to_string(&ActionOperation::LongRunning).expect("serializes"),
            "\"long_running\""
        );
        assert_eq!(
            serde_json::to_string(&OversizePolicy::Downscale).expect("serializes"),
            "\"downscale\""
        );
    }
}
