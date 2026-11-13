//! The `oci` repositories: OCI repositories that hold the prebuilt `.sif` of
//! container nodes, one artifact per node, peppy version and platform.
//!
//! A refresh lists the tags of the repository, keeps the artifacts of the
//! version of this daemon, and records a node when the repository has an
//! artifact of it for the platform of this daemon. The `peppy.json5` of each
//! recorded node goes to a local store, keyed by its digest, so that `repo
//! search`, `repo show`, `stack resolve` and an add read it without the
//! registry.

use crate::services::repo::cache::{DeclaredLinks, EntryOrigin};
use crate::services::repo::descriptions::CachedDescriptions;
use crate::services::repo::index::{DeclaredContent, PublishedItem};
use crate::services::repo::refresh::RepoReadFailure;
use config::node::NodeConfigParser;
use core_node_api::encoding::{
    ArtifactPlatform, ArtifactTag, ArtifactVersion, OciDigest, OciRepository, RepoRefreshFeedback,
};
use daemon_config::consts::{HOST_ARTIFACT_PLATFORM, PeppyDirs, artifact_version};
use daemon_config::repository::{ItemName, ItemTag, ManifestFingerprint, OciArtifact};
use futures::StreamExt as _;
use oci_registry::manifest::{BlobDescriptor, ManifestError, NodeArtifactManifest};
use oci_registry::{Access, CredentialStore, RegistryClient, RegistryError};
use std::collections::BTreeMap;
use std::path::PathBuf;
use tracing::{debug, warn};

/// How many manifests a refresh reads from one registry at once.
const PARALLEL_MANIFEST_READS: usize = 8;

/// The file name of a node manifest in the store.
const NODE_MANIFEST_FILE: &str = "peppy.json5";

/// What one read of a repository yields, or why it failed.
type ReadResult = Result<Vec<PublishedItem>, (RepoReadFailure, String)>;

/// Reads an `oci` repository for this daemon: its version and its
/// platform. A node the repository holds no artifact of for this platform
/// is not recorded, so a repository with a higher id serves it. A node whose
/// artifacts contradict each other is refused, and the refresh says why.
pub(crate) fn read_oci_repo(
    repository: &OciRepository,
    peppy_dirs: &PeppyDirs,
    on_feedback: &mut dyn FnMut(RepoRefreshFeedback),
) -> ReadResult {
    let unreachable = |detail: String| (RepoReadFailure::Unreachable, detail);
    let version = artifact_version().map_err(|e| unreachable(e.to_string()))?;
    on_feedback(RepoRefreshFeedback::Progress {
        message: format!("Listing the artifacts of {repository} for peppy {version}"),
    });
    // The refresh reads its repositories on threads of its own, outside the
    // runtime of the daemon, so this read runs its requests on a runtime of
    // its own.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| unreachable(format!("cannot start the runtime of the read: {e}")))?;
    let client = RegistryClient::new(
        repository,
        CredentialStore::from_environment(),
        Access::Pull,
    )
    .map_err(|e| unreachable(e.to_string()))?;
    let listed = runtime
        .block_on(list_artifacts(&client, &version))
        .map_err(|e| unreachable(format!("{repository}: {e}")))?;
    let grouped = group_node_artifacts(listed, &version, HOST_ARTIFACT_PLATFORM);
    let mut refusals = grouped.refused;
    let mut items = Vec::new();
    let published: Vec<Result<PublishedItem, String>> = runtime.block_on(
        futures::stream::iter(grouped.nodes)
            .map(|node| published_node(&client, peppy_dirs, node))
            .buffered(PARALLEL_MANIFEST_READS)
            .collect(),
    );
    for node in published {
        match node {
            Ok(item) => items.push(item),
            Err(refusal) => refusals.push(refusal),
        }
    }
    for line in grouped.skipped.into_iter().chain(refusals.iter().cloned()) {
        on_feedback(RepoRefreshFeedback::Progress {
            message: format!("{repository}: {line}"),
        });
    }
    for refusal in refusals {
        warn!("{repository}: {refusal}");
    }
    Ok(items)
}

