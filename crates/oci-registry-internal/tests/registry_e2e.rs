//! The registry client against a real CNCF distribution registry, which
//! `PEPPY_TEST_REGISTRY_BIN` names.

use core_node_api::encoding::{ArtifactPlatform, ArtifactVersion, OciRepository};
use oci_registry::manifest::{BlobDescriptor, EMPTY_CONFIG, NodeArtifact, NodeArtifactManifest};
use oci_registry::test_support::{LocalRegistry, RegistryAuth, write_stub_credential_helper};
use oci_registry::{Access, CredentialStore, RegistryClient, RegistryError, digest};
use std::assert_matches;
use std::ffi::OsString;
use std::path::Path;

const PATH: &str = "acme/peppy-nodes";

fn client(registry: &LocalRegistry, store: CredentialStore, access: Access) -> RegistryClient {
    let repository = OciRepository::parse(&registry.repository_url(PATH)).expect("repository");
    RegistryClient::new(&repository, store, access).expect("client")
}

fn store_with_file(dir: &Path, auth_file: Option<&LocalRegistry>) -> CredentialStore {
    let path = dir.join("config.json");
    if let Some(registry) = auth_file {
        registry.write_auth_file(&path);
    }
    CredentialStore::new(path, OsString::new())
}

/// Pushes a node artifact whose `.sif` is `sif` and returns its manifest.
async fn push_artifact(
    client: &RegistryClient,
    dir: &Path,
    node: &str,
    platform: ArtifactPlatform,
    sif: &[u8],
) -> NodeArtifactManifest {
    let sif_path = dir.join(format!("{node}.sif"));
    std::fs::write(&sif_path, sif).expect("write sif");
    let (sif_digest, sif_size) = digest::of_file(&sif_path).expect("hash sif");
    client
        .push_blob_file(&sif_path, &sif_digest, sif_size, &mut |_| {})
        .await
        .expect("push sif");
    let node_manifest = format!("{{ manifest: {{ name: \"{node}\", tag: \"v1\" }} }}");
    let node_manifest_digest = client
        .push_blob_bytes(node_manifest.as_bytes())
        .await
        .expect("push peppy.json5");
    client
        .push_blob_bytes(EMPTY_CONFIG)
        .await
        .expect("push empty config");
    let manifest = NodeArtifactManifest {
        artifact: NodeArtifact {
            node_name: node.to_owned(),
            node_tag: "v1".to_owned(),
            version: ArtifactVersion::parse("v0.0.0-e2e").expect("version"),
            platform,
            source_digest: digest::of_bytes(b"sources"),
            git_source: None,
        },
        sif: BlobDescriptor {
            digest: sif_digest,
            size: sif_size,
        },
        node_manifest: BlobDescriptor {
            digest: node_manifest_digest,
            size: node_manifest.len() as u64,
        },
    };
    let tag = manifest.artifact.tag().expect("tag").to_string();
    client
        .put_manifest(&tag, &manifest.to_bytes())
        .await
        .expect("put manifest");
    manifest
}

#[tokio::test(flavor = "multi_thread")]
async fn an_artifact_pushed_in_chunks_is_listed_read_and_pulled_back() {
    let registry = LocalRegistry::start(RegistryAuth::Anonymous);
    let dir = tempfile::tempdir().expect("tempdir");
    let pusher = client(&registry, store_with_file(dir.path(), None), Access::Push)
        .with_upload_chunk_bytes(1000);
    let sif: Vec<u8> = (0..4500u32).map(|i| (i % 251) as u8).collect();

    let pushed = push_artifact(
        &pusher,
        dir.path(),
        "waldo",
        ArtifactPlatform::LinuxAmd64,
        &sif,
    )
    .await;

    let puller = client(&registry, store_with_file(dir.path(), None), Access::Pull);
    assert_eq!(
        puller.list_tags().await.expect("tags"),
        ["waldo.v1.v0.0.0-e2e.linux-amd64"]
    );
    let (manifest_digest, bytes) = puller
        .manifest("waldo.v1.v0.0.0-e2e.linux-amd64")
        .await
        .expect("read manifest")
        .expect("manifest present");
    assert_eq!(manifest_digest, digest::of_bytes(&pushed.to_bytes()));
    let read = NodeArtifactManifest::parse(&bytes).expect("a node artifact");
    assert_eq!(read, pushed);
    let destination = dir.path().join("pulled").join("waldo.sif");
    puller
        .download_blob(&read.sif.digest, read.sif.size, &destination, &mut |_| {})
        .await
        .expect("pull sif");
    assert_eq!(std::fs::read(&destination).expect("read pulled"), sif);
    let node_manifest = puller
        .blob_bytes(&read.node_manifest.digest)
        .await
        .expect("read peppy.json5");
    assert!(
        String::from_utf8(node_manifest)
            .expect("utf-8")
            .contains("waldo")
    );
}

