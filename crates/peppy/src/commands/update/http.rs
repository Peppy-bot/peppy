//! Release HTTP client: version probing via redirect chain and streaming
//! archive download with TLS verification and HTTPS downgrade protection.

use std::io::{self, IsTerminal, Read};
use std::path::Path;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};
use ureq::tls::{RootCerts, TlsConfig};
use url::Url;

use super::archive;
use super::version::{ReleaseVersion, extract_release_tag_from_url};
use crate::error::{Error, Result};

/// Maximum number of HTTP redirects followed for a single release request.
const MAX_REDIRECTS: usize = 10;

/// Timeout for lightweight `HEAD` probes that resolve the latest release.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-phase timeouts for streaming the release archive. No global timeout is
/// set so a slow but active download of a multi-hundred-MB archive is not cut
/// off prematurely.
const DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

/// Resolved release metadata from probing the download endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedRelease {
    pub version: ReleaseVersion,
    pub download_url: String,
    pub content_length: Option<u64>,
}

/// Builds a `ureq::Agent` configured with the host platform trust roots and no
/// automatic redirect following so every redirect hop is checked for HTTPS
/// downgrades and release tags.
fn build_agent(global_timeout: Option<Duration>) -> ureq::Agent {
    let mut builder = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .tls_config(
            TlsConfig::builder()
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .max_redirects(0)
        .timeout_connect(Some(DOWNLOAD_CONNECT_TIMEOUT))
        .timeout_send_request(Some(DOWNLOAD_CONNECT_TIMEOUT))
        .timeout_recv_response(Some(DOWNLOAD_RESPONSE_TIMEOUT));
    if let Some(timeout) = global_timeout {
        builder = builder.timeout_global(Some(timeout));
    }
    builder.build().into()
}

/// Strips the query string and credentials from a URL before printing it in
/// error messages.
pub(crate) fn redact_url(raw: &str) -> String {
    let Ok(mut parsed) = Url::parse(raw) else {
        return raw.split('?').next().unwrap_or(raw).to_owned();
    };
    parsed.set_query(None);
    parsed.set_fragment(None);
    if !parsed.username().is_empty() {
        let _ = parsed.set_username("***");
    }
    if parsed.password().is_some() {
        let _ = parsed.set_password(Some("***"));
    }
    parsed.into()
}

/// Follows redirects manually for either `HEAD` or `GET`, refusing any
/// redirect that downgrades an `https://` request to plain `http://`.
///
/// Returns the final response together with the last release version tag
/// observed along the redirect chain.
fn follow_redirects(
    agent: &ureq::Agent,
    method: HttpMethod,
    initial_url: &str,
    archive_name: &str,
) -> Result<(ureq::http::Response<ureq::Body>, Option<ReleaseVersion>)> {
    let mut current = Url::parse(initial_url).map_err(|e| {
        Error::ExecutionFailed(format!(
            "Invalid release URL `{}`: {e}",
            redact_url(initial_url)
        ))
    })?;
    let require_https = current.scheme() == "https";
    let mut discovered_version = extract_release_tag_from_url(&current, archive_name);

    for _ in 0..=MAX_REDIRECTS {
        let current_str = current.as_str();
        let response = match method {
            HttpMethod::Head => agent.head(current_str).call(),
            HttpMethod::Get => agent.get(current_str).call(),
        }
        .map_err(|e| {
            Error::ExecutionFailed(format!(
                "{} {} failed: {e}",
                method.as_str(),
                redact_url(current_str)
            ))
        })?;

        let status = response.status().as_u16();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let location = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| {
                    Error::ExecutionFailed(format!(
                        "{} {} returned HTTP {status} without a Location header",
                        method.as_str(),
                        redact_url(current_str)
                    ))
                })?;
            let next = resolve_redirect_target(&current, location, require_https)?;
            if let Some(tag) = extract_release_tag_from_url(&next, archive_name) {
                discovered_version = Some(tag);
            }
            current = next;
            continue;
        }

        return Ok((response, discovered_version));
    }

    Err(Error::ExecutionFailed(format!(
        "Too many redirects (more than {MAX_REDIRECTS}) when requesting {}",
        redact_url(initial_url)
    )))
}

/// Resolves a `Location` header against `current`, refusing any downgrade from
/// `https` to plain `http` when `require_https` is set.
fn resolve_redirect_target(current: &Url, location: &str, require_https: bool) -> Result<Url> {
    let next = current.join(location).map_err(|e| {
        Error::ExecutionFailed(format!(
            "Invalid redirect target from {}: {e}",
            redact_url(current.as_str())
        ))
    })?;
    if require_https && next.scheme() != "https" {
        return Err(Error::ExecutionFailed(format!(
            "Refusing insecure redirect from {} to plain HTTP (`{}`)",
            redact_url(current.as_str()),
            redact_url(next.as_str())
        )));
    }
    Ok(next)
}

