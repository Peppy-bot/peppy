//! The publish of the `.sif` of a container node to an OCI repository, with
//! the build of the node: the checks of `peppy node publish`, the builds of
//! the platforms beside the one of this machine, and the pushes.
//!
//! A publish works on the staged tree of a node, as a build does: it takes
//! the source digest of the tree before any build, copies the tree for each
//! other platform, builds the platform of this machine as a build does, and
//! the other platforms in a VM of their architecture.

use super::transfer_progress::announce_progress;
use config::failure_kind::PeppyFailureKind;
use config::node::NodeConfig;
use core_node_api::encoding::{
    ArtifactPlatform, ArtifactTag, ArtifactVersion, NodePublishRequest, NotPublishedReason,
    OciDigest, OciRepository, PlatformPublish, PublishOutcome, PublishRefusals,
};
use daemon_config::consts::{HOST_ARTIFACT_PLATFORM, PeppyDirs};
use daemon_config::repository::EntryOrigin;
use node_stack::Announcer;
use node_stack::build_io::format_bytes;
use oci_registry::image::DefinitionHeader;
use oci_registry::manifest::{
    BlobDescriptor, EMPTY_CONFIG, GitSource, NodeArtifact, NodeArtifactManifest,
};
use oci_registry::{Access, CredentialStore, RegistryClient, RegistryError};
use peppylib::messaging::Failure;
use std::collections::BTreeMap;
use std::path::Path;

/// The guide that says how to make a node a container node.
const CONTAINERS_GUIDE: &str = "https://docs.peppy.bot/advanced_guides/containers/";

/// What a publish knows once the checks that need no network passed: where
/// it pushes, for which version, and what it was asked.
pub(crate) struct PublishTarget {
    pub repository: OciRepository,
    pub version: ArtifactVersion,
    pub request: NodePublishRequest,
}

/// Checks 1 to 4 of `peppy node publish`, in this order, which need no
/// network: a publish repository is set (in the request, else in the config
/// of this daemon), this peppy has a release tag, the node is a container
/// node, and it does not come from an oci repository. The platforms of the
/// request (check 5) are parsed where the request is. The failure of the
/// first check that fails names its fix.
pub(crate) fn check_target(
    request: &NodePublishRequest,
    settings: &node_stack::PublishSettings,
    config: &NodeConfig,
    pulled_from: Option<&OciRepository>,
) -> Result<PublishTarget, Failure> {
    check_target_of_version(
        request,
        settings,
        config,
        pulled_from,
        daemon_config::consts::artifact_version(),
    )
}

/// [`check_target`] with the artifact version of this peppy made explicit,
/// so the order of the checks is testable in a build of any version.
fn check_target_of_version(
    request: &NodePublishRequest,
    settings: &node_stack::PublishSettings,
    config: &NodeConfig,
    pulled_from: Option<&OciRepository>,
    version: Result<ArtifactVersion, daemon_config::consts::NoArtifactVersion>,
) -> Result<PublishTarget, Failure> {
    let refuse = |message: String| Failure::new(PeppyFailureKind::PreconditionFailed, message);
    let node = node_label(config);
    let repository = match &request.repository {
        Some(repository) => repository.clone(),
        None => configured_repository(settings).map_err(refuse)?,
    };
    let version = version.map_err(|e| refuse(e.to_string()))?;
    if config.execution.container.is_none() {
        return Err(refuse(format!(
            "{node} is a process node (`build_cmd`/`run_cmd`). Only a container node can be \
             published: give it an `execution.container` block and an `apptainer.def` first. \
             See {CONTAINERS_GUIDE}"
        )));
    }
    if let Some(origin) = pulled_from {
        return Err(refuse(pulled_node_refusal(&node, origin, "publish")));
    }
    Ok(PublishTarget {
        repository,
        version,
        request: request.clone(),
    })
}

