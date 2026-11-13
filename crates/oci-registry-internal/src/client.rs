//! A client of one OCI repository, over the OCI distribution API.

use crate::auth::{Challenge, Credential, CredentialStore, api_host};
use crate::digest::{self, Hasher};
use crate::error::{RegistryError, Result};
use crate::manifest::IMAGE_MANIFEST_MEDIA_TYPE;
use bytes::Bytes;
use core_node_api::encoding::{OciDigest, OciRepository};
use futures_util::StreamExt as _;
use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode};
use std::fmt;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// The most bytes peppy reads of a manifest or of a small blob (the
/// `peppy.json5` of a node).
pub const MAX_SMALL_BLOB_BYTES: u64 = 4 << 20;

/// The size of the chunks of a blob upload.
pub const DEFAULT_UPLOAD_CHUNK_BYTES: u64 = 32 << 20;

/// The size of the parts in which a chunk of an upload goes to the
/// connection: the progress of a push moves by one part at a time, so a slow
/// link reports progress inside a chunk.
const UPLOAD_PART_BYTES: usize = 256 << 10;

/// How many tags one page of the tag list asks for.
const DEFAULT_TAGS_PAGE_SIZE: u32 = 1000;

/// The longest a connection takes to open.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The longest the registry may stay silent in the middle of an answer.
const READ_TIMEOUT: Duration = Duration::from_secs(300);

/// The most redirects one request follows: a registry may send a blob from
/// its object storage.
const MAX_REDIRECTS: usize = 10;

/// What the client may do in the repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Read tags, manifests and blobs.
    Pull,
    /// Read, and write blobs and manifests.
    Push,
}

impl Access {
    fn scope_actions(self) -> &'static str {
        match self {
            Self::Pull => "pull",
            Self::Push => "pull,push",
        }
    }
}

/// A client of one OCI repository. It asks the registry for a credential
/// only when the registry asks for one, and keeps the authorization it got
/// for its later requests.
pub struct RegistryClient {
    http: reqwest::Client,
    repository: OciRepository,
    /// `<scheme>://<api host>/v2/<repository path>`.
    base: String,
    access: Access,
    credentials: CredentialStore,
    /// The `Authorization` header of the requests, once the registry asked
    /// for one.
    authorization: tokio::sync::Mutex<Option<HeaderValue>>,
    upload_chunk_bytes: u64,
    tags_page_size: u32,
}

/// Shows which repository the client speaks to and how. The `Authorization`
/// header is a secret, so it is not shown.
impl fmt::Debug for RegistryClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegistryClient")
            .field("repository", &self.repository)
            .field("base", &self.base)
            .field("access", &self.access)
            .field("credentials", &self.credentials)
            .field("upload_chunk_bytes", &self.upload_chunk_bytes)
            .field("tags_page_size", &self.tags_page_size)
            .finish_non_exhaustive()
    }
}

/// An answer that the request expects, or the error of any other.
type Answer = reqwest::Response;

impl RegistryClient {
    /// A client of `repository`, which reads credentials from `credentials`.
    /// A loopback registry is reached over plain HTTP, every other over
    /// HTTPS.
    pub fn new(
        repository: &OciRepository,
        credentials: CredentialStore,
        access: Access,
    ) -> Result<Self> {
        let scheme = if repository.registry_is_loopback() {
            "http"
        } else {
            "https"
        };
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
            .user_agent(format!("peppy/{}", daemon_config::consts::PEPPY_VERSION))
            .build()
            .map_err(|e| RegistryError::Unreachable {
                registry: repository.registry().to_owned(),
                cause: format!("cannot make an HTTP client: {e}"),
            })?;
        Ok(Self {
            http,
            base: format!(
                "{scheme}://{}/v2/{}",
                api_host(repository.registry()),
                repository.path()
            ),
            repository: repository.clone(),
            access,
            credentials,
            authorization: tokio::sync::Mutex::new(None),
            upload_chunk_bytes: DEFAULT_UPLOAD_CHUNK_BYTES,
            tags_page_size: DEFAULT_TAGS_PAGE_SIZE,
        })
    }

    /// The same client with uploads in chunks of `bytes`.
    pub fn with_upload_chunk_bytes(mut self, bytes: u64) -> Self {
        self.upload_chunk_bytes = bytes.max(1);
        self
    }