#[derive(Clone, Copy)]
enum HttpMethod {
    Head,
    Get,
}

impl HttpMethod {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Head => "HEAD",
            Self::Get => "GET",
        }
    }
}

fn content_length_header(response: &ureq::http::Response<ureq::Body>) -> Option<u64> {
    response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
}

/// Probes the release server with `HEAD` to resolve the target release version
/// and confirm the archive for `archive_name` is available.
///
/// The returned `download_url` is pinned to the resolved version tag
/// (`{base_url}/{version}/{archive_name}`) so that a release published between
/// the check and the download cannot change what gets installed, and signed
/// CDN redirects do not expire while waiting on a user prompt.
pub(crate) fn probe_release(
    base_url: &str,
    archive_name: &str,
    requested: Option<ReleaseVersion>,
) -> Result<ResolvedRelease> {
    let base = base_url.trim_end_matches('/');
    let probe_url = match requested {
        Some(version) => format!("{base}/{version}/{archive_name}"),
        None => format!("{base}/latest/{archive_name}"),
    };

    let agent = build_agent(Some(PROBE_TIMEOUT));
    let (response, discovered_version) =
        follow_redirects(&agent, HttpMethod::Head, &probe_url, archive_name)?;

    let status = response.status().as_u16();
    if status == 404 {
        return match requested {
            Some(version) => Err(Error::ExecutionFailed(format!(
                "Release {version} is not available for this platform (`{archive_name}` returned HTTP 404)."
            ))),
            None => Err(Error::ExecutionFailed(format!(
                "Latest release archive `{archive_name}` was not found at {} (HTTP 404).",
                redact_url(&probe_url)
            ))),
        };
    }
    if !(200..300).contains(&status) {
        return Err(Error::ExecutionFailed(format!(
            "Failed to check release at {} (HTTP {status}).",
            redact_url(&probe_url)
        )));
    }

    let version = match requested {
        Some(version) => version,
        None => discovered_version.ok_or_else(|| {
            Error::ExecutionFailed(format!(
                "Could not determine the latest release version from redirects of {}.",
                redact_url(&probe_url)
            ))
        })?,
    };

    let content_length = content_length_header(&response);
    let download_url = format!("{base}/{version}/{archive_name}");

    Ok(ResolvedRelease {
        version,
        download_url,
        content_length,
    })
}

/// Downloads the release `.tgz` archive at `url` and streams it directly into
/// `staging_dir` with safe tar extraction and gzip checksum verification.
pub(crate) fn download_and_extract(
    url: &str,
    archive_name: &str,
    expected_length: Option<u64>,
    staging_dir: &Path,
) -> Result<()> {
    let agent = build_agent(None);
    let (mut response, _) = follow_redirects(&agent, HttpMethod::Get, url, archive_name)?;

    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(Error::ExecutionFailed(format!(
            "Failed to download release archive from {} (HTTP {status}).",
            redact_url(url)
        )));
    }

    let total_bytes = content_length_header(&response).or(expected_length);
    let progress = download_progress_bar(total_bytes);
    let reader = ProgressReader::new(response.body_mut().as_reader(), progress.clone());

    let extract_result = archive::extract_tgz_safely(reader, staging_dir);
    if let Some(pb) = progress {
        pb.finish_and_clear();
    }
    extract_result
}

fn download_progress_bar(total_bytes: Option<u64>) -> Option<ProgressBar> {
    if !io::stderr().is_terminal() {
        return None;
    }
    let pb = match total_bytes {
        Some(len) if len > 0 => {
            let pb = ProgressBar::new(len);
            if let Ok(style) = ProgressStyle::with_template(
                "{spinner} Downloading [{bar:32}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})",
            ) {
                pb.set_style(style.progress_chars("=> "));
            }
            pb
        }
        _ => {
            let pb = ProgressBar::new_spinner();
            if let Ok(style) = ProgressStyle::with_template("{spinner} Downloading {bytes}") {
                pb.set_style(style);
            }
            pb
        }
    };
    pb.enable_steady_tick(Duration::from_millis(120));
    Some(pb)
}

/// Adapter that advances an optional `indicatif::ProgressBar` as bytes are read
/// from the HTTP response body.
struct ProgressReader<R> {
    inner: R,
    progress: Option<ProgressBar>,
}

