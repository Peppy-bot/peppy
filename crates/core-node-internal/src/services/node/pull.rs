//! The pull of the `.sif` of a node from an OCI repository, into the
//! storage of built nodes, by the daemon that runs the node.

use core_node_api::encoding::{ArtifactPlatform, OciRepository};
use daemon_config::consts::PeppyDirs;
use daemon_config::repository::OciArtifact;
use node_stack::Announcer;
use node_stack::build_io::format_bytes;
use oci_registry::manifest::NodeArtifactManifest;
use oci_registry::{Access, CredentialStore, RegistryClient};
use std::path::PathBuf;

/// How many progress lines a pull reports at most: one for each percent of
/// the image.
const PROGRESS_STEPS: u64 = 100;

/// The node an image is pulled for, and the artifact to pull.
pub(crate) struct ImageToPull<'a> {
    pub node_name: &'a str,
    pub node_tag: &'a str,
    pub repository: &'a OciRepository,
    pub artifact: &'a OciArtifact,
    pub platform: ArtifactPlatform,
}

/// Pulls the `.sif` of `image` by digest into
/// `built_nodes/<name>_<tag>/<sif sha256>.linux-<arch>.sif` and returns its
/// path. A `.sif` already there is not pulled again. The bytes go to a
/// temporary file and are checked against the digest while they arrive, so
/// a failed pull stores nothing. Each percent of the image is a progress
/// line, which counts as activity for the idle timeout of the goal.
///
/// The error names the repository, the tag, the digest and the cause. There
/// is no fall back to a build from sources.
pub(crate) async fn pull_node_image(
    peppy_dirs: &PeppyDirs,
    image: ImageToPull<'_>,
    announcer: &Announcer,
) -> Result<PathBuf, String> {
    let artifact = image.artifact;
    let failed = |cause: String| {
        format!(
            "the pull of {} ({}) from {} failed: {cause}",
            artifact.tag, artifact.sif_digest, image.repository
        )
    };
    let destination = node_stack::pulled_artifact_path(
        peppy_dirs,
        image.node_name,
        image.node_tag,
        &artifact.sif_digest,
        image.platform,
    )
    .map_err(|e| failed(e.to_string()))?;
    if std::fs::metadata(&destination).is_ok_and(|meta| meta.is_file() && meta.len() > 0) {
        announcer.line(format!(
            "Using the image of {}:{} pulled earlier: {}",
            image.node_name,
            image.node_tag,
            destination.display()
        ));
        return Ok(destination);
    }

    let client = RegistryClient::new(
        image.repository,
        CredentialStore::from_environment(),
        Access::Pull,
    )
    .map_err(|e| failed(e.to_string()))?;
    let (_, bytes) = client
        .manifest(artifact.manifest_digest.as_str())
        .await
        .map_err(|e| failed(e.to_string()))?
        .ok_or_else(|| {
            failed(format!(
                "the repository has no manifest {} any more",
                artifact.manifest_digest
            ))
        })?;
    let manifest = NodeArtifactManifest::parse(&bytes).map_err(|e| failed(e.to_string()))?;
    if manifest.sif.digest != artifact.sif_digest {
        return Err(failed(format!(
            "its manifest names the image {}, not {}",
            manifest.sif.digest, artifact.sif_digest
        )));
    }
    let size = manifest.sif.size;
    announcer.line(format!(
        "Pulling {} ({}, {}) from {}",
        artifact.tag,
        artifact.sif_digest,
        format_bytes(size),
        image.repository
    ));
    let mut reported_step = 0u64;
    let progress_announcer = announcer.clone();
    let mut on_progress = move |received: u64| {
        let step = received
            .saturating_mul(PROGRESS_STEPS)
            .checked_div(size)
            .unwrap_or(PROGRESS_STEPS);
        if step > reported_step {
            reported_step = step;
            progress_announcer.line(format!(
                "Pulled {step}% ({} of {})",
                format_bytes(received),
                format_bytes(size)
            ));
        }
    };
    client
        .download_blob(&artifact.sif_digest, size, &destination, &mut on_progress)
        .await
        .map_err(|e| failed(e.to_string()))?;

    let pruned = destination.clone();
    let platform = image.platform;
    tokio::task::spawn_blocking(move || node_stack::prune_siblings(&pruned, platform))
        .await
        .map_err(|e| failed(format!("the prune of earlier images failed: {e}")))?;
    announcer.line(format!(
        "Pulled the image of {}:{} to {}",
        image.node_name,
        image.node_tag,
        destination.display()
    ));
    Ok(destination)
}
