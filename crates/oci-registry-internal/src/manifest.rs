//! The OCI manifest of one container node artifact: a layer with the
//! `.sif`, a layer with the `peppy.json5` of the node, and annotations that
//! state which node, version, platform and revision it is.
//!
//! The annotations are the reference when they and the tag differ.

use core_node_api::encoding::{ArtifactPlatform, ArtifactTag, ArtifactVersion, OciDigest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The media type of an OCI image manifest.
pub const IMAGE_MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// The artifact type of a container node artifact.
pub const NODE_ARTIFACT_TYPE: &str = "application/vnd.peppy.node.v1";

/// The media type of the `.sif` layer: the one apptainer gives a SIF it
/// pushes, so `apptainer pull oras://` also reads the artifact.
pub const SIF_LAYER_MEDIA_TYPE: &str = "application/vnd.sylabs.sif.layer.v1.sif";

/// The media type of the layer that holds the `peppy.json5` of the node.
pub const NODE_MANIFEST_LAYER_MEDIA_TYPE: &str = "application/vnd.peppy.node.manifest.v1+json5";

/// The media type of the empty config of an artifact that has no config.
pub const EMPTY_CONFIG_MEDIA_TYPE: &str = "application/vnd.oci.empty.v1+json";

/// The content of the empty config.
pub const EMPTY_CONFIG: &[u8] = b"{}";

/// The annotation keys of a container node artifact.
pub mod annotations {
    pub const NODE_NAME: &str = "bot.peppy.node.name";
    pub const NODE_TAG: &str = "bot.peppy.node.tag";
    pub const VERSION: &str = "bot.peppy.version";
    pub const PLATFORM: &str = "bot.peppy.platform";
    pub const SOURCE_DIGEST: &str = "bot.peppy.source-digest";
    pub const SOURCE_URL: &str = "bot.peppy.source.url";
    pub const SOURCE_COMMIT: &str = "bot.peppy.source.commit";
    pub const SOURCE_PATH: &str = "bot.peppy.source.path";
    /// The standard annotation for the file name of a layer.
    pub const TITLE: &str = "org.opencontainers.image.title";
}

/// The git revision a node was built from, for a node of a git repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSource {
    pub url: String,
    /// The full commit hash.
    pub commit: String,
    /// The repository-relative path of the `peppy.json5` of the node.
    pub path: String,
}

/// What a container node artifact is: the annotations of its manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeArtifact {
    pub node_name: String,
    pub node_tag: String,
    pub version: ArtifactVersion,
    pub platform: ArtifactPlatform,
    /// The sha256 of the staged tree of the node without the vendored
    /// peppylib: the revision of the node, the same on every platform.
    pub source_digest: OciDigest,
    pub git_source: Option<GitSource>,
}

impl NodeArtifact {
    /// The tag this artifact has in a publish repository.
    pub fn tag(&self) -> Result<ArtifactTag, core_node_api::encoding::ArtifactTagError> {
        ArtifactTag::new(
            &self.node_name,
            &self.node_tag,
            &self.version,
            self.platform,
        )
    }
}

/// A blob that a manifest refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobDescriptor {
    pub digest: OciDigest,
    pub size: u64,
}

/// The manifest of one container node artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeArtifactManifest {
    pub artifact: NodeArtifact,
    pub sif: BlobDescriptor,
    pub node_manifest: BlobDescriptor,
}

