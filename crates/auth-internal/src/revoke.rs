//! RFC 7009 token revocation at the issuer. `peppy platform logout` revokes the
//! refresh token and then the access token, so neither outlives the local
//! session file. The backend keeps no session of its own to end: it checks
//! every bearer with the issuer, which stops honouring a revoked one at once.

use super::http::HttpClient;
use crate::error::{Error, Result};

/// The `token_type_hint` values RFC 7009 defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    AccessToken,
    RefreshToken,
}

impl TokenKind {
    fn hint(self) -> &'static str {
        match self {
            Self::AccessToken => "access_token",
            Self::RefreshToken => "refresh_token",
        }
    }
}

/// Revokes `token` at `revocation_endpoint` as the public client `client_id`.
/// The issuer answers `200` whether or not the token was still valid (RFC 7009
/// section 2.2), so only a transport failure or another status is an error.
pub fn revoke_token(
    http: &HttpClient,
    revocation_endpoint: &str,
    client_id: &str,
    token: &str,
    kind: TokenKind,
) -> Result<()> {
    let resp = http.post_form(
        revocation_endpoint,
        &[
            ("token", token),
            ("token_type_hint", kind.hint()),
            ("client_id", client_id),
        ],
        None,
    )?;
    if resp.is_success() {
        return Ok(());
    }
    Err(Error::Auth(format!(
        "revoking the {} failed ({})",
        kind.hint().replace('_', " "),
        resp.status
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    #[test]
    fn revocation_posts_the_rfc_7009_form() {
        let server = MockServer::start();
        let revoke = server.mock(|when, then| {
            when.method(POST)
                .path("/oauth/v2/revoke")
                .header("content-type", "application/x-www-form-urlencoded")
                .body("token=rt-123&token_type_hint=refresh_token&client_id=cli");
            then.status(200);
        });

        revoke_token(
            &HttpClient::new(),
            &format!("{}/oauth/v2/revoke", server.base_url()),
            "cli",
            "rt-123",
            TokenKind::RefreshToken,
        )
        .expect("revocation succeeds");
        assert_eq!(revoke.calls(), 1);
    }

    #[test]
    fn a_non_success_status_is_an_error_naming_the_token() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/oauth/v2/revoke");
            then.status(500);
        });

        let err = revoke_token(
            &HttpClient::new(),
            &format!("{}/oauth/v2/revoke", server.base_url()),
            "cli",
            "at-123",
            TokenKind::AccessToken,
        )
        .expect_err("a 500 is an error");
        assert!(err.to_string().contains("access token"), "{err}");
    }
}
