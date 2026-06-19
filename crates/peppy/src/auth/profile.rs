//! Runtime profile resolution. A profile is essentially a **named backend URL**;
//! everything else (`issuer`, `client_id`, `scopes`) is discovered at runtime
//! from `GET {api_url}/cli-config`.
//!
//! The default profile follows the compile-time build (debug → `dev`, release →
//! `prod`), matching how the peppy data root already splits, but it can be
//! overridden at runtime — independent of that build flag — via `--env` /
//! `PEPPY_PROFILE` / `PEPPY_ENV`. The backend URL precedence is
//! `--api-url` → `PEPPY_API_URL` → the profile's built-in default.

use crate::error::{Error, Result};

const DEV_API_URL: &str = "http://127.0.0.1:3000";
const PROD_API_URL: &str = "https://api.peppy.bot";

/// A resolved profile: its name and the backend base URL to talk to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub api_url: String,
}

/// The default profile name for this build (`dev` in debug, `prod` in release).
fn default_profile_name() -> &'static str {
    if cfg!(debug_assertions) {
        "dev"
    } else {
        "prod"
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Built-in api_url for the well-known profiles; `None` for a custom name (which
/// then requires `--api-url` or `PEPPY_API_URL`).
fn builtin_api_url(profile: &str) -> Option<&'static str> {
    match profile {
        "dev" => Some(DEV_API_URL),
        "prod" => Some(PROD_API_URL),
        _ => None,
    }
}

/// Resolves the active profile from the `--env`/`--api-url` flags plus the
/// environment. Reads process env directly; pass the flags through from clap.
pub fn resolve(env_flag: Option<&str>, api_url_flag: Option<&str>) -> Result<Profile> {
    resolve_from(
        env_flag,
        api_url_flag,
        env_nonempty("PEPPY_PROFILE").or_else(|| env_nonempty("PEPPY_ENV")),
        env_nonempty("PEPPY_API_URL"),
    )
}

/// Pure core of [`resolve`] with the env inputs made explicit, so precedence and
/// the transport guard can be unit-tested without mutating process env.
pub fn resolve_from(
    env_flag: Option<&str>,
    api_url_flag: Option<&str>,
    env_profile: Option<String>,
    env_api_url: Option<String>,
) -> Result<Profile> {
    let name = env_flag
        .map(str::to_string)
        .or(env_profile)
        .unwrap_or_else(|| default_profile_name().to_string());

    let api_url = api_url_flag
        .map(str::to_string)
        .or(env_api_url)
        .or_else(|| builtin_api_url(&name).map(str::to_string))
        .ok_or_else(|| {
            Error::Auth(format!(
                "no backend URL for profile `{name}`: pass --api-url or set PEPPY_API_URL"
            ))
        })?;

    let api_url = api_url.trim_end_matches('/').to_string();
    check_transport(&api_url)?;
    Ok(Profile { name, api_url })
}

/// Plain `http` is allowed only for loopback / `*.localhost` (local dev);
/// anything else must be `https` so prod tokens never travel in cleartext.
fn check_transport(api_url: &str) -> Result<()> {
    let parsed = url::Url::parse(api_url)
        .map_err(|e| Error::Auth(format!("invalid backend URL `{api_url}`: {e}")))?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if is_local(parsed.host_str()) => Ok(()),
        "http" => Err(Error::Auth(format!(
            "refusing plain http for non-local backend `{api_url}` (use https)"
        ))),
        other => Err(Error::Auth(format!(
            "unsupported URL scheme `{other}` in `{api_url}`"
        ))),
    }
}

/// Whether a host is local enough to allow plain http: loopback addresses,
/// `localhost`, or any `*.localhost` name (Zitadel's dev issuer uses the latter).
fn is_local(host: Option<&str>) -> bool {
    match host {
        Some(h) => {
            h == "localhost"
                || h.ends_with(".localhost")
                || h == "127.0.0.1"
                || h == "::1"
                || h.starts_with("127.")
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_beats_env_beats_builtin() {
        // --api-url wins outright.
        let p = resolve_from(
            Some("dev"),
            Some("http://localhost:9999"),
            None,
            Some("http://127.0.0.1:1".into()),
        )
        .expect("resolve");
        assert_eq!(p.api_url, "http://localhost:9999");

        // env api_url beats the builtin default.
        let p = resolve_from(Some("dev"), None, None, Some("http://127.0.0.1:1".into()))
            .expect("resolve");
        assert_eq!(p.api_url, "http://127.0.0.1:1");

        // builtin default for the named profile.
        let p = resolve_from(Some("dev"), None, None, None).expect("resolve");
        assert_eq!(p.api_url, DEV_API_URL);
    }

    #[test]
    fn env_profile_selects_name_when_no_flag() {
        let p = resolve_from(None, None, Some("prod".into()), None).expect("resolve");
        assert_eq!(p.name, "prod");
        assert_eq!(p.api_url, PROD_API_URL);
    }

    #[test]
    fn trailing_slash_trimmed() {
        let p =
            resolve_from(Some("dev"), Some("http://localhost:3000/"), None, None).expect("resolve");
        assert_eq!(p.api_url, "http://localhost:3000");
    }

    #[test]
    fn custom_profile_requires_a_url() {
        let err = resolve_from(Some("staging"), None, None, None).unwrap_err();
        assert!(err.to_string().contains("no backend URL"));
    }

    #[test]
    fn rejects_plain_http_for_remote_host() {
        let err = resolve_from(Some("prod"), Some("http://api.peppy.bot"), None, None).unwrap_err();
        assert!(err.to_string().contains("plain http"));
    }

    #[test]
    fn allows_https_and_local_http() {
        assert!(resolve_from(Some("prod"), Some("https://api.peppy.bot"), None, None).is_ok());
        assert!(
            resolve_from(
                Some("dev"),
                Some("http://auth.peppy.localhost:8080"),
                None,
                None
            )
            .is_ok()
        );
        assert!(resolve_from(Some("dev"), Some("http://127.0.0.1:3000"), None, None).is_ok());
    }
}