/// One tag of the repository that names an artifact of `version`, with its
/// manifest, or why it gives none.
pub(crate) struct ListedArtifact {
    pub tag: ArtifactTag,
    pub manifest: Result<ArtifactManifest, UnreadArtifact>,
}

/// The manifest of a tag that holds a node artifact.
pub(crate) struct ArtifactManifest {
    pub digest: OciDigest,
    pub manifest: NodeArtifactManifest,
}

/// Why a tag of the repository gives no node artifact.
#[derive(Debug)]
pub(crate) enum UnreadArtifact {
    /// The registry did not give the manifest of the tag.
    ManifestUnread(RegistryError),
    /// The manifest of the tag is no node artifact.
    NotANodeArtifact(ManifestError),
}

impl std::fmt::Display for UnreadArtifact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ManifestUnread(error) => write!(f, "has a manifest that cannot be read: {error}"),
            Self::NotANodeArtifact(error) => write!(f, "holds no node artifact: {error}"),
        }
    }
}

/// Whether `error`, met on the read of the manifest of one tag, is one of
/// the whole repository: the registry is not reachable, or does not take the
/// credential of this machine. The read of every other tag would meet it
/// too.
fn fails_the_repository(error: &RegistryError) -> bool {
    matches!(
        error,
        RegistryError::Unreachable { .. }
            | RegistryError::NoCredential { .. }
            | RegistryError::Unauthorized { .. }
            | RegistryError::CredentialHelper { .. }
            | RegistryError::AuthFile(_)
    )
}

/// The tags of the repository that name an artifact of `version`, with their
/// manifests. A tag listed but gone when its manifest is read (a delete in
/// between) is left out. A tag whose manifest the registry does not give
/// refuses its node alone, unless the error is one of the whole repository
/// (see [`fails_the_repository`]), which fails the read.
async fn list_artifacts(
    client: &RegistryClient,
    version: &ArtifactVersion,
) -> Result<Vec<ListedArtifact>, RegistryError> {
    let tags: Vec<ArtifactTag> = client
        .list_tags()
        .await?
        .iter()
        .filter_map(|tag| ArtifactTag::parse(tag).ok())
        .filter(|tag| tag.version() == version)
        .collect();
    let reads: Vec<Result<Option<ListedArtifact>, RegistryError>> = futures::stream::iter(tags)
        .map(|tag| read_listed_artifact(client, tag))
        .buffer_unordered(PARALLEL_MANIFEST_READS)
        .collect()
        .await;
    let mut listed: Vec<ListedArtifact> = reads
        .into_iter()
        .filter_map(Result::transpose)
        .collect::<Result<_, _>>()?;
    // The registry lists tags in its own order; the read is deterministic.
    listed.sort_by(|a, b| a.tag.cmp(&b.tag));
    Ok(listed)
}

/// The artifact under `tag`, `None` when the tag has no manifest any more,
/// or the error of the whole repository that the read met (see
/// [`fails_the_repository`]).
async fn read_listed_artifact(
    client: &RegistryClient,
    tag: ArtifactTag,
) -> Result<Option<ListedArtifact>, RegistryError> {
    let manifest = match client.manifest(&tag.to_string()).await {
        Ok(Some((digest, bytes))) => NodeArtifactManifest::parse(&bytes)
            .map(|manifest| ArtifactManifest { digest, manifest })
            .map_err(UnreadArtifact::NotANodeArtifact),
        Ok(None) => {
            debug!("the tag {tag} has no manifest any more");
            return Ok(None);
        }
        Err(error) if fails_the_repository(&error) => return Err(error),
        Err(error) => Err(UnreadArtifact::ManifestUnread(error)),
    };
    Ok(Some(ListedArtifact { tag, manifest }))
}

/// The artifacts of one node of one revision, by platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NodeArtifacts {
    pub node_name: String,
    pub node_tag: String,
    pub source_digest: OciDigest,
    pub platforms: BTreeMap<ArtifactPlatform, OciArtifact>,
    /// The `peppy.json5` layer of the artifact of the platform of this
    /// daemon.
    pub node_manifest: BlobDescriptor,
}

/// What the artifacts of one repository give: the nodes to record, the
/// nodes refused with why, and the nodes left out because the repository
/// has no artifact of them for this platform.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct GroupedArtifacts {
    pub nodes: Vec<NodeArtifacts>,
    pub refused: Vec<String>,
    pub skipped: Vec<String>,
}

