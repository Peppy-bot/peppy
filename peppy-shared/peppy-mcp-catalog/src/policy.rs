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
/// already matches the codec pass through without transcoding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(try_from = "RawImageRepresentation")]
pub struct ImageRepresentation {
    pub image: ImageCodec,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<JpegQuality>,
    pub fields: ImageFieldMap,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawImageRepresentation {
    image: ImageCodec,
    #[serde(default)]
    quality: Option<JpegQuality>,
    fields: ImageFieldMap,
}

impl TryFrom<RawImageRepresentation> for ImageRepresentation {
    type Error = String;

    /// `raw` publishes frame bytes untouched, so there is no encode step a
    /// quality could apply to; accepting one would silently ignore it.
    fn try_from(raw: RawImageRepresentation) -> Result<Self, String> {
        if raw.quality.is_some() && raw.image != ImageCodec::Jpeg {
            return Err("`quality` applies only to the `jpeg` image representation".to_string());
        }
        Ok(Self {
            image: raw.image,
            quality: raw.quality,
            fields: raw.fields,
        })
    }
}

/// The published encoding of an image resource. `jpeg` transcodes
/// uncompressed frames; `raw` passes frame bytes through untouched and is
/// the explicit opt-in for uncompressed data.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ImageCodec {
    Jpeg,
    Raw,
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

        let error = serde_json::from_str::<ImageRepresentation>(&format!(
            r#"{{"image": "raw", "quality": 80, {fields}}}"#
        ))
        .expect_err("a raw representation has no encode step to apply a quality to")
        .to_string();
        assert!(
            error.contains("`quality` applies only to the `jpeg`"),
            "unexpected error: {error}"
        );
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
