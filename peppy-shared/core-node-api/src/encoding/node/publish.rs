//! The publish a node build goal asks for, and what it did for each
//! platform.

use crate::encoding::{ArtifactPlatform, OciDigest, OciRepository};
use crate::node_capnp;
use crate::{Error, Result};
use std::fmt;

/// How a platform that cannot be published ends the goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishRefusals {
    /// `peppy node publish`: every platform is checked before any build,
    /// and the goal fails at the first refusal.
    FailGoal,
    /// `peppy stack build --publish`: the node builds as a build does, and
    /// the result reports each platform that was not published.
    Report,
}

/// The publish of the `.sif` of a container node that a build goal asks
/// for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodePublishRequest {
    /// The OCI repository to push to. `None`: the `publish_repository` of
    /// the daemon that builds.
    pub repository: Option<OciRepository>,
    /// The platforms to build and push beside the one of the host.
    pub extra_platforms: Vec<ArtifactPlatform>,
    pub refusals: PublishRefusals,
}

impl NodePublishRequest {
    pub(crate) fn write(
        &self,
        mut request: node_capnp::node_publish_request::Builder<'_>,
    ) -> Result<()> {
        if let Some(repository) = &self.repository {
            request.set_repository(repository.to_string());
        }
        let count = crate::encoding::capnp_list_len(
            self.extra_platforms.len(),
            "NodePublishRequest.extra_platforms",
        )?;
        let mut platforms = request.reborrow().init_extra_platforms(count);
        for (index, platform) in self.extra_platforms.iter().enumerate() {
            platforms.set(index as u32, platform.as_str());
        }
        request.set_report_refusals(self.refusals == PublishRefusals::Report);
        Ok(())
    }

    pub(crate) fn read(request: node_capnp::node_publish_request::Reader<'_>) -> Result<Self> {
        let repository = match request.get_repository()?.to_str()? {
            "" => None,
            url => Some(OciRepository::parse(url).map_err(|e| Error::Decoding(e.to_string()))?),
        };
        let extra_platforms = request
            .get_extra_platforms()?
            .iter()
            .map(|platform| {
                ArtifactPlatform::parse(platform?.to_str()?)
                    .map_err(|e| Error::Decoding(e.to_string()))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            repository,
            extra_platforms,
            refusals: if request.get_report_refusals() {
                PublishRefusals::Report
            } else {
                PublishRefusals::FailGoal
            },
        })
    }
}

/// Why a platform of a node was not published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotPublishedReason {
    BuildFailed,
    BaseImageLacksPlatform,
    EmulatorMissing,
    PublishedWithAnotherSourceDigest,
    RegistryRefused,
    RegistryUnreachable,
    NoCredential,
    /// A Python node, for a platform whose peppylib native extension this
    /// peppy does not embed.
    PeppylibMissing,
}

impl NotPublishedReason {
    /// The snake case name of the reason, which the launch result carries.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BuildFailed => "build_failed",
            Self::BaseImageLacksPlatform => "base_image_lacks_platform",
            Self::EmulatorMissing => "emulator_missing",
            Self::PublishedWithAnotherSourceDigest => "published_with_another_source_digest",
            Self::RegistryRefused => "registry_refused",
            Self::RegistryUnreachable => "registry_unreachable",
            Self::NoCredential => "no_credential",
            Self::PeppylibMissing => "peppylib_missing",
        }
    }

    /// The reason [`Self::as_str`] names.
    pub fn parse(name: &str) -> Option<Self> {
        [
            Self::BuildFailed,
            Self::BaseImageLacksPlatform,
            Self::EmulatorMissing,
            Self::PublishedWithAnotherSourceDigest,
            Self::RegistryRefused,
            Self::RegistryUnreachable,
            Self::NoCredential,
            Self::PeppylibMissing,
        ]
        .into_iter()
        .find(|reason| reason.as_str() == name)
    }

    fn to_capnp(self) -> node_capnp::NotPublishedReason {
        use node_capnp::NotPublishedReason as Wire;
        match self {
            Self::BuildFailed => Wire::BuildFailed,
            Self::BaseImageLacksPlatform => Wire::BaseImageLacksPlatform,
            Self::EmulatorMissing => Wire::EmulatorMissing,
            Self::PublishedWithAnotherSourceDigest => Wire::PublishedWithAnotherSourceDigest,
            Self::RegistryRefused => Wire::RegistryRefused,
            Self::RegistryUnreachable => Wire::RegistryUnreachable,
            Self::NoCredential => Wire::NoCredential,
            Self::PeppylibMissing => Wire::PeppylibMissing,
        }
    }

    fn from_capnp(value: node_capnp::NotPublishedReason) -> Self {
        use node_capnp::NotPublishedReason as Wire;
        match value {
            Wire::BuildFailed => Self::BuildFailed,
            Wire::BaseImageLacksPlatform => Self::BaseImageLacksPlatform,
            Wire::EmulatorMissing => Self::EmulatorMissing,
            Wire::PublishedWithAnotherSourceDigest => Self::PublishedWithAnotherSourceDigest,
            Wire::RegistryRefused => Self::RegistryRefused,
            Wire::RegistryUnreachable => Self::RegistryUnreachable,
            Wire::NoCredential => Self::NoCredential,
            Wire::PeppylibMissing => Self::PeppylibMissing,
        }
    }
}

