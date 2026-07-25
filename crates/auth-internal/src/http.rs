//! Thin blocking HTTP client over `ureq` shared by the auth engine.
//!
//! The client is configured with `http_status_as_error(false)` so non-2xx
//! responses come back as [`HttpResponse`] (status + body) rather than an opaque
//! `ureq::Error`: the device flow and `/cli/auth-config` need to read 4xx/5xx bodies
//! (`authorization_pending`, `slow_down`, the 503 "not configured" case). Bearer
//! tokens are never included in error strings (only the URL, with its query
//! stripped), so a verbose log can't leak them.

use std::time::Duration;

use serde::de::DeserializeOwned;
use ureq::config::RedirectAuthHeaders;
use ureq::tls::{RootCerts, TlsConfig};

use crate::error::{Error, Result};

/// A fully-read HTTP response: status code and body text.
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

impl HttpResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Parses the body as JSON into `T`, tagging a parse failure with `what` (the
    /// endpoint or payload name) so a malformed response stays attributable.
    pub fn json<T: DeserializeOwned>(&self, what: &str) -> Result<T> {
        serde_json::from_str(&self.body)
            .map_err(|e| Error::Auth(format!("invalid {what} response: {e}")))
    }
}

/// The default global HTTP timeout for the CLI's blocking client.
const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Every response this client reads is a small JSON document. Bounding the
/// fully buffered body keeps a broken or hostile upstream from making the CLI
/// allocate an arbitrary amount of memory before anything parses it.
const MAX_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;

/// A shared blocking HTTP client wrapping a configured `ureq::Agent`.
///
/// `http_status_as_error(false)` makes 4xx/5xx return `Ok` so callers can inspect
/// the body; a global timeout keeps a hung backend from blocking the CLI forever.
pub struct HttpClient {
    agent: ureq::Agent,
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient {
    pub fn new() -> Self {
        Self::with_timeout(DEFAULT_HTTP_TIMEOUT)
    }

    /// Like [`new`](Self::new) but with an explicit global timeout. The router
    /// federation path uses this to honor the configurable
    /// `federation.connect_timeout_secs` instead of the default; every other
    /// caller stays on [`new`](Self::new).
    pub fn with_timeout(timeout: Duration) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            // Use the host platform's trust policy rather than ureq's static
            // Mozilla bundle, so enterprise and robot-fleet trust
            // administration is honored and a test harness can inject a fixture
            // root through the standard SSL_CERT_FILE mechanism. Full WebPKI
            // hostname validation is retained.
            .tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            // Control-plane redirects are deliberately not followed. It is a
            // stronger and far easier to audit form of the downgrade guard than
            // inspecting each hop: https can never be walked down to http by a
            // Location header, and a bearer can never cross an origin. A caller
            // receives the 3xx and rejects it as an unexpected status.
            .max_redirects(0)
            // Already ureq's default. Pinned so a future default change cannot
            // quietly re-enable header forwarding across a redirect; this line
            // changes no behaviour today.
            .redirect_auth_headers(RedirectAuthHeaders::Never)
            .timeout_global(Some(timeout))
            .build()
            .into();
        Self { agent }
    }

    /// `GET url`, optionally with a bearer token.
    pub fn get(&self, url: &str, bearer: Option<&str>) -> Result<HttpResponse> {
        let resp = with_bearer(self.agent.get(url), bearer)
            .call()
            .map_err(|e| Error::Http(format!("GET {} failed: {e}", redact(url))))?;
        finish("GET", url, resp)
    }