    /// The same client with pages of `size` tags in the tag list.
    pub fn with_tags_page_size(mut self, size: u32) -> Self {
        self.tags_page_size = size.max(1);
        self
    }

    pub fn repository(&self) -> &OciRepository {
        &self.repository
    }

    /// Every tag of the repository, from every page of the tag list. A
    /// repository that the registry does not know yet (nothing was pushed
    /// to it) has no tags.
    pub async fn list_tags(&self) -> Result<Vec<String>> {
        let mut tags = Vec::new();
        let mut next = Some(format!("{}/tags/list?n={}", self.base, self.tags_page_size));
        let mut seen_pages = std::collections::HashSet::new();
        while let Some(url) = next.take() {
            if !seen_pages.insert(url.clone()) {
                return Err(self.protocol("GET tags/list", format!("a tag page loop at {url}")));
            }
            let answer = self.send(Method::GET, &url, HeaderMap::new(), None).await?;
            if answer.status() == StatusCode::NOT_FOUND {
                let detail = error_detail(answer).await;
                if detail.contains("NAME_UNKNOWN") {
                    return Ok(Vec::new());
                }
                return Err(self.refused("GET tags/list", StatusCode::NOT_FOUND, detail));
            }
            let answer = self
                .expect(answer, "GET tags/list", &[StatusCode::OK])
                .await?;
            next = next_page(answer.headers(), &url);
            let page: TagPage = read_json(answer, "GET tags/list", self).await?;
            tags.extend(page.tags.unwrap_or_default());
        }
        Ok(tags)
    }