/// What the publish of a build goal did for one platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishOutcome {
    /// The artifact was pushed under `tag`.
    Pushed {
        tag: String,
        manifest_digest: OciDigest,
    },
    /// The repository holds the artifact of this revision under `tag`
    /// already.
    AlreadyPublished { tag: String },
    /// The platform was not published, for `reason`; `detail` says what
    /// happened.
    NotPublished {
        reason: NotPublishedReason,
        detail: String,
    },
}

/// The outcome of one platform of a publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformPublish {
    pub platform: ArtifactPlatform,
    pub outcome: PublishOutcome,
}

impl PlatformPublish {
    /// Whether the platform is in the repository after the publish: pushed
    /// now, or published before.
    pub fn is_published(&self) -> bool {
        !matches!(self.outcome, PublishOutcome::NotPublished { .. })
    }

    pub(crate) fn write(&self, mut entry: node_capnp::platform_publish::Builder<'_>) {
        entry.set_platform(self.platform.as_str());
        let mut outcome = entry.init_outcome();
        match &self.outcome {
            PublishOutcome::Pushed {
                tag,
                manifest_digest,
            } => {
                let mut pushed = outcome.init_pushed();
                pushed.set_tag(tag);
                pushed.set_manifest_digest(manifest_digest.as_str());
            }
            PublishOutcome::AlreadyPublished { tag } => outcome.set_already_published(tag),
            PublishOutcome::NotPublished { reason, detail } => {
                let mut not_published = outcome.init_not_published();
                not_published.set_reason(reason.to_capnp());
                not_published.set_detail(detail);
            }
        }
    }

    pub(crate) fn read(entry: node_capnp::platform_publish::Reader<'_>) -> Result<Self> {
        use node_capnp::platform_publish::outcome::Which;
        let platform = ArtifactPlatform::parse(entry.get_platform()?.to_str()?)
            .map_err(|e| Error::Decoding(e.to_string()))?;
        let outcome = match entry.get_outcome().which()? {
            Which::Pushed(pushed) => PublishOutcome::Pushed {
                tag: pushed.get_tag()?.to_str()?.to_owned(),
                manifest_digest: OciDigest::parse(pushed.get_manifest_digest()?.to_str()?)
                    .map_err(|e| Error::Decoding(e.to_string()))?,
            },
            Which::AlreadyPublished(tag) => PublishOutcome::AlreadyPublished {
                tag: tag?.to_str()?.to_owned(),
            },
            Which::NotPublished(not_published) => PublishOutcome::NotPublished {
                reason: NotPublishedReason::from_capnp(not_published.get_reason()?),
                detail: not_published.get_detail()?.to_str()?.to_owned(),
            },
        };
        Ok(Self { platform, outcome })
    }
}

impl fmt::Display for PlatformPublish {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.outcome {
            PublishOutcome::Pushed {
                tag,
                manifest_digest,
            } => write!(f, "{}: pushed {tag} ({manifest_digest})", self.platform),
            PublishOutcome::AlreadyPublished { tag } => {
                write!(f, "{}: already published as {tag}", self.platform)
            }
            PublishOutcome::NotPublished { detail, .. } => {
                write!(f, "{}: not published: {detail}", self.platform)
            }
        }
    }
}

/// The publish that `peppy stack build --publish` asks for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StackPublish {
    /// The platforms each machine builds and pushes beside its own.
    pub extra_platforms: Vec<ArtifactPlatform>,
}

/// What `peppy stack build --publish` did for one node, or for one platform
/// of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodePublishLine {
    /// `name:tag`.
    pub node_label: String,
    /// The core node that built the node.
    pub core_node: String,
    pub outcome: NodePublishLineOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodePublishLineOutcome {
    /// What the publish did for one platform of a container node.
    Platform(PlatformPublish),
    /// A process node, which no repository holds.
    ProcessNode,
    /// A node of an oci origin, which the build pulled from this repository.
    OciOrigin(OciRepository),
}

