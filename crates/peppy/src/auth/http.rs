//! Thin blocking HTTP helpers over `ureq` shared by the auth engine.
//!
//! The agent is configured with `http_status_as_error(false)` so non-2xx
//! responses come back as [`HttpResponse`] (status + body) rather than an opaque
//! `ureq::Error` — the device flow and `/cli-config` need to read 4xx/5xx bodies
//! (`authorization_pending`, `slow_down`, the 503 "not configured" case). Bearer
//! tokens are never included in error strings (only the URL, with its query
//! stripped), so a verbose log can't leak them.

use std::time::Duration;

use crate::error::{Error, Result};

/// A fully-read HTTP response: status code, body text, and the parsed
/// `Retry-After` (seconds) when present.
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
    pub retry_after: Option<u64>,
}

impl HttpResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Builds the shared agent. `http_status_as_error(false)` makes 4xx/5xx return
/// `Ok` so callers can inspect the body; a global timeout keeps a hung backend
/// from blocking the CLI forever.
pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into()
}

/// Strips the query string from a URL for error messages, so a `verification_uri_complete`
/// or any future token-bearing query never lands in a log line.
fn redact(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

fn finish(method: &str, url: &str, resp: ureq::http::Response<ureq::Body>) -> Result<HttpResponse> {
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok());
    let mut resp = resp;
    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| Error::Http(format!("{method} {} failed reading body: {e}", redact(url))))?;
    Ok(HttpResponse {
        status,
        body,
        retry_after,
    })
}

/// `GET url`, optionally with a bearer token.
pub fn get(agent: &ureq::Agent, url: &str, bearer: Option<&str>) -> Result<HttpResponse> {
    let mut req = agent.get(url);
    if let Some(token) = bearer {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let resp = req
        .call()
        .map_err(|e| Error::Http(format!("GET {} failed: {e}", redact(url))))?;
    finish("GET", url, resp)
}

/// `POST url` with an `application/x-www-form-urlencoded` body built from `form`.
/// The body is encoded with `url::form_urlencoded` so values like the
/// `device_code` grant type are escaped correctly.
pub fn post_form(
    agent: &ureq::Agent,
    url: &str,
    form: &[(&str, &str)],
    bearer: Option<&str>,
) -> Result<HttpResponse> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form.iter().copied())
        .finish();
    let mut req = agent
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded");
    if let Some(token) = bearer {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let resp = req
        .send(body)
        .map_err(|e| Error::Http(format!("POST {} failed: {e}", redact(url))))?;
    finish("POST", url, resp)
}

/// `POST url` with no body (used for `/logout`).
pub fn post_empty(agent: &ureq::Agent, url: &str, bearer: Option<&str>) -> Result<HttpResponse> {
    let mut req = agent.post(url);
    if let Some(token) = bearer {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let resp = req
        .send_empty()
        .map_err(|e| Error::Http(format!("POST {} failed: {e}", redact(url))))?;
    finish("POST", url, resp)
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