/// A chunk goes to the registry in parts, with its length in its header:
/// the registry takes it whole, and the progress moves by the parts.
#[tokio::test(flavor = "multi_thread")]
async fn a_chunk_sent_in_parts_reaches_the_registry_whole() {
    let registry = LocalRegistry::start(RegistryAuth::Anonymous);
    let dir = tempfile::tempdir().expect("tempdir");
    let pusher = client(&registry, store_with_file(dir.path(), None), Access::Push);
    let sif: Vec<u8> = (0..(1u32 << 20) + 17).map(|i| (i % 251) as u8).collect();
    let sif_path = dir.path().join("big.sif");
    std::fs::write(&sif_path, &sif).expect("write sif");
    let (sif_digest, sif_size) = digest::of_file(&sif_path).expect("hash sif");
    let mut progress = Vec::new();

    let pushed = pusher
        .push_blob_file(&sif_path, &sif_digest, sif_size, &mut |sent| {
            progress.push(sent)
        })
        .await
        .expect("push sif");

    assert!(pushed);
    assert!(progress.len() > 1, "the parts of one chunk: {progress:?}");
    assert_eq!(progress.last(), Some(&sif_size));
    let destination = dir.path().join("pulled.sif");
    client(&registry, store_with_file(dir.path(), None), Access::Pull)
        .download_blob(&sif_digest, sif_size, &destination, &mut |_| {})
        .await
        .expect("pull sif");
    assert_eq!(std::fs::read(&destination).expect("read pulled"), sif);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_tag_list_is_read_across_pages() {
    let registry = LocalRegistry::start(RegistryAuth::Anonymous);
    let dir = tempfile::tempdir().expect("tempdir");
    let pusher = client(&registry, store_with_file(dir.path(), None), Access::Push);
    for node in ["a", "b", "c", "d", "e"] {
        push_artifact(
            &pusher,
            dir.path(),
            node,
            ArtifactPlatform::LinuxArm64,
            node.as_bytes(),
        )
        .await;
    }

    let mut tags = client(&registry, store_with_file(dir.path(), None), Access::Pull)
        .with_tags_page_size(2)
        .list_tags()
        .await
        .expect("tags");

    tags.sort();
    assert_eq!(
        tags,
        ["a", "b", "c", "d", "e"].map(|node| format!("{node}.v1.v0.0.0-e2e.linux-arm64"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_nothing_was_pushed_to_has_no_tags() {
    let registry = LocalRegistry::start(RegistryAuth::Anonymous);
    let dir = tempfile::tempdir().expect("tempdir");

    let tags = client(&registry, store_with_file(dir.path(), None), Access::Pull)
        .list_tags()
        .await
        .expect("tags");

    assert!(tags.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_registry_with_basic_auth_takes_the_credential_of_the_auth_file() {
    let registry = LocalRegistry::start(RegistryAuth::Basic);
    let dir = tempfile::tempdir().expect("tempdir");
    let pusher = client(
        &registry,
        store_with_file(dir.path(), Some(&registry)),
        Access::Push,
    );

    pusher.check_push_access().await.expect("push accepted");
    push_artifact(
        &pusher,
        dir.path(),
        "waldo",
        ArtifactPlatform::LinuxAmd64,
        b"sif",
    )
    .await;

    let anonymous_dir = tempfile::tempdir().expect("tempdir");
    let anonymous = client(
        &registry,
        store_with_file(anonymous_dir.path(), None),
        Access::Pull,
    );
    let error = anonymous.list_tags().await.expect_err("no credential");
    assert_matches!(error, RegistryError::NoCredential { .. }, "{error}");
    let error = client(
        &registry,
        store_with_file(anonymous_dir.path(), None),
        Access::Push,
    )
    .check_push_access()
    .await
    .expect_err("no credential");
    assert_matches!(error, RegistryError::NoCredential { .. }, "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_registry_with_basic_auth_takes_the_credential_of_a_helper() {
    let registry = LocalRegistry::start(RegistryAuth::Basic);
    let dir = tempfile::tempdir().expect("tempdir");
    let helpers = write_stub_credential_helper(
        dir.path(),
        "stub",
        oci_registry::test_support::TEST_USERNAME,
        oci_registry::test_support::TEST_PASSWORD,
    );
    let auth_file = dir.path().join("config.json");
    std::fs::write(&auth_file, r#"{"credsStore": "stub"}"#).expect("write auth file");
    let search_path =
        std::env::join_paths([helpers, "/usr/bin".into(), "/bin".into()]).expect("join paths");
    let store = CredentialStore::new(auth_file, search_path);

    let tags = client(&registry, store, Access::Pull)
        .list_tags()
        .await
        .expect("tags");

    assert!(tags.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_password_is_refused_by_the_registry() {
    let registry = LocalRegistry::start(RegistryAuth::Basic);
    let dir = tempfile::tempdir().expect("tempdir");
    let auth_file = dir.path().join("config.json");
    std::fs::write(
        &auth_file,
        format!(
            r#"{{"auths": {{"{}": {{"username": "peppy", "password": "wrong"}}}}}}"#,
            registry.registry()
        ),
    )
    .expect("write auth file");

    let error = client(
        &registry,
        CredentialStore::new(auth_file, OsString::new()),
        Access::Pull,
    )
    .list_tags()
    .await
    .expect_err("wrong password");

    assert_matches!(error, RegistryError::Unauthorized { .. }, "{error}");
}