impl NodePublishLine {
    /// Whether the line fails the build: a platform of a container node that
    /// was not published. A process node and a node of an oci origin only
    /// show in the report.
    pub fn fails_the_build(&self) -> bool {
        matches!(&self.outcome, NodePublishLineOutcome::Platform(platform) if !platform.is_published())
    }

    pub(crate) fn write(&self, mut line: crate::launch_capnp::node_publish_line::Builder<'_>) {
        line.set_node_label(&self.node_label);
        line.set_core_node(&self.core_node);
        let mut outcome = line.init_outcome();
        match &self.outcome {
            NodePublishLineOutcome::ProcessNode => outcome.set_process_node(()),
            NodePublishLineOutcome::OciOrigin(repository) => {
                outcome.set_oci_origin(repository.to_string())
            }
            NodePublishLineOutcome::Platform(PlatformPublish {
                platform,
                outcome: published,
            }) => match published {
                PublishOutcome::Pushed {
                    tag,
                    manifest_digest,
                } => {
                    let mut pushed = outcome.init_pushed();
                    pushed.set_platform(platform.as_str());
                    pushed.set_tag(tag);
                    pushed.set_manifest_digest(manifest_digest.as_str());
                }
                PublishOutcome::AlreadyPublished { tag } => {
                    let mut already = outcome.init_already_published();
                    already.set_platform(platform.as_str());
                    already.set_tag(tag);
                }
                PublishOutcome::NotPublished { reason, detail } => {
                    let mut not_published = outcome.init_not_published();
                    not_published.set_platform(platform.as_str());
                    not_published.set_reason(reason.as_str());
                    not_published.set_detail(detail);
                }
            },
        }
    }

    pub(crate) fn read(line: crate::launch_capnp::node_publish_line::Reader<'_>) -> Result<Self> {
        use crate::launch_capnp::node_publish_line::outcome::Which;
        let platform_of =
            |text: &str| ArtifactPlatform::parse(text).map_err(|e| Error::Decoding(e.to_string()));
        let outcome = match line.get_outcome().which()? {
            Which::ProcessNode(()) => NodePublishLineOutcome::ProcessNode,
            Which::OciOrigin(url) => NodePublishLineOutcome::OciOrigin(
                OciRepository::parse(url?.to_str()?).map_err(|e| Error::Decoding(e.to_string()))?,
            ),
            Which::Pushed(pushed) => NodePublishLineOutcome::Platform(PlatformPublish {
                platform: platform_of(pushed.get_platform()?.to_str()?)?,
                outcome: PublishOutcome::Pushed {
                    tag: pushed.get_tag()?.to_str()?.to_owned(),
                    manifest_digest: OciDigest::parse(pushed.get_manifest_digest()?.to_str()?)
                        .map_err(|e| Error::Decoding(e.to_string()))?,
                },
            }),
            Which::AlreadyPublished(already) => NodePublishLineOutcome::Platform(PlatformPublish {
                platform: platform_of(already.get_platform()?.to_str()?)?,
                outcome: PublishOutcome::AlreadyPublished {
                    tag: already.get_tag()?.to_str()?.to_owned(),
                },
            }),
            Which::NotPublished(not_published) => {
                let reason_text = not_published.get_reason()?.to_str()?;
                let reason = NotPublishedReason::parse(reason_text).ok_or_else(|| {
                    Error::Decoding(format!(
                        "unknown reason a platform was not published: {reason_text}"
                    ))
                })?;
                NodePublishLineOutcome::Platform(PlatformPublish {
                    platform: platform_of(not_published.get_platform()?.to_str()?)?,
                    outcome: PublishOutcome::NotPublished {
                        reason,
                        detail: not_published.get_detail()?.to_str()?.to_owned(),
                    },
                })
            }
        };
        Ok(Self {
            node_label: line.get_node_label()?.to_str()?.to_owned(),
            core_node: line.get_core_node()?.to_str()?.to_owned(),
            outcome,
        })
    }
}

impl fmt::Display for NodePublishLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let node = format!("{}@{}", self.node_label, self.core_node);
        match &self.outcome {
            NodePublishLineOutcome::Platform(platform) => write!(f, "{node} {platform}"),
            NodePublishLineOutcome::ProcessNode => {
                write!(f, "{node}: not published: process node")
            }
            NodePublishLineOutcome::OciOrigin(repository) => write!(
                f,
                "{node}: not published: comes from the oci repository {repository}"
            ),
        }
    }
}
