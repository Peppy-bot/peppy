//! The base image of a container node: the `From:` of its apptainer
//! definition, and the platforms its registry holds it for.

use crate::client::RegistryClient;
use crate::error::{RegistryError, Result};
use crate::{Access, CredentialStore};
use core_node_api::encoding::{ArtifactPlatform, OciRepository};

/// The registry of an image reference that names none.
const DOCKER_HUB: &str = "docker.io";

/// The tag of an image reference that names no tag or digest.
const DEFAULT_TAG: &str = "latest";

/// The media types of the manifests whose platforms peppy reads: image
/// indexes and lists, and single image manifests.
pub(crate) const PLATFORM_MANIFEST_MEDIA_TYPES: &[&str] = &[
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
];

/// The header of an apptainer definition file: how it bootstraps and from
/// which image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionHeader {
    /// The `Bootstrap:` agent, lowercase (`docker`, `oras`, `localimage`, ...).
    pub bootstrap: String,
    /// The `From:` value, as written. `None` when the header has none.
    pub from: Option<String>,
}

impl DefinitionHeader {
    /// Reads the header of a definition file: the `Keyword: value` lines
    /// before its first `%` section. Keywords are case insensitive, and a
    /// line that starts with `#` is a comment.
    pub fn parse(definition: &str) -> Option<Self> {
        let mut bootstrap = None;
        let mut from = None;
        for line in definition.lines() {
            let line = line.trim();
            if line.starts_with('%') {
                break;
            }
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((keyword, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match keyword.trim().to_ascii_lowercase().as_str() {
                "bootstrap" => bootstrap = Some(value.to_ascii_lowercase()),
                "from" => from = Some(value.to_owned()),
                _ => {}
            }
        }
        Some(Self {
            bootstrap: bootstrap?,
            from,
        })
    }

    /// The image a registry holds that the definition starts from: the
    /// `From:` of a `docker` or `oras` bootstrap. `None` for any other
    /// bootstrap, whose base is not in a registry.
    pub fn registry_image(&self) -> Option<Result<ImageReference>> {
        if self.bootstrap != "docker" && self.bootstrap != "oras" {
            return None;
        }
        let from = self.from.as_deref()?;
        let from = from.strip_prefix("oras://").unwrap_or(from);
        let from = from.strip_prefix("docker://").unwrap_or(from);
        Some(ImageReference::parse(from))
    }
}

/// An image in a registry: the repository and the tag or digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageReference {
    pub repository: OciRepository,
    /// A tag, or a digest `sha256:<hex>`.
    pub reference: String,
}

impl ImageReference {
    /// Parses a Docker image reference: `[registry/]path[:tag][@digest]`. A
    /// first component with a `.` or a `:`, or `localhost`, is the registry;
    /// without one the registry is Docker Hub, where a one-component path is
    /// under `library/`.
    pub fn parse(raw: &str) -> Result<Self> {
        let invalid = |detail: String| RegistryError::Protocol {
            registry: DOCKER_HUB.to_owned(),
            request: format!("the image reference `{raw}`"),
            detail,
        };
        let text = raw.trim();
        let (name, reference) = match text.split_once('@') {
            Some((name, digest)) => (name, digest.to_owned()),
            None => {
                let last_slash = text.rfind('/').map_or(0, |index| index + 1);
                match text[last_slash..].rfind(':') {
                    Some(colon) => (
                        &text[..last_slash + colon],
                        text[last_slash + colon + 1..].to_owned(),
                    ),
                    None => (text, DEFAULT_TAG.to_owned()),
                }
            }
        };
        let (registry, path) = match name.split_once('/') {
            Some((first, rest))
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (first.to_owned(), rest.to_owned())
            }
            _ if !name.contains('/') => (DOCKER_HUB.to_owned(), format!("library/{name}")),
            _ => (DOCKER_HUB.to_owned(), name.to_owned()),
        };
        let repository = OciRepository::parse(&format!("oci://{registry}/{path}"))
            .map_err(|e| invalid(e.to_string()))?;
        if reference.is_empty() {
            return Err(invalid("it names an empty tag".to_owned()));
        }
        Ok(Self {
            repository,
            reference,
        })
    }
}