    /// The manifest that `reference` (a tag or a digest) names, with its
    /// digest, or `None` when the repository has no such manifest.
    pub async fn manifest(&self, reference: &str) -> Result<Option<(OciDigest, Vec<u8>)>> {
        let request = format!("GET manifests/{reference}");
        let url = format!("{}/manifests/{reference}", self.base);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static(IMAGE_MANIFEST_MEDIA_TYPE),
        );
        let answer = self.send(Method::GET, &url, headers, None).await?;
        if answer.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let answer = self.expect(answer, &request, &[StatusCode::OK]).await?;
        let bytes = read_limited(answer, MAX_SMALL_BLOB_BYTES, &request, self).await?;
        let digest = digest::of_bytes(&bytes);
        if let Ok(expected) = OciDigest::parse(reference)
            && expected != digest
        {
            return Err(RegistryError::DigestMismatch {
                what: format!("the manifest {reference} of {}", self.repository),
                expected: expected.to_string(),
                actual: digest.to_string(),
            });
        }
        Ok(Some((digest, bytes)))
    }

    /// The platforms the image `reference` (a tag or a digest) is held for,
    /// as `(os, architecture)`: those of the entries of an image index or a
    /// manifest list, or the one of the config of a single image manifest.
    /// Entries without a platform, or of the `unknown` OS (attestations), are
    /// left out.
    pub async fn image_platforms(&self, reference: &str) -> Result<Vec<(String, String)>> {
        use crate::image::{IndexJson, PLATFORM_MANIFEST_MEDIA_TYPES};
        let request = format!("GET manifests/{reference}");
        let url = format!("{}/manifests/{reference}", self.base);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            header_value(&PLATFORM_MANIFEST_MEDIA_TYPES.join(", ")),
        );
        let answer = self.send(Method::GET, &url, headers, None).await?;
        if answer.status() == StatusCode::NOT_FOUND {
            return Err(RegistryError::NotFound {
                registry: self.repository.registry().to_owned(),
                what: format!("image {}:{reference}", self.repository.path()),
            });
        }
        let answer = self.expect(answer, &request, &[StatusCode::OK]).await?;
        let index: IndexJson = read_json(answer, &request, self).await?;
        if let Some(manifests) = index.manifests {
            return Ok(manifests
                .into_iter()
                .filter_map(|entry| entry.platform)
                .filter(|platform| platform.os != "unknown" && !platform.os.is_empty())
                .map(|platform| (platform.os, platform.architecture))
                .collect());
        }
        let Some(config) = index.config else {
            return Err(self.protocol(
                &request,
                format!(
                    "a manifest of media type {} with neither entries nor a config",
                    index.media_type.unwrap_or_default()
                ),
            ));
        };
        let digest =
            OciDigest::parse(&config.digest).map_err(|e| self.protocol(&request, e.to_string()))?;
        #[derive(serde::Deserialize)]
        struct ImageConfig {
            #[serde(default)]
            os: String,
            #[serde(default)]
            architecture: String,
        }
        let config: ImageConfig = serde_json::from_slice(&self.blob_bytes(&digest).await?)
            .map_err(|e| {
                self.protocol(&request, format!("an image config that is not JSON: {e}"))
            })?;
        if config.os.is_empty() || config.architecture.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![(config.os, config.architecture)])
    }

    /// The bytes of a small blob, checked against `digest`.
    pub async fn blob_bytes(&self, digest: &OciDigest) -> Result<Vec<u8>> {
        let request = format!("GET blobs/{digest}");
        let answer = self.get_blob(digest).await?;
        let bytes = read_limited(answer, MAX_SMALL_BLOB_BYTES, &request, self).await?;
        let actual = digest::of_bytes(&bytes);
        if &actual != digest {
            return Err(RegistryError::DigestMismatch {
                what: format!("the blob {digest} of {}", self.repository),
                expected: digest.to_string(),
                actual: actual.to_string(),
            });
        }
        Ok(bytes)
    }

    /// Writes the blob `digest` of `size` bytes to `destination`. The bytes
    /// go to a temporary file next to it, and their sha256 is checked while
    /// they arrive; only a blob of the right size and digest is moved to
    /// `destination`, so a failed pull leaves nothing there. `on_progress`
    /// gets the number of bytes received so far after each part.
    pub async fn download_blob(
        &self,
        digest: &OciDigest,
        size: u64,
        destination: &Path,
        on_progress: &mut (dyn FnMut(u64) + Send),
    ) -> Result<()> {
        let parent = destination
            .parent()
            .ok_or_else(|| {
                RegistryError::io(
                    destination,
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent directory"),
                )
            })?
            .to_path_buf();
        tokio::fs::create_dir_all(&parent)
            .await
            .map_err(|e| RegistryError::io(&parent, e))?;
        let temporary = tempfile::Builder::new()
            .prefix(".pull-")
            .suffix(".partial")
            .tempfile_in(&parent)
            .map_err(|e| RegistryError::io(&parent, e))?;
        let mut file = tokio::fs::File::from_std(
            temporary
                .reopen()
                .map_err(|e| RegistryError::io(temporary.path(), e))?,
        );
        let mut answer = self.get_blob(digest).await?;
        let what = format!("the blob {digest} of {}", self.repository);
        let mut hasher = Hasher::default();
        let mut received = 0u64;
        while let Some(part) = answer.chunk().await.map_err(|e| self.unreachable(e))? {
            received += part.len() as u64;
            if received > size {
                return Err(RegistryError::SizeMismatch {
                    what,
                    expected: size,
                    actual: received,
                });
            }
            hasher.update(&part);
            file.write_all(&part)
                .await
                .map_err(|e| RegistryError::io(temporary.path(), e))?;
            on_progress(received);
        }
        if received != size {
            return Err(RegistryError::SizeMismatch {
                what,
                expected: size,
                actual: received,
            });
        }
        let actual = hasher.finish();
        if &actual != digest {
            return Err(RegistryError::DigestMismatch {
                what,
                expected: digest.to_string(),
                actual: actual.to_string(),
            });
        }
        file.sync_all()
            .await
            .map_err(|e| RegistryError::io(temporary.path(), e))?;
        drop(file);
        temporary
            .persist(destination)
            .map_err(|e| RegistryError::io(destination, e.error))?;
        Ok(())
    }

    /// Whether the repository has the blob `digest`.
    pub async fn blob_exists(&self, digest: &OciDigest) -> Result<bool> {
        let request = format!("HEAD blobs/{digest}");
        let url = format!("{}/blobs/{digest}", self.base);
        let answer = self
            .send(Method::HEAD, &url, HeaderMap::new(), None)
            .await?;
        match answer.status() {
            StatusCode::OK => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            status => Err(self.refused(&request, status, error_detail(answer).await)),
        }
    }

    /// Checks that the registry accepts a push to the repository: opens a
    /// blob upload and cancels it.
    pub async fn check_push_access(&self) -> Result<()> {
        let location = self.start_upload().await?;
        // A registry that cannot cancel an upload lets it expire, so a
        // failed cancel does not fail the check.
        if let Err(error) = self
            .send(Method::DELETE, &location, HeaderMap::new(), None)
            .await
        {
            tracing::debug!(%error, "the registry did not cancel the upload of the push check");
        }
        Ok(())
    }

    /// Pushes the file at `path`, of digest `digest` and `size` bytes, in
    /// chunks, unless the repository already has it. Returns whether it
    /// pushed. `on_progress` gets the number of bytes sent so far as the
    /// connection takes each part of a chunk. A chunk sent again after a
    /// challenge of the registry counts again from its start.
    pub async fn push_blob_file(
        &self,
        path: &Path,
        digest: &OciDigest,
        size: u64,
        on_progress: &mut (dyn FnMut(u64) + Send),
    ) -> Result<bool> {
        if self.blob_exists(digest).await? {
            return Ok(false);
        }
        let mut location = self.start_upload().await?;
        let mut file = tokio::fs::File::open(path)
            .await
            .map_err(|e| RegistryError::io(path, e))?;
        let mut sent = 0u64;
        while sent < size {
            let length = self.upload_chunk_bytes.min(size - sent);
            let mut chunk =
                vec![0u8; usize::try_from(length).expect("an upload chunk fits in memory")];
            file.read_exact(&mut chunk)
                .await
                .map_err(|e| RegistryError::io(path, e))?;
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
            headers.insert(
                header::CONTENT_RANGE,
                header_value(&format!("{}-{}", sent, sent + length - 1)),
            );
            // A body in parts has no length of its own: the header keeps the
            // chunk out of a chunked transfer encoding.
            headers.insert(header::CONTENT_LENGTH, header_value(&length.to_string()));
            let request = format!("PATCH blobs/uploads ({length} bytes at {sent})");
            let (taken_tx, mut taken_rx) = tokio::sync::mpsc::unbounded_channel();
            let answer = report_taken_while(
                self.send_body(
                    Method::PATCH,
                    &location,
                    headers,
                    Some(RequestBody::InParts {
                        bytes: Bytes::from(chunk),
                        taken: taken_tx,
                    }),
                ),
                &mut taken_rx,
                |taken| on_progress(sent + taken),
            )
            .await?;
            let answer = self
                .expect(answer, &request, &[StatusCode::ACCEPTED])
                .await?;
            location = self.upload_location(&answer, &location, &request)?;
            sent += length;
        }
        self.finish_upload(&location, digest, None).await?;
        Ok(true)
    }

    /// Pushes `bytes` as one blob, unless the repository already has it.
    pub async fn push_blob_bytes(&self, bytes: &[u8]) -> Result<OciDigest> {
        let digest = digest::of_bytes(bytes);
        if self.blob_exists(&digest).await? {
            return Ok(digest);
        }
        let location = self.start_upload().await?;
        self.finish_upload(&location, &digest, Some(Bytes::copy_from_slice(bytes)))
            .await?;
        Ok(digest)
    }

    /// Pushes an image manifest under `tag`, and returns its digest.
    pub async fn put_manifest(&self, tag: &str, bytes: &[u8]) -> Result<OciDigest> {
        let request = format!("PUT manifests/{tag}");
        let url = format!("{}/manifests/{tag}", self.base);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(IMAGE_MANIFEST_MEDIA_TYPE),
        );
        let answer = self
            .send(
                Method::PUT,
                &url,
                headers,
                Some(Bytes::copy_from_slice(bytes)),
            )
            .await?;
        self.expect(answer, &request, &[StatusCode::CREATED])
            .await?;
        Ok(digest::of_bytes(bytes))
    }

    async fn get_blob(&self, digest: &OciDigest) -> Result<Answer> {
        let request = format!("GET blobs/{digest}");
        let url = format!("{}/blobs/{digest}", self.base);
        let answer = self.send(Method::GET, &url, HeaderMap::new(), None).await?;
        if answer.status() == StatusCode::NOT_FOUND {
            return Err(RegistryError::NotFound {
                registry: self.repository.registry().to_owned(),
                what: format!("blob {digest} in {}", self.repository),
            });
        }
        self.expect(answer, &request, &[StatusCode::OK]).await
    }

    /// Opens a blob upload and returns its location.
    async fn start_upload(&self) -> Result<String> {
        let request = "POST blobs/uploads";
        let url = format!("{}/blobs/uploads/", self.base);
        let answer = self
            .send(Method::POST, &url, HeaderMap::new(), Some(Bytes::new()))
            .await?;
        let answer = self
            .expect(answer, request, &[StatusCode::ACCEPTED])
            .await?;
        self.upload_location(&answer, &url, request)
    }

    /// Closes the upload at `location` with its last bytes, if any, and
    /// gives the blob its digest.
    async fn finish_upload(
        &self,
        location: &str,
        digest: &OciDigest,
        body: Option<Bytes>,
    ) -> Result<()> {
        let request = format!("PUT blobs/uploads ({digest})");
        let mut url = url::Url::parse(location)
            .map_err(|e| self.protocol(&request, format!("an upload location {location}: {e}")))?;
        url.query_pairs_mut().append_pair("digest", digest.as_str());
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        let answer = self
            .send(
                Method::PUT,
                url.as_str(),
                headers,
                Some(body.unwrap_or_default()),
            )
            .await?;
        self.expect(answer, &request, &[StatusCode::CREATED])
            .await?;
        Ok(())
    }

    /// The absolute URL of the `Location` of an upload answer, which may be
    /// relative to the request.
    fn upload_location(&self, answer: &Answer, request_url: &str, request: &str) -> Result<String> {
        let location = answer
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| self.protocol(request, "no Location header".to_owned()))?;
        let base = url::Url::parse(request_url)
            .map_err(|e| self.protocol(request, format!("a request URL {request_url}: {e}")))?;
        base.join(location)
            .map(String::from)
            .map_err(|e| self.protocol(request, format!("the Location {location}: {e}")))
    }

    /// Sends a request, and when the registry asks for a credential, gets
    /// an authorization for it and sends the request again.
    async fn send(
        &self,
        method: Method,
        url: &str,
        headers: HeaderMap,
        body: Option<Bytes>,
    ) -> Result<Answer> {
        self.send_body(method, url, headers, body.map(RequestBody::Whole))
            .await
    }

    /// [`Self::send`] with a body that may go to the connection in parts.
    async fn send_body(
        &self,
        method: Method,
        url: &str,
        headers: HeaderMap,
        body: Option<RequestBody>,
    ) -> Result<Answer> {
        let attempt = |authorization: Option<HeaderValue>| {
            let mut builder = self
                .http
                .request(method.clone(), url)
                .headers(headers.clone());
            if let Some(value) = authorization {
                builder = builder.header(header::AUTHORIZATION, value);
            }
            if let Some(body) = &body {
                builder = builder.body(body.to_reqwest());
            }
            builder.send()
        };
        let known = self.authorization.lock().await.clone();
        let answer = attempt(known.clone())
            .await
            .map_err(|e| self.unreachable(e))?;
        if answer.status() != StatusCode::UNAUTHORIZED {
            return Ok(answer);
        }
        let challenge = answer
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .and_then(Challenge::parse);
        let Some(challenge) = challenge else {
            let detail = error_detail(answer).await;
            return Err(self.refused(&format!("{method} {url}"), StatusCode::UNAUTHORIZED, detail));
        };
        let credential = self
            .credentials
            .credential_for(self.repository.registry())
            .await?;
        let authorization = self.authorize(&challenge, credential.as_ref()).await?;
        *self.authorization.lock().await = Some(authorization.clone());
        let answer = attempt(Some(authorization))
            .await
            .map_err(|e| self.unreachable(e))?;
        if answer.status() != StatusCode::UNAUTHORIZED {
            return Ok(answer);
        }
        let detail = error_detail(answer).await;
        Err(match credential {
            None => RegistryError::NoCredential {
                registry: self.repository.registry().to_owned(),
            },
            Some(_) => RegistryError::Unauthorized {
                registry: self.repository.registry().to_owned(),
                detail,
            },
        })
    }

    /// The `Authorization` header that answers `challenge`.
    async fn authorize(
        &self,
        challenge: &Challenge,
        credential: Option<&Credential>,
    ) -> Result<HeaderValue> {
        let registry = self.repository.registry().to_owned();
        let params = match challenge {
            Challenge::Basic => {
                let header = credential
                    .and_then(Credential::basic_header)
                    .ok_or(RegistryError::NoCredential { registry })?;
                return Ok(header_value(&header));
            }
            Challenge::Bearer { params } => params,
        };
        let realm = &params["realm"];
        let scope = format!(
            "repository:{}:{}",
            self.repository.path(),
            self.access.scope_actions()
        );
        let service = params.get("service").cloned().unwrap_or_default();
        let request = format!("the token request to {realm}");
        let token_answer = match credential {
            Some(Credential::IdentityToken(token)) => {
                let form = url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("grant_type", "refresh_token")
                    .append_pair("refresh_token", token)
                    .append_pair("service", &service)
                    .append_pair("scope", &scope)
                    .append_pair("client_id", "peppy")
                    .finish();
                self.http
                    .post(realm)
                    .header(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/x-www-form-urlencoded"),
                    )
                    .body(form)
                    .send()
                    .await
            }
            basic => {
                let mut url = url::Url::parse(realm).map_err(|e| {
                    self.protocol(&request, format!("a token realm that is not a URL: {e}"))
                })?;
                url.query_pairs_mut()
                    .append_pair("service", &service)
                    .append_pair("scope", &scope);
                let mut builder = self.http.get(url);
                if let Some(header) = basic.and_then(Credential::basic_header) {
                    builder = builder.header(header::AUTHORIZATION, header);
                }
                builder.send().await
            }
        }
        .map_err(|e| self.unreachable(e))?;
        if token_answer.status() == StatusCode::UNAUTHORIZED
            || token_answer.status() == StatusCode::FORBIDDEN
        {
            let detail = error_detail(token_answer).await;
            return Err(match credential {
                None => RegistryError::NoCredential { registry },
                Some(_) => RegistryError::Unauthorized { registry, detail },
            });
        }
        let token_answer = self
            .expect(token_answer, &request, &[StatusCode::OK])
            .await?;
        let token: TokenAnswer = read_json(token_answer, &request, self).await?;
        let token = token
            .token
            .or(token.access_token)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| self.protocol(&request, "no token".to_owned()))?;
        Ok(header_value(&format!("Bearer {token}")))
    }

    /// `answer` when its status is one of `expected`, else the error that
    /// names the request, the status and what the registry said.
    async fn expect(
        &self,
        answer: Answer,
        request: &str,
        expected: &[StatusCode],
    ) -> Result<Answer> {
        if expected.contains(&answer.status()) {
            return Ok(answer);
        }
        let status = answer.status();
        Err(self.refused(request, status, error_detail(answer).await))
    }

    fn refused(&self, request: &str, status: StatusCode, detail: String) -> RegistryError {
        RegistryError::Refused {
            registry: self.repository.registry().to_owned(),
            request: request.to_owned(),
            status: status.as_u16(),
            detail,
        }
    }

    fn protocol(&self, request: &str, detail: String) -> RegistryError {
        RegistryError::Protocol {
            registry: self.repository.registry().to_owned(),
            request: request.to_owned(),
            detail,
        }
    }

    fn unreachable(&self, error: reqwest::Error) -> RegistryError {
        RegistryError::Unreachable {
            registry: self.repository.registry().to_owned(),
            cause: error_with_causes(&error),
        }
    }
}