/// Why a manifest is not the manifest of a container node artifact.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    #[error("the manifest is not JSON of an OCI image manifest: {0}")]
    NotAnImageManifest(String),
    #[error("the manifest has the artifact type {0:?}, not {NODE_ARTIFACT_TYPE}")]
    NotANodeArtifact(Option<String>),
    #[error("the manifest has no layer of the media type {0}")]
    MissingLayer(&'static str),
    #[error("the manifest has {count} layers of the media type {media_type}, not one")]
    RepeatedLayer {
        media_type: &'static str,
        count: usize,
    },
    #[error("the manifest has no annotation {0}")]
    MissingAnnotation(&'static str),
    #[error("the annotation {key} of the manifest is invalid: {reason}")]
    InvalidAnnotation { key: &'static str, reason: String },
}

/// The JSON of an OCI image manifest, in the fields peppy writes and reads.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageManifestJson {
    schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    artifact_type: Option<String>,
    config: DescriptorJson,
    layers: Vec<DescriptorJson>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    annotations: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DescriptorJson {
    media_type: String,
    digest: String,
    size: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    annotations: BTreeMap<String, String>,
}

/// The digest of [`EMPTY_CONFIG`].
pub fn empty_config_digest() -> OciDigest {
    crate::digest::of_bytes(EMPTY_CONFIG)
}

impl NodeArtifactManifest {
    /// The manifest as the bytes peppy pushes. The same artifact always
    /// gives the same bytes, so the same digest.
    pub fn to_bytes(&self) -> Vec<u8> {
        let artifact = &self.artifact;
        let mut annotations = BTreeMap::from([
            (
                annotations::NODE_NAME.to_owned(),
                artifact.node_name.clone(),
            ),
            (annotations::NODE_TAG.to_owned(), artifact.node_tag.clone()),
            (
                annotations::VERSION.to_owned(),
                artifact.version.as_str().to_owned(),
            ),
            (
                annotations::PLATFORM.to_owned(),
                artifact.platform.as_str().to_owned(),
            ),
            (
                annotations::SOURCE_DIGEST.to_owned(),
                artifact.source_digest.as_str().to_owned(),
            ),
        ]);
        if let Some(git) = &artifact.git_source {
            annotations.insert(annotations::SOURCE_URL.to_owned(), git.url.clone());
            annotations.insert(annotations::SOURCE_COMMIT.to_owned(), git.commit.clone());
            annotations.insert(annotations::SOURCE_PATH.to_owned(), git.path.clone());
        }
        let title = |name: String| BTreeMap::from([(annotations::TITLE.to_owned(), name)]);
        let manifest = ImageManifestJson {
            schema_version: 2,
            media_type: Some(IMAGE_MANIFEST_MEDIA_TYPE.to_owned()),
            artifact_type: Some(NODE_ARTIFACT_TYPE.to_owned()),
            config: DescriptorJson {
                media_type: EMPTY_CONFIG_MEDIA_TYPE.to_owned(),
                digest: empty_config_digest().to_string(),
                size: EMPTY_CONFIG.len() as u64,
                annotations: BTreeMap::new(),
            },
            layers: vec![
                DescriptorJson {
                    media_type: SIF_LAYER_MEDIA_TYPE.to_owned(),
                    digest: self.sif.digest.to_string(),
                    size: self.sif.size,
                    annotations: title(format!("{}_{}.sif", artifact.node_name, artifact.node_tag)),
                },
                DescriptorJson {
                    media_type: NODE_MANIFEST_LAYER_MEDIA_TYPE.to_owned(),
                    digest: self.node_manifest.digest.to_string(),
                    size: self.node_manifest.size,
                    annotations: title(config::consts::NODE_CONFIG_FILE.to_owned()),
                },
            ],
            annotations,
        };
        serde_json::to_vec(&manifest).expect("serializing a manifest of strings cannot fail")
    }

    /// Parses the manifest of a container node artifact. Any manifest that
    /// is not one (another artifact type, a missing layer, a missing or
    /// invalid annotation) is refused, so a publish repository may hold
    /// other artifacts beside the nodes.
    pub fn parse(bytes: &[u8]) -> Result<Self, ManifestError> {
        let manifest: ImageManifestJson = serde_json::from_slice(bytes)
            .map_err(|e| ManifestError::NotAnImageManifest(e.to_string()))?;
        if manifest.schema_version != 2 {
            return Err(ManifestError::NotAnImageManifest(format!(
                "schemaVersion is {}, not 2",
                manifest.schema_version
            )));
        }
        if manifest.artifact_type.as_deref() != Some(NODE_ARTIFACT_TYPE) {
            return Err(ManifestError::NotANodeArtifact(manifest.artifact_type));
        }
        let layer = |media_type: &'static str| -> Result<BlobDescriptor, ManifestError> {
            let matching: Vec<&DescriptorJson> = manifest
                .layers
                .iter()
                .filter(|layer| layer.media_type == media_type)
                .collect();
            let [layer] = matching.as_slice() else {
                return Err(if matching.is_empty() {
                    ManifestError::MissingLayer(media_type)
                } else {
                    ManifestError::RepeatedLayer {
                        media_type,
                        count: matching.len(),
                    }
                });
            };
            let digest = OciDigest::parse(&layer.digest).map_err(|e| {
                ManifestError::NotAnImageManifest(format!("a layer of {media_type}: {e}"))
            })?;
            Ok(BlobDescriptor {
                digest,
                size: layer.size,
            })
        };
        let sif = layer(SIF_LAYER_MEDIA_TYPE)?;
        let node_manifest = layer(NODE_MANIFEST_LAYER_MEDIA_TYPE)?;
        let annotation = |key: &'static str| -> Result<&str, ManifestError> {
            manifest
                .annotations
                .get(key)
                .map(String::as_str)
                .ok_or(ManifestError::MissingAnnotation(key))
        };
        let invalid = |key: &'static str| {
            move |reason: String| ManifestError::InvalidAnnotation { key, reason }
        };
        let node_name = annotation(annotations::NODE_NAME)?;
        config::repo_node_id::validate_repo_node_name(node_name, "name")
            .map_err(invalid(annotations::NODE_NAME))?;
        let node_tag = annotation(annotations::NODE_TAG)?;
        config::repo_node_id::validate_repo_node_tag(node_tag, "tag")
            .map_err(invalid(annotations::NODE_TAG))?;
        let version = ArtifactVersion::parse(annotation(annotations::VERSION)?)
            .map_err(|e| invalid(annotations::VERSION)(e.to_string()))?;
        let platform = ArtifactPlatform::parse(annotation(annotations::PLATFORM)?)
            .map_err(|e| invalid(annotations::PLATFORM)(e.to_string()))?;
        let source_digest = OciDigest::parse(annotation(annotations::SOURCE_DIGEST)?)
            .map_err(|e| invalid(annotations::SOURCE_DIGEST)(e.to_string()))?;
        let git_source = match (
            manifest.annotations.get(annotations::SOURCE_URL),
            manifest.annotations.get(annotations::SOURCE_COMMIT),
            manifest.annotations.get(annotations::SOURCE_PATH),
        ) {
            (None, None, None) => None,
            (Some(url), Some(commit), Some(path)) => Some(GitSource {
                url: url.clone(),
                commit: commit.clone(),
                path: path.clone(),
            }),
            _ => {
                return Err(ManifestError::InvalidAnnotation {
                    key: annotations::SOURCE_URL,
                    reason: format!(
                        "{}, {} and {} come together or not at all",
                        annotations::SOURCE_URL,
                        annotations::SOURCE_COMMIT,
                        annotations::SOURCE_PATH
                    ),
                });
            }
        };
        Ok(Self {
            artifact: NodeArtifact {
                node_name: node_name.to_owned(),
                node_tag: node_tag.to_owned(),
                version,
                platform,
                source_digest,
                git_source,
            },
            sif,
            node_manifest,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    fn digest(byte: char) -> OciDigest {
        OciDigest::from_sha256_hex(&byte.to_string().repeat(64)).expect("digest")
    }

    pub(crate) fn sample(git_source: Option<GitSource>) -> NodeArtifactManifest {
        NodeArtifactManifest {
            artifact: NodeArtifact {
                node_name: "waldo".to_owned(),
                node_tag: "v1".to_owned(),
                version: ArtifactVersion::parse("v0.36.0").expect("version"),
                platform: ArtifactPlatform::LinuxArm64,
                source_digest: digest('a'),
                git_source,
            },
            sif: BlobDescriptor {
                digest: digest('b'),
                size: 1 << 30,
            },
            node_manifest: BlobDescriptor {
                digest: digest('c'),
                size: 412,
            },
        }
    }

    fn git() -> GitSource {
        GitSource {
            url: "https://github.com/acme/nodes.git".to_owned(),
            commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            path: "nodes/waldo/peppy.json5".to_owned(),
        }
    }

    #[test]
    fn a_manifest_round_trips_with_and_without_a_git_source() {
        for source in [None, Some(git())] {
            let manifest = sample(source);
            let bytes = manifest.to_bytes();
            assert_eq!(NodeArtifactManifest::parse(&bytes), Ok(manifest.clone()));
            assert_eq!(bytes, manifest.to_bytes(), "the bytes are deterministic");
        }
        assert_eq!(
            sample(None).artifact.tag().expect("tag").to_string(),
            "waldo.v1.v0.36.0.linux-arm64"
        );
    }

    #[test]
    fn a_manifest_holds_the_annotations_and_layers_of_the_layout() {
        let bytes = sample(Some(git())).to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(json["mediaType"], IMAGE_MANIFEST_MEDIA_TYPE);
        assert_eq!(json["artifactType"], NODE_ARTIFACT_TYPE);
        assert_eq!(json["config"]["mediaType"], EMPTY_CONFIG_MEDIA_TYPE);
        assert_eq!(
            json["config"]["digest"],
            "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
        assert_eq!(json["layers"][0]["mediaType"], SIF_LAYER_MEDIA_TYPE);
        assert_eq!(
            json["layers"][0]["annotations"][annotations::TITLE],
            "waldo_v1.sif"
        );
        assert_eq!(
            json["layers"][1]["annotations"][annotations::TITLE],
            "peppy.json5"
        );
        let annotations = &json["annotations"];
        assert_eq!(annotations["bot.peppy.node.name"], "waldo");
        assert_eq!(annotations["bot.peppy.node.tag"], "v1");
        assert_eq!(annotations["bot.peppy.version"], "v0.36.0");
        assert_eq!(annotations["bot.peppy.platform"], "linux/arm64");
        assert_eq!(
            annotations["bot.peppy.source-digest"],
            digest('a').to_string()
        );
        assert_eq!(annotations["bot.peppy.source.url"], git().url);
        assert_eq!(annotations["bot.peppy.source.commit"], git().commit);
        assert_eq!(annotations["bot.peppy.source.path"], git().path);
    }

    fn with_json(
        edit: impl FnOnce(&mut serde_json::Value),
    ) -> Result<NodeArtifactManifest, ManifestError> {
        let mut json: serde_json::Value =
            serde_json::from_slice(&sample(Some(git())).to_bytes()).expect("json");
        edit(&mut json);
        NodeArtifactManifest::parse(&serde_json::to_vec(&json).expect("bytes"))
    }

    #[test]
    fn manifests_of_other_artifacts_are_refused() {
        assert_matches!(
            NodeArtifactManifest::parse(b"not json"),
            Err(ManifestError::NotAnImageManifest(_))
        );
        assert_eq!(
            with_json(|json| json["artifactType"] = "application/vnd.other".into()),
            Err(ManifestError::NotANodeArtifact(Some(
                "application/vnd.other".to_owned()
            )))
        );
        assert_eq!(
            with_json(|json| {
                json.as_object_mut().expect("object").remove("artifactType");
            }),
            Err(ManifestError::NotANodeArtifact(None))
        );
    }

    #[test]
    fn a_missing_or_repeated_layer_is_refused() {
        assert_eq!(
            with_json(|json| {
                json["layers"].as_array_mut().expect("layers").remove(0);
            }),
            Err(ManifestError::MissingLayer(SIF_LAYER_MEDIA_TYPE))
        );
        assert_eq!(
            with_json(|json| {
                let layers = json["layers"].as_array_mut().expect("layers");
                layers.push(layers[1].clone());
            }),
            Err(ManifestError::RepeatedLayer {
                media_type: NODE_MANIFEST_LAYER_MEDIA_TYPE,
                count: 2
            })
        );
    }

    #[test]
    fn missing_or_invalid_annotations_are_refused() {
        assert_eq!(
            with_json(|json| {
                json["annotations"]
                    .as_object_mut()
                    .expect("annotations")
                    .remove(annotations::SOURCE_DIGEST);
            }),
            Err(ManifestError::MissingAnnotation(annotations::SOURCE_DIGEST))
        );
        for (key, value) in [
            (annotations::NODE_NAME, "wal do"),
            (annotations::NODE_TAG, "1v"),
            (annotations::VERSION, "v1/2"),
            (annotations::PLATFORM, "linux/riscv64"),
            (annotations::SOURCE_DIGEST, "sha256:abc"),
        ] {
            let result = with_json(|json| json["annotations"][key] = value.into());
            assert_matches!(result, Err(ManifestError::InvalidAnnotation { key: k, .. }) if k == key,
                "{key}: {result:?}"
            );
        }
        let partial = with_json(|json| {
            json["annotations"]
                .as_object_mut()
                .expect("annotations")
                .remove(annotations::SOURCE_COMMIT);
        });
        assert_matches!(partial, Err(ManifestError::InvalidAnnotation { .. }));
    }
}