/// Groups the artifacts of `version` by node.
///
/// The annotations of a manifest are the reference when they and the tag
/// differ: an artifact whose annotations name another version is not one of
/// `version`. A node is refused when one of its tags gives no node
/// artifact, when two of its tags claim one platform, or when its platforms
/// were built from different revisions (two publishers at the same time):
/// without that refusal, a stack across an amd64 and an arm64 machine would
/// run two revisions of one node.
pub(crate) fn group_node_artifacts(
    listed: Vec<ListedArtifact>,
    version: &ArtifactVersion,
    host_platform: ArtifactPlatform,
) -> GroupedArtifacts {
    struct Candidate {
        tag: ArtifactTag,
        manifest_digest: OciDigest,
        manifest: NodeArtifactManifest,
    }
    let mut by_node: BTreeMap<(String, String), Vec<Candidate>> = BTreeMap::new();
    let mut broken: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for artifact in listed {
        let ArtifactManifest {
            digest: manifest_digest,
            manifest,
        } = match artifact.manifest {
            Ok(manifest) => manifest,
            Err(error) => {
                broken
                    .entry((
                        artifact.tag.node_name().to_owned(),
                        artifact.tag.node_tag().to_owned(),
                    ))
                    .or_default()
                    .push(format!("the tag {} {error}", artifact.tag));
                continue;
            }
        };
        if &manifest.artifact.version != version {
            debug!(
                "the tag {} holds an artifact of peppy {}",
                artifact.tag, manifest.artifact.version
            );
            continue;
        }
        by_node
            .entry((
                manifest.artifact.node_name.clone(),
                manifest.artifact.node_tag.clone(),
            ))
            .or_default()
            .push(Candidate {
                tag: artifact.tag,
                manifest_digest,
                manifest,
            });
    }

    let mut grouped = GroupedArtifacts::default();
    for ((name, tag), problems) in &broken {
        grouped
            .refused
            .push(format!("refused `{name}:{tag}`: {}", problems.join("; ")));
    }
    for ((name, tag), candidates) in by_node {
        if broken.contains_key(&(name.clone(), tag.clone())) {
            continue;
        }
        let refuse = |reason: String| format!("refused `{name}:{tag}`: {reason}");
        let mut platforms: BTreeMap<ArtifactPlatform, &Candidate> = BTreeMap::new();
        let mut problems = Vec::new();
        for candidate in &candidates {
            let platform = candidate.manifest.artifact.platform;
            if let Some(other) = platforms.insert(platform, candidate) {
                problems.push(format!(
                    "the tags {} and {} both hold its artifact for {platform}",
                    other.tag, candidate.tag
                ));
            }
        }
        let mut digests: Vec<(&OciDigest, ArtifactPlatform)> = platforms
            .iter()
            .map(|(platform, candidate)| (&candidate.manifest.artifact.source_digest, *platform))
            .collect();
        digests.dedup_by(|a, b| a.0 == b.0);
        if let [(first, first_platform), (second, second_platform), ..] = digests.as_slice() {
            problems.push(format!(
                "its artifacts were built from different revisions: source digest {first} for \
                 {first_platform} and {second} for {second_platform}. Publish one revision for \
                 every platform"
            ));
        }
        if !problems.is_empty() {
            grouped.refused.push(refuse(problems.join("; ")));
            continue;
        }
        let Some(host) = platforms.get(&host_platform) else {
            grouped.skipped.push(format!(
                "`{name}:{tag}` has no artifact for {host_platform}, the platform of this \
                 machine; a repository with a higher id serves it"
            ));
            continue;
        };
        grouped.nodes.push(NodeArtifacts {
            node_name: name.clone(),
            node_tag: tag.clone(),
            source_digest: host.manifest.artifact.source_digest.clone(),
            node_manifest: host.manifest.node_manifest.clone(),
            platforms: platforms
                .iter()
                .map(|(platform, candidate)| {
                    (
                        *platform,
                        OciArtifact {
                            tag: candidate.tag.clone(),
                            manifest_digest: candidate.manifest_digest.clone(),
                            sif_digest: candidate.manifest.sif.digest.clone(),
                        },
                    )
                })
                .collect(),
        });
    }
    grouped
}