/// The body of a request.
enum RequestBody {
    /// Bytes that go to the connection at once.
    Whole(Bytes),
    /// Bytes that go to the connection in parts of [`UPLOAD_PART_BYTES`].
    /// As the connection takes each part, `taken` gets the count of the
    /// bytes of the body taken so far.
    InParts {
        bytes: Bytes,
        taken: tokio::sync::mpsc::UnboundedSender<u64>,
    },
}

impl RequestBody {
    /// The body of one attempt of the request: a request sent again sends
    /// and counts its body again from its start.
    fn to_reqwest(&self) -> reqwest::Body {
        match self {
            Self::Whole(bytes) => reqwest::Body::from(bytes.clone()),
            Self::InParts { bytes, taken } => {
                let parts: Vec<Bytes> = (0..bytes.len())
                    .step_by(UPLOAD_PART_BYTES)
                    .map(|start| bytes.slice(start..(start + UPLOAD_PART_BYTES).min(bytes.len())))
                    .collect();
                let taken = taken.clone();
                let mut count = 0u64;
                reqwest::Body::wrap_stream(futures_util::stream::iter(parts).map(move |part| {
                    count += part.len() as u64;
                    // The receiver is gone once the request ended.
                    let _ = taken.send(count);
                    Ok::<_, !>(part)
                }))
            }
        }
    }
}

