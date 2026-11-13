//! The client against a mock registry: the requests it makes and how it
//! reads each answer. The end-to-end tests against the CNCF distribution
//! registry live with the daemon.

use super::*;
use httpmock::prelude::*;
use std::assert_matches;
use std::ffi::OsString;

const REPOSITORY_PATH: &str = "acme/nodes";

struct Registry {
    server: MockServer,
    _dir: tempfile::TempDir,
    auth_file: std::path::PathBuf,
}

impl Registry {
    async fn start() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_file = dir.path().join("config.json");
        Self {
            server: MockServer::start_async().await,
            _dir: dir,
            auth_file,
        }
    }

    fn registry(&self) -> String {
        format!("127.0.0.1:{}", self.server.address().port())
    }

    fn repository(&self) -> OciRepository {
        OciRepository::parse(&format!("oci://{}/{REPOSITORY_PATH}", self.registry()))
            .expect("repository")
    }

    /// Writes an auth file with `peppy:s3cret` for this registry.
    fn log_in(&self) {
        std::fs::write(
            &self.auth_file,
            format!(
                r#"{{"auths": {{"{}": {{"auth": "cGVwcHk6czNjcmV0"}}}}}}"#,
                self.registry()
            ),
        )
        .expect("write auth file");
    }

    fn client(&self, access: Access) -> RegistryClient {
        RegistryClient::new(
            &self.repository(),
            CredentialStore::new(self.auth_file.clone(), OsString::new()),
            access,
        )
        .expect("client")
    }

    fn path(&self, rest: &str) -> String {
        format!("/v2/{REPOSITORY_PATH}/{rest}")
    }
}

/// `Basic` of `peppy:s3cret`.
const BASIC_PEPPY: &str = "Basic cGVwcHk6czNjcmV0";

#[tokio::test]
async fn tag_list_follows_every_page() {
    let registry = Registry::start().await;
    let second = format!("{}?n=1000&last=b", registry.path("tags/list"));
    registry
        .server
        .mock_async(|when, then| {
            when.method(GET)
                .path(registry.path("tags/list"))
                .query_param("n", "1000")
                .is_true(|request| !request.uri().to_string().contains("last="));
            then.status(200)
                .header("Link", format!("<{second}>; rel=\"next\""))
                .json_body(serde_json::json!({"name": REPOSITORY_PATH, "tags": ["a", "b"]}));
        })
        .await;
    registry
        .server
        .mock_async(|when, then| {
            when.method(GET)
                .path(registry.path("tags/list"))
                .query_param("last", "b");
            then.status(200)
                .json_body(serde_json::json!({"name": REPOSITORY_PATH, "tags": ["c"]}));
        })
        .await;

    let tags = registry
        .client(Access::Pull)
        .list_tags()
        .await
        .expect("tags");

    assert_eq!(tags, ["a", "b", "c"]);
}

#[tokio::test]
async fn a_repository_the_registry_does_not_know_has_no_tags() {
    let registry = Registry::start().await;
    registry
        .server
        .mock_async(|when, then| {
            when.method(GET).path(registry.path("tags/list"));
            then.status(404).json_body(serde_json::json!({
                "errors": [{"code": "NAME_UNKNOWN", "message": "repository name not known"}]
            }));
        })
        .await;

    let tags = registry
        .client(Access::Pull)
        .list_tags()
        .await
        .expect("tags");

    assert!(tags.is_empty());
}

#[tokio::test]
async fn a_bearer_challenge_is_answered_with_a_token_got_with_the_credential() {
    let registry = Registry::start().await;
    registry.log_in();
    let realm = registry.server.url("/token");
    let challenge = format!(
        r#"Bearer realm="{realm}",service="{}",scope="repository:{REPOSITORY_PATH}:pull""#,
        registry.registry()
    );
    let anonymous = registry
        .server
        .mock_async(|when, then| {
            when.method(GET)
                .path(registry.path("tags/list"))
                .header_missing("authorization");
            then.status(401)
                .header("WWW-Authenticate", challenge.clone());
        })
        .await;
    let token = registry
        .server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/token")
                .query_param("scope", format!("repository:{REPOSITORY_PATH}:pull,push"))
                .query_param("service", registry.registry())
                .header("authorization", BASIC_PEPPY);
            then.status(200)
                .json_body(serde_json::json!({"token": "tok-123"}));
        })
        .await;
    let authorized = registry
        .server
        .mock_async(|when, then| {
            when.method(GET)
                .path(registry.path("tags/list"))
                .header("authorization", "Bearer tok-123");
            then.status(200)
                .json_body(serde_json::json!({"tags": ["x"]}));
        })
        .await;
    let client = registry.client(Access::Push);

    assert_eq!(client.list_tags().await.expect("tags"), ["x"]);
    // The client keeps the token for its next requests.
    assert_eq!(client.list_tags().await.expect("tags"), ["x"]);

    anonymous.assert_calls_async(1).await;
    token.assert_calls_async(1).await;
    authorized.assert_calls_async(2).await;
}

