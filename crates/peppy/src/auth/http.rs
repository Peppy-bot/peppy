//! Thin blocking HTTP client over `ureq` shared by the auth engine.
//!
//! The client is configured with `http_status_as_error(false)` so non-2xx
//! responses come back as [`HttpResponse`] (status + body) rather than an opaque
//! `ureq::Error` — the device flow and `/cli-config` need to read 4xx/5xx bodies
//! (`authorization_pending`, `slow_down`, the 503 "not configured" case). Bearer
//! tokens are never included in error strings (only the URL, with its query
//! stripped), so a verbose log can't leak them.

use std::time::Duration;

use serde::de::DeserializeOwned;

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
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(30)))
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

    /// `POST url` with no body (used for `/logout`).
    pub fn post_empty(&self, url: &str, bearer: Option<&str>) -> Result<HttpResponse> {
        let resp = with_bearer(self.agent.post(url), bearer)
            .send_empty()
            .map_err(|e| Error::Http(format!("POST {} failed: {e}", redact(url))))?;
        finish("POST", url, resp)
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
        .read_to_string()
        .map_err(|e| Error::Http(format!("{method} {} failed reading body: {e}", redact(url))))?;
    Ok(HttpResponse { status, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_strips_query() {
        assert_eq!(redact("https://h/oauth?code=secret"), "https://h/oauth");
        assert_eq!(redact("https://h/me"), "https://h/me");
    }
}