/// Awaits `request`, and gives `on_taken` each count of `taken` while the
/// request runs, then the counts left once it ended.
async fn report_taken_while<T>(
    request: impl Future<Output = T>,
    taken: &mut tokio::sync::mpsc::UnboundedReceiver<u64>,
    mut on_taken: impl FnMut(u64),
) -> T {
    let mut request = std::pin::pin!(request);
    let outcome = loop {
        tokio::select! {
            outcome = &mut request => break outcome,
            Some(count) = taken.recv() => on_taken(count),
        }
    };
    while let Ok(count) = taken.try_recv() {
        on_taken(count);
    }
    outcome
}

fn header_value(text: &str) -> HeaderValue {
    HeaderValue::from_str(text).unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// One page of `GET /v2/<name>/tags/list`.
#[derive(serde::Deserialize)]
struct TagPage {
    #[serde(default)]
    tags: Option<Vec<String>>,
}

/// The answer of a token service.
#[derive(serde::Deserialize)]
struct TokenAnswer {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
}

/// The `rel="next"` URL of a `Link` header, resolved against `current`.
fn next_page(headers: &HeaderMap, current: &str) -> Option<String> {
    let link = headers.get(header::LINK)?.to_str().ok()?;
    link.split(',').find_map(|part| {
        let (target, params) = part.split_once(';')?;
        if !params
            .split(';')
            .any(|param| param.trim().replace(' ', "") == "rel=\"next\"")
        {
            return None;
        }
        let target = target.trim().strip_prefix('<')?.strip_suffix('>')?;
        url::Url::parse(current)
            .ok()?
            .join(target)
            .ok()
            .map(String::from)
    })
}

/// The body of `answer`, up to `limit` bytes.
async fn read_limited(
    mut answer: Answer,
    limit: u64,
    request: &str,
    client: &RegistryClient,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(part) = answer.chunk().await.map_err(|e| client.unreachable(e))? {
        if bytes.len() as u64 + part.len() as u64 > limit {
            return Err(client.protocol(request, format!("more than {limit} bytes")));
        }
        bytes.extend_from_slice(&part);
    }
    Ok(bytes)
}

async fn read_json<T: serde::de::DeserializeOwned>(
    answer: Answer,
    request: &str,
    client: &RegistryClient,
) -> Result<T> {
    let bytes = read_limited(answer, MAX_SMALL_BLOB_BYTES, request, client).await?;
    serde_json::from_slice(&bytes)
        .map_err(|e| client.protocol(request, format!("JSON that is not the expected one: {e}")))
}

/// What a registry said in an error answer: the `code: message` of each
/// error of the OCI error body, else the start of the body.
async fn error_detail(answer: Answer) -> String {
    #[derive(serde::Deserialize)]
    struct ErrorBody {
        errors: Vec<ErrorEntry>,
    }
    #[derive(serde::Deserialize)]
    struct ErrorEntry {
        #[serde(default)]
        code: String,
        #[serde(default)]
        message: String,
    }
    let status = answer.status();
    let body = answer.bytes().await.unwrap_or_default();
    if let Ok(parsed) = serde_json::from_slice::<ErrorBody>(&body)
        && !parsed.errors.is_empty()
    {
        return parsed
            .errors
            .iter()
            .map(|error| format!("{}: {}", error.code, error.message))
            .collect::<Vec<_>>()
            .join("; ");
    }
    let text = String::from_utf8_lossy(&body);
    let text = text.trim();
    if text.is_empty() {
        return status.canonical_reason().unwrap_or("no message").to_owned();
    }
    text.chars().take(300).collect()
}

/// `error` with its chain of causes: reqwest puts the reason of a failed
/// connection (refused, DNS, TLS) in its sources.
fn error_with_causes(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests;