/// The publish repository of this daemon (check 1 of `peppy node
/// publish`), or the refusal that names its fix.
pub(crate) fn configured_repository(
    settings: &node_stack::PublishSettings,
) -> Result<OciRepository, String> {
    settings.repository.clone().ok_or_else(|| {
        format!(
            "no publish_repository in `{}`: set it to the URL of the OCI repository to publish \
             to, such as \"oci://ghcr.io/acme/peppy-nodes\"",
            settings.config_source
        )
    })
}

/// Why a build or a publish (`action`) refuses `node`, whose image was
/// pulled from the OCI repository `repository`: it has no sources here.
pub(crate) fn pulled_node_refusal(node: &str, repository: &OciRepository, action: &str) -> String {
    format!(
        "{node} comes from the oci repository `{repository}` and has no sources. To {action} it, \
         add a git or fs repository that holds it, with a lower id."
    )
}

/// A client that pushes to `repository` with the credentials of this
/// machine.
fn push_client(repository: &OciRepository) -> Result<RegistryClient, String> {
    RegistryClient::new(
        repository,
        CredentialStore::from_environment(),
        Access::Push,
    )
    .map_err(|e| e.to_string())
}

/// Check 6 of `peppy node publish`: the registry of `repository` accepts a
/// push from this machine, `core_node`, with its own credentials. Gives the
/// client that pushes, or the refusal that names its fix.
pub(crate) async fn check_push_from(
    repository: &OciRepository,
    core_node: &str,
) -> Result<RegistryClient, String> {
    let client = push_client(repository)?;
    client
        .check_push_access()
        .await
        .map_err(|error| push_check_failure(repository, core_node, &error))?;
    Ok(client)
}

/// A publish whose checks passed: the platforms to build and push, and what
/// the checks decided for the others.
pub(crate) struct PublishPlan {
    client: RegistryClient,
    repository: OciRepository,
    version: ArtifactVersion,
    node_name: String,
    node_tag: String,
    source_digest: OciDigest,
    git_source: Option<GitSource>,
    /// The `peppy.json5` of the staged tree, the same on every platform.
    node_manifest: Vec<u8>,
    /// Every platform of the request: the one of this machine first, then
    /// the others in the order of the request.
    platforms: Vec<ArtifactPlatform>,
    /// What the checks decided for the platforms of the request that are
    /// not to be built and pushed.
    decided: BTreeMap<ArtifactPlatform, PublishOutcome>,
    refusals: PublishRefusals,
}

impl PublishPlan {
    /// Whether the platform of this machine is to be pushed.
    pub(crate) fn pushes_host_platform(&self) -> bool {
        !self.decided.contains_key(&HOST_ARTIFACT_PLATFORM)
    }

    /// Whether the node builds for this machine: always in a stack build,
    /// which builds every node as a build does, and for `peppy node publish`
    /// only when that platform is to be pushed, since a platform published
    /// already is skipped.
    pub(crate) fn builds_host_platform(&self) -> bool {
        self.refusals == PublishRefusals::Report || self.pushes_host_platform()
    }

    /// The platforms beside the one of this machine to build and push.
    pub(crate) fn other_platforms(&self) -> Vec<ArtifactPlatform> {
        self.platforms
            .iter()
            .copied()
            .filter(|platform| {
                *platform != HOST_ARTIFACT_PLATFORM && !self.decided.contains_key(platform)
            })
            .collect()
    }
}

/// What the checks of a publish read of the node and of this machine.
pub(crate) struct PlanInputs<'a> {
    pub target: PublishTarget,
    pub config: &'a NodeConfig,
    pub repository_origin: Option<&'a EntryOrigin>,
    pub staged_dir: &'a Path,
    pub core_node: &'a str,
    pub announcer: &'a Announcer,
}

