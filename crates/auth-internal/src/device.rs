//! RFC 8628 device-authorization grant, protocol only: [`start`] the flow, then
//! [`poll`] the token endpoint until the user approves in the browser. The CLI
//! never sees the user's Google/passkey credentials. Showing/opening the
//! verification URL and any waiting UX are the caller's job (the `peppy platform
//! login` command).

use std::time::Duration;

use serde::Deserialize;

use super::discovery::OidcEndpoints;
use super::http::HttpClient;
use super::storage::now_unix;
use crate::error::{Error, Result};

const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Tokens obtained from a successful device or refresh grant.
#[derive(Debug, Clone)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: String,
    /// Absolute expiry, unix seconds.
    pub expires_at: i64,
    pub token_type: String,
    pub scope: String,
}

/// A started device-authorization flow: what the caller shows the user
/// (`user_code`, `verification_uri`) and what [`poll`] exchanges for tokens.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    /// Flow lifetime in seconds; [`poll`] gives up past it.
    pub expires_in: i64,
    /// Server-suggested poll interval, seconds.
    #[serde(default = "default_interval")]
    pub interval: u64,
}

fn default_interval() -> u64 {
    5
}

/// The JSON body of a successful token-endpoint response (device or refresh
/// grant). Shared by [`device::poll`] and [`refresh::refresh`] so the parsing
/// and [`TokenSet`] materialization live in one place.
///
/// [`device::poll`]: super::device::poll
/// [`refresh::refresh`]: super::refresh::refresh
#[derive(Deserialize)]
pub(crate) struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: i64,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Deserialize)]
struct TokenErrorBody {
    #[serde(default)]
    error: String,
}

impl TokenResponse {
    /// Materializes a [`TokenSet`], carrying `prev_refresh` forward when the
    /// response omits a rotated refresh token (refresh grants may not re-issue
    /// one).
    pub(crate) fn into_set(self, now: i64, prev_refresh: Option<&str>) -> TokenSet {
        let refresh_token = self
            .refresh_token
            .filter(|t| !t.is_empty())
            .or_else(|| prev_refresh.map(str::to_string))
            .unwrap_or_default();
        TokenSet {
            access_token: self.access_token,
            refresh_token,
            expires_at: now + self.expires_in,
            token_type: self.token_type.unwrap_or_else(|| "Bearer".to_string()),
            scope: self.scope.unwrap_or_default(),
        }
    }
}

/// How a non-success poll of the token endpoint should be handled.
enum PollOutcome {
    KeepWaiting,
    SlowDown,
    Fatal(Error),
}

/// Classifies an OAuth error code from the token endpoint during device polling.
fn classify(error: &str) -> PollOutcome {
    match error {
        "authorization_pending" => PollOutcome::KeepWaiting,
        "slow_down" => PollOutcome::SlowDown,
        "access_denied" => PollOutcome::Fatal(Error::Auth(
            "authorization denied in the browser".to_string(),
        )),
        "expired_token" => PollOutcome::Fatal(Error::Auth(
            "login timed out, run `peppy platform login` again".to_string(),
        )),
        other => PollOutcome::Fatal(Error::Auth(format!("device login failed: {other}"))),
    }
}

/// Starts the device flow against `endpoints`, requesting `scopes` verbatim.
/// The caller shows the returned code/URL to the user, then exchanges the
/// authorization for tokens with [`poll`].
pub fn start(
    http: &HttpClient,
    endpoints: &OidcEndpoints,
    client_id: &str,
    scopes: &str,
) -> Result<DeviceAuthorization> {
    let start = http.post_form(
        &endpoints.device_authorization_endpoint,
        &[("client_id", client_id), ("scope", scopes)],
        None,
    )?;
    if !start.is_success() {
        return Err(Error::Auth(format!(
            "device authorization failed ({}): {}",
            start.status, start.body
        )));
    }
    start.json("device authorization")
}

/// Polls `token_endpoint` (blocking) until the user approves in the browser,
/// the flow expires, or the server reports a fatal error.
pub fn poll(
    http: &HttpClient,
    token_endpoint: &str,
    client_id: &str,
    da: &DeviceAuthorization,
) -> Result<TokenSet> {
    let deadline = now_unix() + da.expires_in;
    let mut interval = da.interval.max(1);

    loop {
        let resp = http.post_form(
            token_endpoint,
            &[
                ("grant_type", DEVICE_CODE_GRANT),
                ("device_code", &da.device_code),
                ("client_id", client_id),
            ],
            None,
        )?;

        if resp.is_success() {
            let token: TokenResponse = resp.json("token")?;
            break Ok(token.into_set(now_unix(), None));
        }

        let body: TokenErrorBody = serde_json::from_str(&resp.body).unwrap_or(TokenErrorBody {
            error: String::new(),
        });
        match classify(&body.error) {
            PollOutcome::KeepWaiting => {}
            PollOutcome::SlowDown => interval += 5,
            PollOutcome::Fatal(e) => break Err(e),
        }

        if now_unix() >= deadline {
            break Err(Error::Auth(
                "login timed out, run `peppy platform login` again".to_string(),
            ));
        }
        std::thread::sleep(Duration::from_secs(interval));
    }
}

#[cfg(test)]
mod tests {
    use httpmock::prelude::*;
    use serde_json::json;

    use super::*;