#[tokio::test]
async fn a_basic_challenge_is_answered_with_the_credential() {
    let registry = Registry::start().await;
    registry.log_in();
    registry
        .server
        .mock_async(|when, then| {
            when.method(GET)
                .path(registry.path("tags/list"))
                .header_missing("authorization");
            then.status(401)
                .header("WWW-Authenticate", r#"Basic realm="Registry Realm""#);
        })
        .await;
    registry
        .server
        .mock_async(|when, then| {
            when.method(GET)
                .path(registry.path("tags/list"))
                .header("authorization", BASIC_PEPPY);
            then.status(200).json_body(serde_json::json!({"tags": []}));
        })
        .await;

    let tags = registry
        .client(Access::Pull)
        .list_tags()
        .await
        .expect("tags");

    assert!(tags.is_empty());
}

#[tokio::test]
async fn a_registry_that_asks_for_a_credential_the_auth_file_lacks_says_so() {
    let registry = Registry::start().await;
    registry
        .server
        .mock_async(|when, then| {
            when.method(GET).path(registry.path("tags/list"));
            then.status(401)
                .header("WWW-Authenticate", r#"Basic realm="Registry Realm""#);
        })
        .await;

    let error = registry
        .client(Access::Pull)
        .list_tags()
        .await
        .expect_err("no credential");

    assert_matches!(&error, RegistryError::NoCredential { registry: r } if *r == registry.registry(),
        "{error}"
    );
    assert!(error.to_string().contains("docker login"), "{error}");
}

#[tokio::test]
async fn a_refused_credential_is_an_error_with_the_answer_of_the_registry() {
    let registry = Registry::start().await;
    registry.log_in();
    registry
        .server
        .mock_async(|when, then| {
            when.method(GET).path(registry.path("tags/list"));
            then.status(401)
                .header("WWW-Authenticate", r#"Basic realm="Registry Realm""#)
                .json_body(serde_json::json!({
                    "errors": [{"code": "UNAUTHORIZED", "message": "authentication required"}]
                }));
        })
        .await;

    let error = registry
        .client(Access::Pull)
        .list_tags()
        .await
        .expect_err("refused");

    assert_matches!(error, RegistryError::Unauthorized { .. }, "{error}");
    assert!(
        error
            .to_string()
            .contains("UNAUTHORIZED: authentication required"),
        "{error}"
    );
}

#[tokio::test]
async fn a_manifest_is_read_with_its_digest_and_a_missing_one_is_none() {
    let registry = Registry::start().await;
    let body = br#"{"schemaVersion":2}"#;
    let digest = digest::of_bytes(body);
    registry
        .server
        .mock_async(|when, then| {
            when.method(GET)
                .path(registry.path("manifests/waldo.v1.v0.36.0.linux-amd64"))
                .header("accept", IMAGE_MANIFEST_MEDIA_TYPE);
            then.status(200).body(body);
        })
        .await;
    registry
        .server
        .mock_async(|when, then| {
            when.method(GET)
                .path(registry.path(&format!("manifests/{digest}")));
            then.status(200).body(b"tampered");
        })
        .await;
    let client = registry.client(Access::Pull);

    let (got, bytes) = client
        .manifest("waldo.v1.v0.36.0.linux-amd64")
        .await
        .expect("read")
        .expect("present");
    assert_eq!(got, digest);
    assert_eq!(bytes, body);
    assert_eq!(client.manifest("absent").await.expect("read"), None);
    let error = client
        .manifest(digest.as_str())
        .await
        .expect_err("a manifest asked by digest must have it");
    assert_matches!(error, RegistryError::DigestMismatch { .. }, "{error}");
}

#[tokio::test]
async fn a_downloaded_blob_lands_only_when_its_digest_and_size_match() {
    let registry = Registry::start().await;
    let content = vec![7u8; 10_000];
    let digest = digest::of_bytes(&content);
    let tampered = digest::of_bytes(b"other");
    registry
        .server
        .mock_async(|when, then| {
            when.method(GET)
                .path(registry.path(&format!("blobs/{digest}")));
            then.status(200).body(&content);
        })
        .await;
    registry
        .server
        .mock_async(|when, then| {
            when.method(GET)
                .path(registry.path(&format!("blobs/{tampered}")));
            then.status(200).body(&content);
        })
        .await;
    let client = registry.client(Access::Pull);
    let dir = tempfile::tempdir().expect("tempdir");

    let good = dir.path().join("good.sif");
    let mut seen = Vec::new();
    client
        .download_blob(&digest, content.len() as u64, &good, &mut |bytes| {
            seen.push(bytes)
        })
        .await
        .expect("download");
    assert_eq!(std::fs::read(&good).expect("read"), content);
    assert_eq!(seen.last(), Some(&(content.len() as u64)));

    let bad = dir.path().join("bad.sif");
    let error = client
        .download_blob(&tampered, content.len() as u64, &bad, &mut |_| {})
        .await
        .expect_err("digest mismatch");
    assert_matches!(error, RegistryError::DigestMismatch { .. }, "{error}");

    let short = dir.path().join("short.sif");
    let error = client
        .download_blob(&digest, content.len() as u64 - 1, &short, &mut |_| {})
        .await
        .expect_err("more bytes than the descriptor states");
    assert_matches!(error, RegistryError::SizeMismatch { .. }, "{error}");

    // Nothing but the good blob is left: no partial file, no bad blob.
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .expect("list")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(names, ["good.sif"]);
}

#[tokio::test]
async fn a_missing_blob_is_not_found() {
    let registry = Registry::start().await;
    let digest = digest::of_bytes(b"absent");
    registry
        .server
        .mock_async(|when, then| {
            when.method(GET)
                .path(registry.path(&format!("blobs/{digest}")));
            then.status(404);
        })
        .await;
    let dir = tempfile::tempdir().expect("tempdir");

    let error = registry
        .client(Access::Pull)
        .download_blob(&digest, 6, &dir.path().join("x.sif"), &mut |_| {})
        .await
        .expect_err("absent");

    assert_matches!(error, RegistryError::NotFound { .. }, "{error}");
}

#[tokio::test]
async fn a_file_is_pushed_in_chunks_with_their_ranges_then_closed_with_its_digest() {
    let registry = Registry::start().await;
    let content: Vec<u8> = (0..25u8).collect();
    let digest = digest::of_bytes(&content);
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("node.sif");
    std::fs::write(&path, &content).expect("write");
    registry
        .server
        .mock_async(|when, then| {
            when.method(httpmock::Method::HEAD)
                .path(registry.path(&format!("blobs/{digest}")));
            then.status(404);
        })
        .await;
    registry
        .server
        .mock_async(|when, then| {
            when.method(POST).path(registry.path("blobs/uploads/"));
            then.status(202)
                .header("Location", registry.path("blobs/uploads/u1?_state=0"));
        })
        .await;
    let mut chunks = Vec::new();
    for (index, (start, end)) in [(0, 9), (10, 19), (20, 24)].into_iter().enumerate() {
        let expected = content[start..=end].to_vec();
        chunks.push(
            registry
                .server
                .mock_async(|when, then| {
                    when.method(PATCH)
                        .path(registry.path("blobs/uploads/u1"))
                        .query_param("_state", index.to_string())
                        .header("content-range", format!("{start}-{end}"))
                        .is_true(move |request| request.body_ref() == expected.as_slice());
                    then.status(202).header(
                        "Location",
                        // An absolute location is followed as well.
                        format!(
                            "{}?_state={}",
                            registry.server.url(registry.path("blobs/uploads/u1")),
                            index + 1
                        ),
                    );
                })
                .await,
        );
    }
    let close = registry
        .server
        .mock_async(|when, then| {
            when.method(PUT)
                .path(registry.path("blobs/uploads/u1"))
                .query_param("_state", "3")
                .query_param("digest", digest.as_str());
            then.status(201);
        })
        .await;
    let client = registry.client(Access::Push).with_upload_chunk_bytes(10);
    let mut progress = Vec::new();

    let pushed = client
        .push_blob_file(&path, &digest, content.len() as u64, &mut |sent| {
            progress.push(sent)
        })
        .await
        .expect("push");

    assert!(pushed);
    assert_eq!(progress, [10, 20, 25]);
    for chunk in &chunks {
        chunk.assert_calls_async(1).await;
    }
    close.assert_calls_async(1).await;
}

/// A chunk goes to the connection in parts, each counted as sent, with the
/// length of the chunk in its header.
#[tokio::test]
async fn the_progress_of_a_push_moves_by_the_parts_of_a_chunk() {
    let registry = Registry::start().await;
    let content: Vec<u8> = Vec::from_fn(2 * UPLOAD_PART_BYTES + 100, |i| i as u8);
    let digest = digest::of_bytes(&content);
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("node.sif");
    std::fs::write(&path, &content).expect("write");
    registry
        .server
        .mock_async(|when, then| {
            when.method(httpmock::Method::HEAD)
                .path(registry.path(&format!("blobs/{digest}")));
            then.status(404);
        })
        .await;
    registry
        .server
        .mock_async(|when, then| {
            when.method(POST).path(registry.path("blobs/uploads/"));
            then.status(202)
                .header("Location", registry.path("blobs/uploads/u1"));
        })
        .await;
    let expected = content.clone();
    let chunk = registry
        .server
        .mock_async(|when, then| {
            when.method(PATCH)
                .path(registry.path("blobs/uploads/u1"))
                .header("content-length", content.len().to_string())
                .is_true(move |request| request.body_ref() == expected.as_slice());
            then.status(202)
                .header("Location", registry.path("blobs/uploads/u2"));
        })
        .await;
    registry
        .server
        .mock_async(|when, then| {
            when.method(PUT).path(registry.path("blobs/uploads/u2"));
            then.status(201);
        })
        .await;
    let mut progress = Vec::new();

    registry
        .client(Access::Push)
        .push_blob_file(&path, &digest, content.len() as u64, &mut |sent| {
            progress.push(sent)
        })
        .await
        .expect("push");

    let part = UPLOAD_PART_BYTES as u64;
    assert_eq!(progress, [part, 2 * part, 2 * part + 100]);
    chunk.assert_calls_async(1).await;
}

#[tokio::test]
async fn a_blob_the_registry_has_is_not_pushed_again() {
    let registry = Registry::start().await;
    let digest = digest::of_bytes(b"present");
    registry
        .server
        .mock_async(|when, then| {
            when.method(httpmock::Method::HEAD)
                .path(registry.path(&format!("blobs/{digest}")));
            then.status(200);
        })
        .await;
    let upload = registry
        .server
        .mock_async(|when, then| {
            when.method(POST).path(registry.path("blobs/uploads/"));
            then.status(202);
        })
        .await;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("present");
    std::fs::write(&path, b"present").expect("write");

    let pushed = registry
        .client(Access::Push)
        .push_blob_file(&path, &digest, 7, &mut |_| {})
        .await
        .expect("push");

    assert!(!pushed);
    upload.assert_calls_async(0).await;
}

#[tokio::test]
async fn the_push_check_opens_an_upload_and_cancels_it() {
    let registry = Registry::start().await;
    registry
        .server
        .mock_async(|when, then| {
            when.method(POST).path(registry.path("blobs/uploads/"));
            then.status(202)
                .header("Location", registry.path("blobs/uploads/check"));
        })
        .await;
    let cancel = registry
        .server
        .mock_async(|when, then| {
            when.method(DELETE)
                .path(registry.path("blobs/uploads/check"));
            then.status(204);
        })
        .await;

    registry
        .client(Access::Push)
        .check_push_access()
        .await
        .expect("push accepted");

    cancel.assert_calls_async(1).await;
}

#[tokio::test]
async fn a_registry_that_refuses_the_push_check_answers_with_its_reason() {
    let registry = Registry::start().await;
    registry
        .server
        .mock_async(|when, then| {
            when.method(POST).path(registry.path("blobs/uploads/"));
            then.status(403).json_body(serde_json::json!({
                "errors": [{"code": "DENIED", "message": "requested access to the resource is denied"}]
            }));
        })
        .await;

    let error = registry
        .client(Access::Push)
        .check_push_access()
        .await
        .expect_err("denied");

    let message = error.to_string();
    assert!(message.contains("403"), "{message}");
    assert!(message.contains("DENIED: requested access"), "{message}");
}

#[tokio::test]
async fn a_manifest_is_put_under_its_tag_with_the_image_manifest_media_type() {
    let registry = Registry::start().await;
    let body = br#"{"schemaVersion":2}"#.to_vec();
    let put = registry
        .server
        .mock_async(|when, then| {
            when.method(PUT)
                .path(registry.path("manifests/waldo.v1.v0.36.0.linux-amd64"))
                .header("content-type", IMAGE_MANIFEST_MEDIA_TYPE)
                .body(String::from_utf8(body.clone()).expect("utf-8"));
            then.status(201);
        })
        .await;

    let digest = registry
        .client(Access::Push)
        .put_manifest("waldo.v1.v0.36.0.linux-amd64", &body)
        .await
        .expect("put");

    assert_eq!(digest, digest::of_bytes(&body));
    put.assert_calls_async(1).await;
}

#[tokio::test]
async fn an_unreachable_registry_names_itself_and_the_cause() {
    // Bind and drop a listener so the port refuses connections.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("address")
        .port();
    let repository =
        OciRepository::parse(&format!("oci://127.0.0.1:{port}/acme/nodes")).expect("repository");
    let client = RegistryClient::new(
        &repository,
        CredentialStore::new("/nonexistent".into(), OsString::new()),
        Access::Pull,
    )
    .expect("client");

    let error = client.list_tags().await.expect_err("unreachable");

    assert_matches!(error, RegistryError::Unreachable { .. }, "{error}");
    assert!(
        error.to_string().contains(&format!("127.0.0.1:{port}")),
        "{error}"
    );
}

#[test]
fn the_next_page_comes_from_a_relative_or_absolute_link() {
    let current = "http://127.0.0.1:5000/v2/acme/nodes/tags/list?n=1000";
    let mut headers = HeaderMap::new();
    headers.insert(
        header::LINK,
        HeaderValue::from_static("</v2/acme/nodes/tags/list?n=1000&last=z>; rel=\"next\""),
    );
    assert_eq!(
        next_page(&headers, current).as_deref(),
        Some("http://127.0.0.1:5000/v2/acme/nodes/tags/list?n=1000&last=z")
    );
    headers.insert(
        header::LINK,
        HeaderValue::from_static("<https://other.example/x>; rel=\"prev\""),
    );
    assert_eq!(next_page(&headers, current), None);
    assert_eq!(next_page(&HeaderMap::new(), current), None);
}

#[tokio::test]
async fn the_platforms_of_an_index_leave_out_attestations() {
    let registry = Registry::start().await;
    registry
        .server
        .mock_async(|when, then| {
            when.method(GET)
                .path(registry.path("manifests/latest"))
                .is_true(|request| {
                    request
                        .headers()
                        .get("accept")
                        .and_then(|value| value.to_str().ok())
                        .is_some_and(|accept| accept.contains("image.index.v1+json"))
                });
            then.status(200).json_body(serde_json::json!({
                "mediaType": "application/vnd.oci.image.index.v1+json",
                "manifests": [
                    {"platform": {"os": "linux", "architecture": "amd64"}},
                    {"platform": {"os": "unknown", "architecture": "unknown"}},
                    {"platform": {"os": "linux", "architecture": "arm64", "variant": "v8"}}
                ]
            }));
        })
        .await;

    let platforms = registry
        .client(Access::Pull)
        .image_platforms("latest")
        .await
        .expect("platforms");

    assert_eq!(
        platforms,
        [
            ("linux".to_owned(), "amd64".to_owned()),
            ("linux".to_owned(), "arm64".to_owned())
        ]
    );
}

#[tokio::test]
async fn the_platform_of_a_single_manifest_is_the_one_of_its_config() {
    let registry = Registry::start().await;
    let config = br#"{"os": "linux", "architecture": "amd64"}"#;
    let config_digest = digest::of_bytes(config);
    registry
        .server
        .mock_async(|when, then| {
            when.method(GET).path(registry.path("manifests/amd64-only"));
            then.status(200).json_body(serde_json::json!({
                "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
                "config": {"digest": config_digest.to_string(), "size": config.len()},
                "layers": []
            }));
        })
        .await;
    registry
        .server
        .mock_async(|when, then| {
            when.method(GET)
                .path(registry.path(&format!("blobs/{config_digest}")));
            then.status(200).body(config);
        })
        .await;

    let platforms = registry
        .client(Access::Pull)
        .image_platforms("amd64-only")
        .await
        .expect("platforms");

    assert_eq!(platforms, [("linux".to_owned(), "amd64".to_owned())]);
}
