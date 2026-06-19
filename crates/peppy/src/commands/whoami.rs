//! `peppy whoami` (alias `status`) — resolve the active credential, call
//! `GET /me`, and print the identity, profile, and token validity. `--json`
//! emits a machine-readable object (never including raw tokens).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::Command;
use crate::auth::client::Principal;
use crate::auth::{client, http, profile, resolver, storage};
use crate::context::AppContext;
use crate::error::{Error, Result};

pub struct WhoamiCommand {
    pub env: Option<String>,
    pub api_url: Option<String>,
    /// Emit machine-readable JSON instead of human text.
    pub json: bool,
    /// Test seam: override the credentials file.
    pub credentials_file: Option<PathBuf>,
}

impl Command for WhoamiCommand {
    fn execute(self, _ctx: &Arc<AppContext>) -> Result<()> {
        let profile = profile::resolve(self.env.as_deref(), self.api_url.as_deref())?;
        let creds_path = self.credentials_file.unwrap_or_else(storage::default_path);
        let agent = http::agent();
        let pat = std::env::var("PEPPY_API_KEY")
            .ok()
            .filter(|v| !v.is_empty());

        match resolver::resolve(&profile, &creds_path, &agent, pat) {
            Ok(mut cred) => {
                let principal = client::get_me(&agent, &profile.api_url, &mut cred)?;
                let expires_at = session_expiry(&creds_path, &profile.name);
                if self.json {
                    print_json(&profile.name, &profile.api_url, &principal, expires_at);
                } else {
                    print_human(&profile.name, &profile.api_url, &principal, expires_at);
                }
                Ok(())
            }
            Err(Error::NotAuthenticated) => {
                if self.json {
                    let doc = serde_json::json!({
                        "authenticated": false,
                        "profile": profile.name,
                        "api_url": profile.api_url,
                    });
                    println!("{doc}");
                } else {
                    println!("Not authenticated ({}). Run `peppy login`.", profile.name);
                }
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

/// Reads the stored access-token expiry (unix seconds) for a session profile, if
/// any. A PAT has no stored expiry, so this returns `None`.
fn session_expiry(creds_path: &Path, profile: &str) -> Option<i64> {
    storage::load(creds_path)
        .ok()?
        .profiles
        .get(profile)
        .map(|pc| pc.expires_at)
}

fn token_is_valid(expires_at: Option<i64>) -> bool {
    // No stored expiry (PAT) but `/me` succeeded → treat as valid.
    match expires_at {
        None => true,
        Some(exp) => storage::now_unix() < exp,
    }
}

fn print_human(profile: &str, api_url: &str, p: &Principal, expires_at: Option<i64>) {
    println!("Logged in as {} ({profile})", p.display_name());
    println!("  subject : {}", p.sub);
    if let Some(kind) = &p.kind {
        println!("  type    : {kind}");
    }
    if let Some(role) = &p.role {
        println!("  role    : {role}");
    }
    if let Some(email) = &p.email {
        println!("  email   : {email}");
    }
    println!("  backend : {api_url}");
    let token = if token_is_valid(expires_at) {
        "valid"
    } else {
        "expired"
    };
    println!("  token   : {token}");
}

fn print_json(profile: &str, api_url: &str, p: &Principal, expires_at: Option<i64>) {
    let doc = serde_json::json!({
        "authenticated": true,
        "profile": profile,
        "api_url": api_url,
        "principal": {
            "id": p.id,
            "sub": p.sub,
            "kind": p.kind,
            "username": p.username,
            "email": p.email,
            "role": p.role,
        },
        "token": {
            "valid": token_is_valid(expires_at),
            "expires_at": expires_at,
        },
    });
    println!("{doc}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_validity_handles_pat_and_expiry() {
        assert!(token_is_valid(None), "PAT (no expiry) reads as valid");
        assert!(token_is_valid(Some(storage::now_unix() + 60)));
        assert!(!token_is_valid(Some(storage::now_unix() - 60)));
    }
}