impl std::fmt::Display for ImageReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let separator = if self.reference.starts_with("sha256:") {
            '@'
        } else {
            ':'
        };
        write!(
            f,
            "{}/{}{separator}{}",
            self.repository.registry(),
            self.repository.path(),
            self.reference
        )
    }
}

/// Whether the registry holds `image` for `platform`, read with the
/// credentials of `credentials`. `None` when the registry does not say: a
/// single manifest whose config names no architecture.
pub async fn image_has_platform(
    image: &ImageReference,
    platform: ArtifactPlatform,
    credentials: CredentialStore,
) -> Result<Option<bool>> {
    let client = RegistryClient::new(&image.repository, credentials, Access::Pull)?;
    let platforms = client.image_platforms(&image.reference).await?;
    if platforms.is_empty() {
        return Ok(None);
    }
    let architecture = match platform {
        ArtifactPlatform::LinuxAmd64 => "amd64",
        ArtifactPlatform::LinuxArm64 => "arm64",
    };
    Ok(Some(
        platforms
            .iter()
            .any(|(os, arch)| os == "linux" && arch == architecture),
    ))
}

/// The JSON of an image index or a manifest list, in the fields peppy reads.
#[derive(serde::Deserialize)]
pub(crate) struct IndexJson {
    #[serde(rename = "mediaType", default)]
    pub media_type: Option<String>,
    #[serde(default)]
    pub manifests: Option<Vec<IndexEntryJson>>,
    #[serde(default)]
    pub config: Option<ConfigDescriptorJson>,
}

#[derive(serde::Deserialize)]
pub(crate) struct IndexEntryJson {
    #[serde(default)]
    pub platform: Option<PlatformJson>,
}

#[derive(serde::Deserialize)]
pub(crate) struct PlatformJson {
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub architecture: String,
}

#[derive(serde::Deserialize)]
pub(crate) struct ConfigDescriptorJson {
    pub digest: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_definition_header_gives_its_bootstrap_and_base() {
        let header = DefinitionHeader::parse(
            "# a comment\nBootstrap: Docker\nFROM: peppybot/rust-cargo-base:latest\n\n%post\n  From: other\n",
        )
        .expect("header");
        assert_eq!(header.bootstrap, "docker");
        assert_eq!(
            header.from.as_deref(),
            Some("peppybot/rust-cargo-base:latest")
        );
        assert!(DefinitionHeader::parse("%post\necho\n").is_none());
        let local =
            DefinitionHeader::parse("Bootstrap: localimage\nFrom: ./base.sif\n").expect("header");
        assert!(local.registry_image().is_none());
    }

    #[test]
    fn image_references_name_their_registry_path_and_tag() {
        for (raw, registry, path, reference) in [
            ("ubuntu", "docker.io", "library/ubuntu", "latest"),
            ("ubuntu:24.04", "docker.io", "library/ubuntu", "24.04"),
            (
                "peppybot/rust-cargo-base:latest",
                "docker.io",
                "peppybot/rust-cargo-base",
                "latest",
            ),
            (
                "mirror.gcr.io/library/alpine:3.20",
                "mirror.gcr.io",
                "library/alpine",
                "3.20",
            ),
            (
                "localhost:5000/nodes/base",
                "localhost:5000",
                "nodes/base",
                "latest",
            ),
            (
                "nvcr.io/nvidia/isaac-sim@sha256:0000000000000000000000000000000000000000000000000000000000000000",
                "nvcr.io",
                "nvidia/isaac-sim",
                "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            ),
        ] {
            let image = ImageReference::parse(raw).expect(raw);
            assert_eq!(image.repository.registry(), registry, "{raw}");
            assert_eq!(image.repository.path(), path, "{raw}");
            assert_eq!(image.reference, reference, "{raw}");
        }
        let oras = DefinitionHeader::parse("Bootstrap: oras\nFrom: oras://ghcr.io/acme/base:1\n")
            .expect("header")
            .registry_image()
            .expect("a registry image")
            .expect("valid");
        assert_eq!(oras.to_string(), "ghcr.io/acme/base:1");
        assert!(ImageReference::parse("Upper/Case").is_err());
    }
}