    /// `POST url` with an `application/x-www-form-urlencoded` body built from `form`.
    /// The body is encoded with `url::form_urlencoded` so values like the
    /// `device_code` grant type are escaped correctly.
    pub fn post_form(
        &self,
        url: &str,
        form: &[(&str, &str)],
        bearer: Option<&str>,
    ) -> Result<HttpResponse> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form.iter().copied())
            .finish();
        let req = self
            .agent
            .post(url)
            .header("Content-Type", "application/x-www-form-urlencoded");
        let resp = with_bearer(req, bearer)
            .send(body)
            .map_err(|e| Error::Http(format!("POST {} failed: {e}", redact(url))))?;
        finish("POST", url, resp)
    }

    /// `POST url` with a pre-serialized JSON `body` (`application/json`),
    /// optionally with a bearer token. The caller serializes the payload so this
    /// thin client stays free of `serde::Serialize` bounds.
    pub fn post_json(&self, url: &str, body: &str, bearer: Option<&str>) -> Result<HttpResponse> {
        let req = self
            .agent
            .post(url)
            .header("Content-Type", "application/json");
        let resp = with_bearer(req, bearer)
            .send(body)
            .map_err(|e| Error::Http(format!("POST {} failed: {e}", redact(url))))?;
        finish("POST", url, resp)
    }

    /// `POST url` with no body (used for `/logout`).
    pub fn post_empty(&self, url: &str, bearer: Option<&str>) -> Result<HttpResponse> {
        let resp = with_bearer(self.agent.post(url), bearer)
            .send_empty()
            .map_err(|e| Error::Http(format!("POST {} failed: {e}", redact(url))))?;
        finish("POST", url, resp)
    }

    /// `DELETE url` (used for `/me/core-nodes/{core_node_name}`).
    pub fn delete(&self, url: &str, bearer: Option<&str>) -> Result<HttpResponse> {
        let resp = with_bearer(self.agent.delete(url), bearer)
            .call()
            .map_err(|e| Error::Http(format!("DELETE {} failed: {e}", redact(url))))?;
        finish("DELETE", url, resp)
    }
}

/// Strips the query string from a URL for error messages, so a `verification_uri_complete`
/// or any future token-bearing query never lands in a log line.
fn redact(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

/// Applies an optional `Bearer` token to a request builder. Generic over the body
/// state so the bodyless `get` and the body-carrying `post` verbs share one path.
fn with_bearer<B>(req: ureq::RequestBuilder<B>, bearer: Option<&str>) -> ureq::RequestBuilder<B> {
    match bearer {
        Some(token) => req.header("Authorization", format!("Bearer {token}")),
        None => req,
    }
}

fn finish(method: &str, url: &str, resp: ureq::http::Response<ureq::Body>) -> Result<HttpResponse> {
    let status = resp.status().as_u16();
    let mut resp = resp;
    let body = resp
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_string()
        .map_err(|e| Error::Http(format!("{method} {} failed reading body: {e}", redact(url))))?;
    Ok(HttpResponse { status, body })
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::Method::GET;
    use httpmock::MockServer;

    #[test]
    fn redact_strips_query() {
        assert_eq!(redact("https://h/oauth?code=secret"), "https://h/oauth");
        assert_eq!(redact("https://h/me"), "https://h/me");
    }

    #[test]
    fn client_uses_verified_platform_trust_roots() {
        let client = HttpClient::new();
        let tls = client.agent.config().tls_config();
        assert!(matches!(tls.root_certs(), RootCerts::PlatformVerifier));
        assert!(tls.use_sni());
        assert!(!tls.disable_verification());
    }

    #[test]
    fn redirects_are_returned_without_being_followed() {
        let server = MockServer::start();
        let target = server.mock(|when, then| {
            when.method(GET).path("/target");
            then.status(200).body("followed");
        });
        let redirect = server.mock(|when, then| {
            when.method(GET).path("/redirect");
            then.status(302)
                .header("Location", format!("{}/target", server.base_url()));
        });

        let response = HttpClient::new()
            .get(&format!("{}/redirect", server.base_url()), Some("secret"))
            .expect("the raw redirect response is available to the caller");

        assert_eq!(response.status, 302);
        redirect.assert_calls(1);
        target.assert_calls(0);
    }
}