/// Checks 6 to 8 of `peppy node publish`, in this order: the registry
/// accepts a push (for `peppy node publish` only: `peppy stack build
/// --publish` checks it on every machine before any build), each other
/// platform can build here, and each platform whose tag exists holds this
/// revision. With [`PublishRefusals::FailGoal`] the first refusal fails the
/// publish before any build; with [`PublishRefusals::Report`] a refused
/// platform is reported and the others go on.
pub(crate) async fn plan(inputs: PlanInputs<'_>) -> Result<PublishPlan, Failure> {
    let PlanInputs {
        target,
        config,
        repository_origin,
        staged_dir,
        core_node,
        announcer,
    } = inputs;
    let refuse = |message: String| Failure::new(PeppyFailureKind::PreconditionFailed, message);
    let refusals = target.request.refusals;
    let node_name = config.manifest.name.as_str().to_owned();
    let node_tag = config.manifest.tag.clone();
    let mut platforms = vec![HOST_ARTIFACT_PLATFORM];
    for platform in &target.request.extra_platforms {
        if !platforms.contains(platform) {
            platforms.push(*platform);
        }
    }
    // A tag a registry refuses fails the publish whatever the mode: it is a
    // fact of the node, not of a platform or of this machine.
    let tags = platforms
        .iter()
        .map(|platform| {
            ArtifactTag::new(&node_name, &node_tag, &target.version, *platform)
                .map(|tag| (*platform, tag))
                .map_err(|e| refuse(e.to_string()))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;

    let client = match refusals {
        PublishRefusals::FailGoal => {
            announcer.line(format!(
                "Checking that {} accepts a push",
                target.repository
            ));
            check_push_from(&target.repository, core_node)
                .await
                .map_err(refuse)?
        }
        PublishRefusals::Report => push_client(&target.repository).map_err(refuse)?,
    };

    let mut decided = BTreeMap::new();
    for platform in platforms.iter().skip(1) {
        if let Some((reason, detail)) = other_platform_problem(config, staged_dir, *platform).await
        {
            refuse_platform(&mut decided, refusals, *platform, reason, detail)?;
        }
    }

    let source_digest = {
        let tree = staged_dir.to_path_buf();
        tokio::task::spawn_blocking(move || node_stack::source_digest(&tree))
            .await
            .map_err(|e| refuse(format!("the source digest task failed: {e}")))?
            .map_err(|e| {
                refuse(format!(
                    "the source digest of the staged tree {} cannot be read: {e}",
                    staged_dir.display()
                ))
            })?
    };
    announcer.line(format!(
        "Source digest of {}: {source_digest}",
        node_label(config)
    ));
    for platform in &platforms {
        if decided.contains_key(platform) {
            continue;
        }
        let tag = &tags[platform];
        match published_revision(&client, tag).await {
            Ok(None) => {}
            Ok(Some(published)) if published == source_digest => {
                decided.insert(
                    *platform,
                    PublishOutcome::AlreadyPublished {
                        tag: tag.to_string(),
                    },
                );
            }
            Ok(Some(published)) => refuse_platform(
                &mut decided,
                refusals,
                *platform,
                NotPublishedReason::PublishedWithAnotherSourceDigest,
                format!(
                    "the tag {tag} of {} holds the revision {published}, and this one is \
                     {source_digest}: a tag is immutable, so publish another tag of the node, \
                     or another peppy version",
                    target.repository
                ),
            )?,
            Err(error) => refuse_platform(
                &mut decided,
                refusals,
                *platform,
                not_published_reason(&error),
                format!("the tag {tag} cannot be read: {error}"),
            )?,
        }
    }

    let node_manifest = tokio::fs::read(staged_dir.join(config::consts::NODE_CONFIG_FILE))
        .await
        .map_err(|e| refuse(format!("the staged peppy.json5 cannot be read: {e}")))?;
    Ok(PublishPlan {
        client,
        repository: target.repository,
        version: target.version,
        node_name,
        node_tag,
        source_digest,
        git_source: repository_origin.and_then(git_source_of),
        node_manifest,
        platforms,
        decided,
        refusals,
    })
}

/// A platform that the checks refuse for `reason`: the failure of the
/// publish with [`PublishRefusals::FailGoal`], the outcome of the platform
/// with [`PublishRefusals::Report`].
fn refuse_platform(
    decided: &mut BTreeMap<ArtifactPlatform, PublishOutcome>,
    refusals: PublishRefusals,
    platform: ArtifactPlatform,
    reason: NotPublishedReason,
    detail: String,
) -> Result<(), Failure> {
    match refusals {
        PublishRefusals::FailGoal => Err(Failure::new(
            PeppyFailureKind::PreconditionFailed,
            format!("{platform}: {detail}"),
        )),
        PublishRefusals::Report => {
            decided.insert(platform, PublishOutcome::NotPublished { reason, detail });
            Ok(())
        }
    }
}

/// The revision the artifact under `tag` was built from, `None` when the
/// repository has no such tag.
async fn published_revision(
    client: &RegistryClient,
    tag: &ArtifactTag,
) -> Result<Option<OciDigest>, RegistryError> {
    let Some((_, bytes)) = client.manifest(&tag.to_string()).await? else {
        return Ok(None);
    };
    match NodeArtifactManifest::parse(&bytes) {
        Ok(manifest) => Ok(Some(manifest.artifact.source_digest)),
        Err(error) => Err(RegistryError::Protocol {
            registry: client.repository().registry().to_owned(),
            request: format!("GET manifests/{tag}"),
            detail: format!("a manifest that is no node artifact: {error}"),
        }),
    }
}

/// Why this machine cannot build the node for `platform`, another one than
/// its own, in the order the checks run: the emulator of that architecture,
/// the base image of the definition for that platform, and for a Python
/// node the peppylib native extension of that platform.
async fn other_platform_problem(
    config: &NodeConfig,
    staged_dir: &Path,
    platform: ArtifactPlatform,
) -> Option<(NotPublishedReason, String)> {
    if let Some(problem) = containers::emulation_problem(platform) {
        return Some((NotPublishedReason::EmulatorMissing, problem));
    }
    let container = config.execution.container.as_ref()?;
    let definition = tokio::fs::read_to_string(staged_dir.join(&container.def_file))
        .await
        .ok();
    if let Some(image) = definition
        .as_deref()
        .and_then(DefinitionHeader::parse)
        .and_then(|header| header.registry_image())
    {
        let image = match image {
            Ok(image) => image,
            Err(error) => {
                return Some((
                    NotPublishedReason::BaseImageLacksPlatform,
                    error.to_string(),
                ));
            }
        };
        match oci_registry::image::image_has_platform(
            &image,
            platform,
            CredentialStore::from_environment(),
        )
        .await
        {
            Ok(Some(false)) => {
                return Some((
                    NotPublishedReason::BaseImageLacksPlatform,
                    format!(
                        "the base image {image} of {} has no {platform}",
                        container.def_file
                    ),
                ));
            }
            Ok(Some(true) | None) => {}
            Err(error) => {
                return Some((
                    not_published_reason(&error),
                    format!("the base image {image} cannot be read: {error}"),
                ));
            }
        }
    }
    if config.execution.language == config::node::PeppygenLanguage::Python
        && !generator::embeds_linux_peppylib(platform)
    {
        return Some((
            NotPublishedReason::PeppylibMissing,
            format!(
                "this peppy holds no peppylib native extension for {platform}, which a Python \
                 node of that platform needs: a release of peppy holds the one of every Linux \
                 platform"
            ),
        ));
    }
    None
}

/// The copies of the staged tree that the other platforms of a publish
/// build from, made before the build of this machine, which may rewrite the
/// staged tree (the locks of a PyPI mirror). The vendored peppylib of a
/// Python node is staged again for each platform. A platform whose copy
/// fails gets that failure as its outcome. The copies run on the blocking
/// pool: a staged tree may hold hundreds of MB of vendored sources.
pub(crate) async fn copy_trees_for_other_platforms(
    plan: &PublishPlan,
    config: &NodeConfig,
    staged_dir: &Path,
    peppy_dirs: &PeppyDirs,
) -> Vec<(ArtifactPlatform, Result<tempfile::TempDir, String>)> {
    let platforms = plan.other_platforms();
    let python_codec_api = (config.execution.language == config::node::PeppygenLanguage::Python)
        .then(|| generator::CodecApi::of_manifest(config.execution.codec_api));
    let copied_platforms = platforms.clone();
    let staged_dir = staged_dir.to_path_buf();
    let peppy_dirs = peppy_dirs.clone();
    tokio::task::spawn_blocking(move || {
        copied_platforms
            .into_iter()
            .map(|platform| {
                let copy =
                    copy_tree_for_platform(&staged_dir, &peppy_dirs, platform, python_codec_api);
                (platform, copy)
            })
            .collect()
    })
    .await
    .unwrap_or_else(|error| {
        platforms
            .into_iter()
            .map(|platform| {
                (
                    platform,
                    Err(format!(
                        "the copy of the staged tree for {platform} failed: {error}"
                    )),
                )
            })
            .collect()
    })
}

/// A copy of the staged tree at `staged_dir` that builds for `platform`,
/// with the peppylib of that platform when the node is a Python node of the
/// codec API `python_codec_api`.
fn copy_tree_for_platform(
    staged_dir: &Path,
    peppy_dirs: &PeppyDirs,
    platform: ArtifactPlatform,
    python_codec_api: Option<generator::CodecApi>,
) -> Result<tempfile::TempDir, String> {
    let copy = node_stack::add_steps::copy_staged_tree(staged_dir, &peppy_dirs.tmp_dir())
        .map_err(|e| format!("the staged tree cannot be copied for {platform}: {e}"))?;
    if let Some(codec_api) = python_codec_api {
        generator::restage_peppylib_for_platform(copy.path(), peppy_dirs, platform, codec_api)
            .map_err(|e| format!("the peppylib of {platform} cannot be staged: {e}"))?;
    }
    Ok(copy)
}

/// Pushes the image at `sif_path`, built for `platform`, and gives what the
/// publish did for that platform: its blobs that the registry does not have
/// yet (the image in chunks), then its manifest under its tag.
pub(crate) async fn push_platform(
    plan: &PublishPlan,
    platform: ArtifactPlatform,
    sif_path: &Path,
    announcer: &Announcer,
) -> PlatformPublish {
    let outcome = match push_image(plan, platform, sif_path, announcer).await {
        Ok(outcome) => outcome,
        Err(error) => PublishOutcome::NotPublished {
            reason: not_published_reason(&error),
            detail: format!("the push to {} failed: {error}", plan.repository),
        },
    };
    let published = PlatformPublish { platform, outcome };
    announcer.line(published.to_string());
    published
}

async fn push_image(
    plan: &PublishPlan,
    platform: ArtifactPlatform,
    sif_path: &Path,
    announcer: &Announcer,
) -> Result<PublishOutcome, RegistryError> {
    let tag = ArtifactTag::new(&plan.node_name, &plan.node_tag, &plan.version, platform)
        .expect("the checks of the publish made every tag of the plan");
    // Another publisher may have pushed the tag since the checks: a tag is
    // immutable, so its revision decides.
    match published_revision(&plan.client, &tag).await? {
        Some(published) if published == plan.source_digest => {
            return Ok(PublishOutcome::AlreadyPublished {
                tag: tag.to_string(),
            });
        }
        Some(published) => {
            return Ok(PublishOutcome::NotPublished {
                reason: NotPublishedReason::PublishedWithAnotherSourceDigest,
                detail: format!(
                    "the tag {tag} was pushed with the revision {published} during this publish, \
                     and this one is {}",
                    plan.source_digest
                ),
            });
        }
        None => {}
    }
    let (sif_digest, sif_size) = {
        let path = sif_path.to_path_buf();
        let size = tokio::fs::metadata(sif_path)
            .await
            .map_err(|e| RegistryError::io(sif_path, e))?
            .len();
        announcer.line(format!(
            "Hashing the image of {tag} ({})",
            format_bytes(size)
        ));
        let on_progress = announce_progress("Hashed", size, announcer);
        tokio::task::spawn_blocking(move || {
            oci_registry::digest::of_file_reporting(&path, on_progress)
        })
        .await
        .map_err(|e| RegistryError::io(sif_path, std::io::Error::other(e.to_string())))?
        .map_err(|e| RegistryError::io(sif_path, e))?
    };
    announcer.line(format!(
        "Pushing {tag} ({sif_digest}, {}) to {}",
        format_bytes(sif_size),
        plan.repository
    ));
    let mut on_progress = announce_progress("Pushed", sif_size, announcer);
    let pushed = plan
        .client
        .push_blob_file(sif_path, &sif_digest, sif_size, &mut on_progress)
        .await?;
    if !pushed {
        announcer.line(format!(
            "{} has the image {sif_digest} already",
            plan.repository
        ));
    }
    let node_manifest_digest = plan.client.push_blob_bytes(&plan.node_manifest).await?;
    plan.client.push_blob_bytes(EMPTY_CONFIG).await?;
    let manifest = NodeArtifactManifest {
        artifact: NodeArtifact {
            node_name: plan.node_name.clone(),
            node_tag: plan.node_tag.clone(),
            version: plan.version.clone(),
            platform,
            source_digest: plan.source_digest.clone(),
            git_source: plan.git_source.clone(),
        },
        sif: BlobDescriptor {
            digest: sif_digest,
            size: sif_size,
        },
        node_manifest: BlobDescriptor {
            digest: node_manifest_digest,
            size: plan.node_manifest.len() as u64,
        },
    };
    let manifest_digest = plan
        .client
        .put_manifest(&tag.to_string(), &manifest.to_bytes())
        .await?;
    Ok(PublishOutcome::Pushed {
        tag: tag.to_string(),
        manifest_digest,
    })
}

/// What the publish did for each platform of its request: the platform of
/// this machine first, then the others in the order of the request.
pub(crate) fn report(plan: &PublishPlan, pushed: Vec<PlatformPublish>) -> Vec<PlatformPublish> {
    order_report(&plan.platforms, plan.decided.clone(), pushed)
}

/// The outcomes `decided` by the checks and `pushed` by the builds, in the
/// order of `platforms`. A platform with neither, which a cancel stopped
/// before its build, is left out.
fn order_report(
    platforms: &[ArtifactPlatform],
    decided: BTreeMap<ArtifactPlatform, PublishOutcome>,
    pushed: Vec<PlatformPublish>,
) -> Vec<PlatformPublish> {
    let mut by_platform = decided;
    for published in pushed {
        by_platform.insert(published.platform, published.outcome);
    }
    platforms
        .iter()
        .filter_map(|platform| {
            by_platform.remove(platform).map(|outcome| PlatformPublish {
                platform: *platform,
                outcome,
            })
        })
        .collect()
}

/// The outcome of a platform whose build failed.
pub(crate) fn build_failed(platform: ArtifactPlatform, detail: String) -> PlatformPublish {
    PlatformPublish {
        platform,
        outcome: PublishOutcome::NotPublished {
            reason: NotPublishedReason::BuildFailed,
            detail,
        },
    }
}

/// The annotations of the git revision a node of a git repository was built
/// from.
fn git_source_of(origin: &EntryOrigin) -> Option<GitSource> {
    match origin {
        EntryOrigin::Git {
            repo_url,
            commit,
            path,
            ..
        } => Some(GitSource {
            url: crate::source_url::SourceUrl::new(repo_url).to_string(),
            commit: commit.as_str().to_owned(),
            path: path.as_str().to_owned(),
        }),
        EntryOrigin::Fs { .. } | EntryOrigin::Oci { .. } => None,
    }
}

/// Why a platform was not published, from the error of the registry.
fn not_published_reason(error: &RegistryError) -> NotPublishedReason {
    match error {
        RegistryError::Unreachable { .. } => NotPublishedReason::RegistryUnreachable,
        RegistryError::NoCredential { .. } => NotPublishedReason::NoCredential,
        RegistryError::Io { .. } => NotPublishedReason::BuildFailed,
        RegistryError::Unauthorized { .. }
        | RegistryError::Refused { .. }
        | RegistryError::NotFound { .. }
        | RegistryError::DigestMismatch { .. }
        | RegistryError::SizeMismatch { .. }
        | RegistryError::Protocol { .. }
        | RegistryError::CredentialHelper { .. }
        | RegistryError::AuthFile(_) => NotPublishedReason::RegistryRefused,
    }
}

/// The message of a push check that failed on the machine `core_node`.
fn push_check_failure(
    repository: &OciRepository,
    core_node: &str,
    error: &RegistryError,
) -> String {
    match error {
        RegistryError::NoCredential { registry } => format!(
            "`{core_node}` has no credential for {registry}, which {repository} needs for a push: \
             log in with `docker login {registry}` or `apptainer registry login`"
        ),
        RegistryError::Unreachable { .. } => {
            format!("the registry of {repository} is not reachable from `{core_node}`: {error}")
        }
        _ => format!("the registry refused a push to {repository}: {error}"),
    }
}

fn node_label(config: &NodeConfig) -> String {
    format!(
        "`{}:{}`",
        config.manifest.name.as_str(),
        config.manifest.tag
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use daemon_config::consts::NoArtifactVersion;

    fn config(container: bool) -> NodeConfig {
        let execution = if container {
            r#"execution: { language: "python", container: { def_file: "apptainer.def" } }"#
        } else {
            r#"execution: { language: "rust", build_cmd: ["true"], run_cmd: ["true"] }"#
        };
        config::node::NodeConfigParser::from_content(&format!(
            r#"{{
                peppy_schema: "node/v1",
                manifest: {{ name: "waldo", tag: "v1", description: ["A node."] }},
                interfaces: {{}},
                {execution},
            }}"#
        ))
        .expect("valid manifest")
    }

    fn request() -> NodePublishRequest {
        NodePublishRequest {
            repository: None,
            extra_platforms: Vec::new(),
            refusals: PublishRefusals::FailGoal,
        }
    }

    fn settings(repository: Option<&str>) -> node_stack::PublishSettings {
        node_stack::PublishSettings {
            repository: repository.map(|url| OciRepository::parse(url).expect("url")),
            config_source: "/data/peppy/conf/peppy_config.json5".to_owned(),
        }
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "a test shorthand for a value that the code under test gives wrapped"
    )]
    fn version() -> Result<ArtifactVersion, NoArtifactVersion> {
        Ok(ArtifactVersion::parse("v0.36.0").expect("version"))
    }

    fn pulled() -> OciRepository {
        OciRepository::parse("oci://ghcr.io/acme/nodes").expect("url")
    }

    fn refusal(result: Result<PublishTarget, Failure>) -> String {
        match result {
            Ok(_) => panic!("the checks pass"),
            Err(failure) => failure.message().to_owned(),
        }
    }

    /// Every check fails here: the first one in the order of `peppy node
    /// publish` is the one the publish gives, then the next once it is fixed.
    #[test]
    fn the_checks_stop_at_the_first_failure_in_their_order() {
        let process = config(false);
        let untagged = || Err(NoArtifactVersion::Untagged);

        let first = refusal(check_target_of_version(
            &request(),
            &settings(None),
            &process,
            Some(&pulled()),
            untagged(),
        ));
        assert!(
            first.starts_with("no publish_repository in `/data/peppy/conf/peppy_config.json5`"),
            "{first}"
        );

        let second = refusal(check_target_of_version(
            &request(),
            &settings(Some("oci://ghcr.io/acme/peppy-nodes")),
            &process,
            Some(&pulled()),
            untagged(),
        ));
        assert!(second.contains("without a release tag"), "{second}");

        let third = refusal(check_target_of_version(
            &request(),
            &settings(Some("oci://ghcr.io/acme/peppy-nodes")),
            &process,
            Some(&pulled()),
            version(),
        ));
        assert_eq!(
            third,
            format!(
                "`waldo:v1` is a process node (`build_cmd`/`run_cmd`). Only a container node can \
                 be published: give it an `execution.container` block and an `apptainer.def` \
                 first. See {CONTAINERS_GUIDE}"
            )
        );

        let fourth = refusal(check_target_of_version(
            &request(),
            &settings(Some("oci://ghcr.io/acme/peppy-nodes")),
            &config(true),
            Some(&pulled()),
            version(),
        ));
        assert!(
            fourth.contains("comes from the oci repository `oci://ghcr.io/acme/nodes`"),
            "{fourth}"
        );
    }

    #[test]
    fn the_repository_of_the_request_wins_over_the_one_of_the_daemon() {
        let target = check_target_of_version(
            &NodePublishRequest {
                repository: Some(
                    OciRepository::parse("oci://coordinator/acme/nodes").expect("url"),
                ),
                ..request()
            },
            &settings(Some("oci://ghcr.io/acme/peppy-nodes")),
            &config(true),
            None,
            version(),
        )
        .unwrap_or_else(|failure| panic!("the checks pass: {failure}"));
        assert_eq!(
            target.repository.to_string(),
            "oci://coordinator/acme/nodes"
        );
        let own = check_target_of_version(
            &request(),
            &settings(Some("oci://ghcr.io/acme/peppy-nodes")),
            &config(true),
            None,
            version(),
        )
        .unwrap_or_else(|failure| panic!("the checks pass: {failure}"));
        assert_eq!(own.repository.to_string(), "oci://ghcr.io/acme/peppy-nodes");
        assert_eq!(own.version.as_str(), "v0.36.0");
    }

    fn other_platform() -> ArtifactPlatform {
        ArtifactPlatform::ALL
            .into_iter()
            .find(|platform| *platform != HOST_ARTIFACT_PLATFORM)
            .expect("two platforms")
    }

    fn already_published(tag: &str) -> PublishOutcome {
        PublishOutcome::AlreadyPublished {
            tag: tag.to_owned(),
        }
    }

    #[test]
    fn the_report_lists_the_platform_of_this_machine_first_then_the_request() {
        let other = other_platform();
        let pushed = vec![
            PlatformPublish {
                platform: other,
                outcome: already_published("other"),
            },
            PlatformPublish {
                platform: HOST_ARTIFACT_PLATFORM,
                outcome: already_published("host"),
            },
        ];
        let order: Vec<ArtifactPlatform> =
            order_report(&[HOST_ARTIFACT_PLATFORM, other], BTreeMap::new(), pushed)
                .into_iter()
                .map(|published| published.platform)
                .collect();
        assert_eq!(order, [HOST_ARTIFACT_PLATFORM, other]);
    }

    /// A platform that the checks refused or found published already is
    /// built by nobody, and is in the report all the same.
    #[test]
    fn the_report_holds_the_platforms_that_the_checks_decided() {
        let other = other_platform();
        let refused = PublishOutcome::NotPublished {
            reason: NotPublishedReason::EmulatorMissing,
            detail: "no emulator".to_owned(),
        };
        let pushed = vec![PlatformPublish {
            platform: HOST_ARTIFACT_PLATFORM,
            outcome: already_published("host"),
        }];
        let report = order_report(
            &[HOST_ARTIFACT_PLATFORM, other],
            BTreeMap::from([(other, refused.clone())]),
            pushed,
        );
        assert_eq!(
            report,
            [
                PlatformPublish {
                    platform: HOST_ARTIFACT_PLATFORM,
                    outcome: already_published("host"),
                },
                PlatformPublish {
                    platform: other,
                    outcome: refused,
                },
            ]
        );
    }
}