impl<R> ProgressReader<R> {
    const fn new(inner: R, progress: Option<ProgressBar>) -> Self {
        Self { inner, progress }
    }
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buf)?;
        if count > 0
            && let Some(pb) = &self.progress
        {
            pb.inc(count as u64);
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::Method::{GET, HEAD};
    use httpmock::MockServer;

    const ARCHIVE: &str = "peppy-x86_64-unknown-linux-gnu.tgz";

    #[test]
    fn redact_url_strips_query_fragment_and_credentials() {
        assert_eq!(
            redact_url("https://user:secret@example.com/v0.39.0/a.tgz?sig=abc&jwt=xyz#frag"),
            "https://***:***@example.com/v0.39.0/a.tgz"
        );
        assert_eq!(
            redact_url("https://peppy.bot/latest/peppy.tgz"),
            "https://peppy.bot/latest/peppy.tgz"
        );
    }

    #[test]
    fn probe_release_follows_latest_redirect_chain_and_pins_version() {
        let server = MockServer::start();
        let latest = server.mock(|when, then| {
            when.method(HEAD).path(format!("/latest/{ARCHIVE}"));
            then.status(302).header(
                "Location",
                format!("{}/releases/latest/download/{ARCHIVE}", server.base_url()),
            );
        });
        let gh_latest = server.mock(|when, then| {
            when.method(HEAD)
                .path(format!("/releases/latest/download/{ARCHIVE}"));
            then.status(302).header(
                "Location",
                format!("{}/releases/download/v0.39.0/{ARCHIVE}", server.base_url()),
            );
        });
        let gh_tag = server.mock(|when, then| {
            when.method(HEAD)
                .path(format!("/releases/download/v0.39.0/{ARCHIVE}"));
            then.status(302).header(
                "Location",
                format!("{}/cdn/asset?sig=secret", server.base_url()),
            );
        });
        let cdn = server.mock(|when, then| {
            when.method(HEAD).path("/cdn/asset");
            then.status(200).header("Content-Length", "12345");
        });

        let resolved =
            probe_release(&server.base_url(), ARCHIVE, None).expect("probe should resolve v0.39.0");

        assert_eq!(resolved.version, ReleaseVersion::parse("v0.39.0").unwrap());
        assert_eq!(
            resolved.download_url,
            format!("{}/v0.39.0/{ARCHIVE}", server.base_url())
        );
        assert_eq!(resolved.content_length, Some(12345));
        latest.assert_calls(1);
        gh_latest.assert_calls(1);
        gh_tag.assert_calls(1);
        cdn.assert_calls(1);
    }

    #[test]
    fn probe_release_reports_404_for_missing_pinned_version() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(HEAD).path(format!("/v0.99.0/{ARCHIVE}"));
            then.status(404);
        });

        let version = ReleaseVersion::parse("v0.99.0").unwrap();
        let err = probe_release(&server.base_url(), ARCHIVE, Some(version))
            .expect_err("missing version should fail");
        assert!(err.to_string().contains("v0.99.0"), "{err}");
        assert!(err.to_string().contains("404"), "{err}");
    }

    #[test]
    fn probe_release_fails_when_latest_has_no_version_in_redirects() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(HEAD).path(format!("/latest/{ARCHIVE}"));
            then.status(200);
        });

        let err = probe_release(&server.base_url(), ARCHIVE, None)
            .expect_err("unversioned latest should fail");
        assert!(
            err.to_string()
                .contains("Could not determine the latest release version"),
            "{err}"
        );
    }

    #[test]
    fn follow_redirects_stops_on_redirect_loop() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(HEAD).path("/loop");
            then.status(302)
                .header("Location", format!("{}/loop", server.base_url()));
        });

        let agent = build_agent(Some(PROBE_TIMEOUT));
        let err = follow_redirects(
            &agent,
            HttpMethod::Head,
            &format!("{}/loop", server.base_url()),
            ARCHIVE,
        )
        .expect_err("redirect loop should fail");
        assert!(err.to_string().contains("Too many redirects"), "{err}");
    }

    #[test]
    fn download_and_extract_streams_valid_archive() {
        let server = MockServer::start();
        let tgz =
            archive::test_support::build_tgz(&[("bin/peppy", b"#!/bin/sh\necho ok\n", 0o755)]);
        server.mock(|when, then| {
            when.method(GET).path(format!("/v0.40.0/{ARCHIVE}"));
            then.status(200).body(tgz.clone());
        });

        let staging = tempfile::tempdir().unwrap();
        download_and_extract(
            &format!("{}/v0.40.0/{ARCHIVE}", server.base_url()),
            ARCHIVE,
            None,
            staging.path(),
        )
        .expect("archive should download and extract");

        assert!(staging.path().join("bin/peppy").is_file());
    }

    #[test]
    fn resolve_redirect_target_refuses_https_to_http_downgrade() {
        let https = Url::parse("https://peppy.bot/latest/peppy.tgz").unwrap();
        let err = resolve_redirect_target(&https, "http://evil.example/peppy.tgz?token=1", true)
            .expect_err("HTTPS downgrade must be refused");
        let msg = err.to_string();
        assert!(msg.contains("plain HTTP"), "{msg}");
        assert!(!msg.contains("token=1"), "query should be redacted: {msg}");

        let ok = resolve_redirect_target(&https, "/v0.39.0/peppy.tgz", true)
            .expect("relative HTTPS redirect should resolve");
        assert_eq!(ok.as_str(), "https://peppy.bot/v0.39.0/peppy.tgz");
    }
}