/// The cache entry of one node: its `peppy.json5` (see
/// [`node_manifest_bytes`]) must declare the node and run it in a
/// container.
async fn published_node(
    client: &RegistryClient,
    peppy_dirs: &PeppyDirs,
    node: NodeArtifacts,
) -> Result<PublishedItem, String> {
    let label = format!("`{}:{}`", node.node_name, node.node_tag);
    let refuse = |reason: String| format!("refused {label}: {reason}");
    let bytes = node_manifest_bytes(client, peppy_dirs, &node.node_manifest.digest)
        .await
        .map_err(refuse)?;
    let content = std::str::from_utf8(&bytes)
        .map_err(|e| refuse(format!("its peppy.json5 is not UTF-8: {e}")))?;
    let parsed = NodeConfigParser::from_content(content)
        .map_err(|e| refuse(format!("its peppy.json5 does not parse: {e}")))?;
    if parsed.manifest.name.as_str() != node.node_name || parsed.manifest.tag != node.node_tag {
        return Err(refuse(format!(
            "its peppy.json5 declares `{}:{}`",
            parsed.manifest.name.as_str(),
            parsed.manifest.tag
        )));
    }
    if parsed.execution.container.is_none() {
        return Err(refuse(
            "its peppy.json5 declares a process node, and only a container node runs from an \
             artifact"
                .to_owned(),
        ));
    }
    let name = ItemName::parse(&node.node_name).map_err(|e| refuse(e.to_string()))?;
    let tag = ItemTag::parse(&node.node_tag).map_err(|e| refuse(e.to_string()))?;
    Ok(PublishedItem {
        name,
        tag: Some(tag),
        sha256: ManifestFingerprint::of_bytes(&bytes),
        origin: EntryOrigin::Oci {
            url: client.repository().clone(),
            source_digest: node.source_digest,
            platforms: node.platforms,
        },
        content: DeclaredContent::Node {
            links: DeclaredLinks::from(&parsed.manifest),
            descriptions: CachedDescriptions::of_document(&parsed.manifest.description),
        },
    })
}

/// Where the store keeps the `peppy.json5` of digest `digest`: a directory
/// of its own, which holds that file only.
pub(crate) fn node_manifest_path(peppy_dirs: &PeppyDirs, digest: &OciDigest) -> PathBuf {
    peppy_dirs
        .oci_node_manifests_dir()
        .join(digest.hex())
        .join(NODE_MANIFEST_FILE)
}

/// The path of the `peppy.json5` of fingerprint `sha256` when the store
/// holds it, without reaching the registry.
pub fn stored_node_manifest(
    peppy_dirs: &PeppyDirs,
    sha256: &ManifestFingerprint,
) -> Option<PathBuf> {
    let digest = OciDigest::from_sha256_hex(sha256.as_str()).ok()?;
    let path = node_manifest_path(peppy_dirs, &digest);
    path.is_file().then_some(path)
}

/// The `peppy.json5` of digest `digest` that the store holds, `None` when
/// it holds none or a file of another digest.
fn stored_node_manifest_bytes(peppy_dirs: &PeppyDirs, digest: &OciDigest) -> Option<Vec<u8>> {
    let bytes = std::fs::read(node_manifest_path(peppy_dirs, digest)).ok()?;
    (oci_registry::digest::of_bytes(&bytes) == *digest).then_some(bytes)
}

/// The `peppy.json5` of digest `digest`: from the store when it holds it,
/// else read from the repository of `client` into the store. Its digest
/// names its content, so a stored one is never read again.
async fn node_manifest_bytes(
    client: &RegistryClient,
    peppy_dirs: &PeppyDirs,
    digest: &OciDigest,
) -> Result<Vec<u8>, String> {
    if let Some(bytes) = stored_node_manifest_bytes(peppy_dirs, digest) {
        return Ok(bytes);
    }
    let bytes = client.blob_bytes(digest).await.map_err(|e| {
        format!(
            "its peppy.json5 {digest} cannot be read from {}: {e}",
            client.repository()
        )
    })?;
    store_node_manifest(peppy_dirs, digest, &bytes)?;
    Ok(bytes)
}