    /// A started flow pointing at a mocked token endpoint, with the poll pacing
    /// (`interval`, `expires_in`) chosen per test.
    fn device_auth(interval: u64, expires_in: i64) -> DeviceAuthorization {
        DeviceAuthorization {
            device_code: "dev-123".into(),
            user_code: "ABCD-EFGH".into(),
            verification_uri: "https://issuer.example/device".into(),
            verification_uri_complete: None,
            expires_in,
            interval,
        }
    }

    #[test]
    fn poll_returns_tokens_on_success() {
        let server = MockServer::start();
        let token = server.mock(|when, then| {
            when.method(POST).path("/token");
            then.status(200).json_body(json!({
                "access_token": "at",
                "refresh_token": "rt",
                "expires_in": 3600,
                "token_type": "Bearer",
                "scope": "openid",
            }));
        });

        let http = HttpClient::new();
        let set = poll(
            &http,
            &format!("{}/token", server.base_url()),
            "cli-client-id",
            &device_auth(1, 60),
        )
        .expect("approved flow yields tokens");
        assert_eq!(set.access_token, "at");
        assert_eq!(set.refresh_token, "rt");
        assert_eq!(token.calls(), 1, "success on the first poll: no re-poll");
    }

    /// `authorization_pending` re-polls at the server interval until the flow
    /// deadline, then fails with the user-actionable timeout error.
    #[test]
    fn poll_repolls_while_pending_until_the_deadline() {
        let server = MockServer::start();
        let pending = server.mock(|when, then| {
            when.method(POST).path("/token");
            then.status(400)
                .json_body(json!({ "error": "authorization_pending" }));
        });

        let http = HttpClient::new();
        // 1s interval against a 2s flow lifetime: pending at ~t0 and ~t1, then
        // the deadline check breaks the loop.
        let err = poll(
            &http,
            &format!("{}/token", server.base_url()),
            "cli-client-id",
            &device_auth(1, 2),
        )
        .expect_err("an unapproved flow times out");
        assert!(matches!(err, Error::Auth(_)));
        assert!(err.to_string().contains("login timed out"));
        assert!(
            pending.calls() >= 2,
            "pending must re-poll at the interval, not give up on the first response"
        );
    }

    /// `slow_down` backs the interval off (+5s, RFC 8628): with a 1s starting
    /// interval and a 4s lifetime, an honored backoff re-polls exactly once (at
    /// ~6s, past the deadline); an ignored one would keep polling every second.
    #[test]
    fn poll_slow_down_backs_off_the_interval() {
        let server = MockServer::start();
        let slow = server.mock(|when, then| {
            when.method(POST).path("/token");
            then.status(400).json_body(json!({ "error": "slow_down" }));
        });

        let http = HttpClient::new();
        let err = poll(
            &http,
            &format!("{}/token", server.base_url()),
            "cli-client-id",
            &device_auth(1, 4),
        )
        .expect_err("times out past the flow lifetime");
        assert!(err.to_string().contains("login timed out"));
        assert_eq!(
            slow.calls(),
            2,
            "the +5s backoff allows exactly one re-poll before the 4s deadline"
        );
    }

    /// A fatal OAuth code (`expired_token`) fails on the spot: no re-poll and no
    /// waiting out the interval or the local deadline.
    #[test]
    fn poll_expired_token_fails_immediately() {
        let server = MockServer::start();
        let expired = server.mock(|when, then| {
            when.method(POST).path("/token");
            then.status(400)
                .json_body(json!({ "error": "expired_token" }));
        });

        let http = HttpClient::new();
        let started = std::time::Instant::now();
        let err = poll(
            &http,
            &format!("{}/token", server.base_url()),
            "cli-client-id",
            // A generous lifetime proves the error comes from the fatal code,
            // not the local deadline.
            &device_auth(1, 60),
        )
        .expect_err("expired_token is fatal");
        assert!(matches!(err, Error::Auth(_)));
        assert!(err.to_string().contains("login timed out"));
        assert_eq!(expired.calls(), 1, "fatal: no re-poll");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "fatal errors do not sleep out the interval"
        );
    }

    #[test]
    fn classify_maps_oauth_codes() {
        assert!(matches!(
            classify("authorization_pending"),
            PollOutcome::KeepWaiting
        ));
        assert!(matches!(classify("slow_down"), PollOutcome::SlowDown));
        assert!(matches!(classify("access_denied"), PollOutcome::Fatal(_)));
        assert!(matches!(classify("expired_token"), PollOutcome::Fatal(_)));
        assert!(matches!(classify("anything_else"), PollOutcome::Fatal(_)));
    }

    #[test]
    fn into_set_carries_refresh_forward_when_absent() {
        let resp = TokenResponse {
            access_token: "a".into(),
            refresh_token: None,
            expires_in: 60,
            token_type: None,
            scope: None,
        };
        let set = resp.into_set(1_000, Some("old-refresh"));
        assert_eq!(set.refresh_token, "old-refresh");
        assert_eq!(set.expires_at, 1_060);
        assert_eq!(set.token_type, "Bearer");
    }

    #[test]
    fn into_set_prefers_rotated_refresh() {
        let resp = TokenResponse {
            access_token: "a".into(),
            refresh_token: Some("new".into()),
            expires_in: 30,
            token_type: Some("Bearer".into()),
            scope: Some("openid".into()),
        };
        let set = resp.into_set(0, Some("old"));
        assert_eq!(set.refresh_token, "new");
    }
}