/// Writes `bytes`, the `peppy.json5` of digest `digest`, to the store.
fn store_node_manifest(
    peppy_dirs: &PeppyDirs,
    digest: &OciDigest,
    bytes: &[u8],
) -> Result<(), String> {
    let path = node_manifest_path(peppy_dirs, digest);
    let dir = path.parent().expect("a store path has a directory");
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    config::atomic_write::publish_atomic(&path, |tmp| std::fs::write(tmp, bytes))
        .map(|_| ())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// The path of the `peppy.json5` of fingerprint `sha256` in the store, read
/// from `repository` first when the store does not hold it. Blocking: run
/// it on the blocking pool inside the runtime.
pub(crate) fn ensure_node_manifest(
    peppy_dirs: &PeppyDirs,
    repository: &OciRepository,
    sha256: &ManifestFingerprint,
) -> Result<PathBuf, String> {
    let digest = OciDigest::from_sha256_hex(sha256.as_str())
        .map_err(|e| format!("the fingerprint {sha256} is no digest: {e}"))?;
    let path = node_manifest_path(peppy_dirs, &digest);
    if stored_node_manifest_bytes(peppy_dirs, &digest).is_some() {
        return Ok(path);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the runtime of the read: {e}"))?;
    let client = RegistryClient::new(
        repository,
        CredentialStore::from_environment(),
        Access::Pull,
    )
    .map_err(|e| e.to_string())?;
    runtime.block_on(node_manifest_bytes(&client, peppy_dirs, &digest))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_node_api::encoding::ArtifactPlatform::{LinuxAmd64, LinuxArm64};
    use oci_registry::manifest::NodeArtifact;

    fn version(text: &str) -> ArtifactVersion {
        ArtifactVersion::parse(text).expect("version")
    }

    fn digest(seed: &str) -> OciDigest {
        oci_registry::digest::of_bytes(seed.as_bytes())
    }

    /// The tag and manifest of an artifact of `name:v1` for `platform`,
    /// built from the revision `revision`.
    fn artifact(name: &str, platform: ArtifactPlatform, revision: &str) -> ListedArtifact {
        let v = version("v0.36.0");
        let manifest = NodeArtifactManifest {
            artifact: NodeArtifact {
                node_name: name.to_owned(),
                node_tag: "v1".to_owned(),
                version: v.clone(),
                platform,
                source_digest: digest(revision),
                git_source: None,
            },
            sif: BlobDescriptor {
                digest: digest(&format!("{name}-{revision}-{platform}-sif")),
                size: 10,
            },
            node_manifest: BlobDescriptor {
                digest: digest(&format!("{name}-{revision}-json5")),
                size: 5,
            },
        };
        ListedArtifact {
            tag: ArtifactTag::new(name, "v1", &v, platform).expect("tag"),
            manifest: Ok(ArtifactManifest {
                digest: oci_registry::digest::of_bytes(&manifest.to_bytes()),
                manifest,
            }),
        }
    }

    #[test]
    fn a_node_with_an_artifact_for_this_platform_is_recorded_with_every_platform() {
        let grouped = group_node_artifacts(
            vec![
                artifact("waldo", LinuxAmd64, "r1"),
                artifact("waldo", LinuxArm64, "r1"),
            ],
            &version("v0.36.0"),
            LinuxAmd64,
        );

        assert!(grouped.refused.is_empty(), "{:?}", grouped.refused);
        let [node] = grouped.nodes.as_slice() else {
            panic!("one node: {grouped:?}");
        };
        assert_eq!(node.node_name, "waldo");
        assert_eq!(node.source_digest, digest("r1"));
        assert_eq!(
            node.platforms.keys().copied().collect::<Vec<_>>(),
            [LinuxAmd64, LinuxArm64]
        );
        assert_eq!(
            node.platforms[&LinuxArm64].tag.to_string(),
            "waldo.v1.v0.36.0.linux-arm64"
        );
        assert_eq!(node.node_manifest.digest, digest("waldo-r1-json5"));
    }

    #[test]
    fn a_node_without_an_artifact_for_this_platform_is_left_to_the_next_repository() {
        let grouped = group_node_artifacts(
            vec![artifact("waldo", LinuxArm64, "r1")],
            &version("v0.36.0"),
            LinuxAmd64,
        );

        assert!(grouped.nodes.is_empty());
        assert!(grouped.refused.is_empty());
        let [line] = grouped.skipped.as_slice() else {
            panic!("one skipped node: {grouped:?}");
        };
        assert!(
            line.contains("`waldo:v1` has no artifact for linux/amd64"),
            "{line}"
        );
    }

    #[test]
    fn platforms_built_from_different_revisions_refuse_the_node_and_name_both_digests() {
        let grouped = group_node_artifacts(
            vec![
                artifact("waldo", LinuxAmd64, "r1"),
                artifact("waldo", LinuxArm64, "r2"),
                artifact("other", LinuxAmd64, "r9"),
            ],
            &version("v0.36.0"),
            LinuxAmd64,
        );

        assert_eq!(
            grouped
                .nodes
                .iter()
                .map(|node| node.node_name.as_str())
                .collect::<Vec<_>>(),
            ["other"]
        );
        let [refusal] = grouped.refused.as_slice() else {
            panic!("one refusal: {grouped:?}");
        };
        assert!(refusal.contains("refused `waldo:v1`"), "{refusal}");
        assert!(refusal.contains(digest("r1").as_str()), "{refusal}");
        assert!(refusal.contains(digest("r2").as_str()), "{refusal}");
    }

    #[test]
    fn annotations_of_another_version_are_not_artifacts_of_this_one() {
        let mut listed = artifact("waldo", LinuxAmd64, "r1");
        if let Ok(read) = &mut listed.manifest {
            read.manifest.artifact.version = version("v0.35.0");
        }

        let grouped = group_node_artifacts(vec![listed], &version("v0.36.0"), LinuxAmd64);

        assert_eq!(grouped, GroupedArtifacts::default());
    }

    #[test]
    fn annotations_name_the_node_and_platform_when_the_tag_differs() {
        // Pushed under the tag of `other` for arm64, its annotations say
        // `waldo` for amd64: the annotations are the reference.
        let mut listed = artifact("waldo", LinuxAmd64, "r1");
        listed.tag = ArtifactTag::new("other", "v1", &version("v0.36.0"), LinuxArm64).expect("tag");

        let grouped = group_node_artifacts(vec![listed], &version("v0.36.0"), LinuxAmd64);

        let [node] = grouped.nodes.as_slice() else {
            panic!("one node: {grouped:?}");
        };
        assert_eq!(node.node_name, "waldo");
        assert_eq!(
            node.platforms[&LinuxAmd64].tag.to_string(),
            "other.v1.v0.36.0.linux-arm64"
        );
    }

    #[test]
    fn two_tags_that_claim_one_platform_refuse_the_node() {
        let first = artifact("waldo", LinuxAmd64, "r1");
        let mut second = artifact("waldo", LinuxAmd64, "r1");
        second.tag = ArtifactTag::new("copy", "v1", &version("v0.36.0"), LinuxAmd64).expect("tag");

        let grouped = group_node_artifacts(vec![first, second], &version("v0.36.0"), LinuxAmd64);

        assert!(grouped.nodes.is_empty());
        let [refusal] = grouped.refused.as_slice() else {
            panic!("one refusal: {grouped:?}");
        };
        assert!(
            refusal.contains("both hold its artifact for linux/amd64"),
            "{refusal}"
        );
    }

    #[test]
    fn a_tag_that_holds_no_node_artifact_refuses_its_node() {
        let good = artifact("waldo", LinuxAmd64, "r1");
        let mut broken = artifact("waldo", LinuxArm64, "r1");
        broken.manifest = Err(UnreadArtifact::NotANodeArtifact(
            ManifestError::NotANodeArtifact(None),
        ));

        let grouped = group_node_artifacts(vec![good, broken], &version("v0.36.0"), LinuxAmd64);

        assert!(grouped.nodes.is_empty());
        let [refusal] = grouped.refused.as_slice() else {
            panic!("one refusal: {grouped:?}");
        };
        assert!(
            refusal.contains("waldo.v1.v0.36.0.linux-arm64 holds no node artifact"),
            "{refusal}"
        );
    }

    /// A tag whose manifest the registry does not give refuses its node, and
    /// the other nodes of the repository are recorded.
    #[test]
    fn a_tag_whose_manifest_cannot_be_read_refuses_its_node_alone() {
        let mut unread = artifact("waldo", LinuxAmd64, "r1");
        unread.manifest = Err(UnreadArtifact::ManifestUnread(RegistryError::Refused {
            registry: "ghcr.io".to_owned(),
            request: "GET manifests/waldo.v1.v0.36.0.linux-amd64".to_owned(),
            status: 500,
            detail: "internal error".to_owned(),
        }));

        let grouped = group_node_artifacts(
            vec![unread, artifact("other", LinuxAmd64, "r1")],
            &version("v0.36.0"),
            LinuxAmd64,
        );

        assert_eq!(
            grouped
                .nodes
                .iter()
                .map(|node| node.node_name.as_str())
                .collect::<Vec<_>>(),
            ["other"]
        );
        let [refusal] = grouped.refused.as_slice() else {
            panic!("one refusal: {grouped:?}");
        };
        assert!(
            refusal.contains(
                "refused `waldo:v1`: the tag waldo.v1.v0.36.0.linux-amd64 has a manifest that \
                 cannot be read"
            ),
            "{refusal}"
        );
    }

    #[test]
    fn only_the_errors_of_the_registry_or_its_credential_fail_the_repository() {
        let registry = || "ghcr.io".to_owned();
        assert!(fails_the_repository(&RegistryError::Unreachable {
            registry: registry(),
            cause: "connection refused".to_owned(),
        }));
        assert!(fails_the_repository(&RegistryError::NoCredential {
            registry: registry()
        }));
        assert!(!fails_the_repository(&RegistryError::Protocol {
            registry: registry(),
            request: "GET manifests/x".to_owned(),
            detail: "a manifest over 4 MiB".to_owned(),
        }));
    }

    /// The digest of a `peppy.json5` names its content: a refresh reads a
    /// stored one without the registry, and reads again one whose stored
    /// file has another digest.
    #[tokio::test]
    async fn a_stored_node_manifest_is_read_without_the_registry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dirs = PeppyDirs::new(dir.path());
        let bytes = b"{ manifest: { name: \"waldo\" } }";
        let digest = oci_registry::digest::of_bytes(bytes);
        store_node_manifest(&dirs, &digest, bytes).expect("store");
        let client = RegistryClient::new(
            &OciRepository::parse("oci://127.0.0.1:1/unreachable").expect("url"),
            CredentialStore::new("/nonexistent".into(), std::ffi::OsString::new()),
            Access::Pull,
        )
        .expect("client");

        assert_eq!(
            node_manifest_bytes(&client, &dirs, &digest)
                .await
                .expect("stored"),
            bytes
        );

        std::fs::write(node_manifest_path(&dirs, &digest), b"another content")
            .expect("replace the stored file");
        let error = node_manifest_bytes(&client, &dirs, &digest)
            .await
            .expect_err("the registry is unreachable");
        assert!(
            error.contains("cannot be read from oci://127.0.0.1:1/unreachable"),
            "{error}"
        );
    }

    #[test]
    fn the_store_keeps_a_node_manifest_under_its_digest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dirs = PeppyDirs::new(dir.path());
        let bytes = b"{ manifest: { name: \"waldo\" } }";
        let digest = oci_registry::digest::of_bytes(bytes);

        store_node_manifest(&dirs, &digest, bytes).expect("store");

        let path = node_manifest_path(&dirs, &digest);
        assert_eq!(std::fs::read(&path).expect("read"), bytes);
        assert_eq!(
            path.parent().expect("dir").file_name().expect("name"),
            digest.hex()
        );
        // A stored manifest is read from the store, without the registry.
        let repository = OciRepository::parse("oci://127.0.0.1:1/unreachable").expect("url");
        let fingerprint = ManifestFingerprint::of_bytes(bytes);
        assert_eq!(
            ensure_node_manifest(&dirs, &repository, &fingerprint).expect("stored"),
            path
        );
    }
}
