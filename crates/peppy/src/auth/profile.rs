//! Backend URL resolution for the CLI auth commands.
//!
//! The backend the commands talk to is fixed by the build: the local dev
//! backend in debug builds, the prod backend in release. Its URL is stored in
//! the `resource_servers` block of `peppy_config.json5` (seeded with the build
//! default) and can be overridden at runtime via `--api-url` / `PEPPY_API_URL`.

use crate::error::{Error, Result};
use config::peppy_config::ResourceServers;

/// Resolves the backend base URL in precedence order: the `--api-url` flag,
/// then `PEPPY_API_URL`, then the build's URL from the `resource_servers`
/// block. Reads process env directly; pass the flag through from clap.
pub fn resolve_api_url(api_url_flag: Option<&str>, servers: &ResourceServers) -> Result<String> {
    resolve_api_url_from(api_url_flag, env_nonempty("PEPPY_API_URL"), servers)
}

/// Pure core of [`resolve_api_url`] with the env input made explicit, so
/// precedence and the transport guard can be unit-tested without mutating
/// process env.
pub fn resolve_api_url_from(
    api_url_flag: Option<&str>,
    env_api_url: Option<String>,
    servers: &ResourceServers,
) -> Result<String> {
    let api_url = api_url_flag
        .map(str::to_string)
        .or(env_api_url)
        .unwrap_or_else(|| servers.api.clone());

    let api_url = api_url.trim_end_matches('/').to_string();
    check_transport(&api_url)?;
    Ok(api_url)
}

/// The backend environment label for this build (`dev` in debug, `prod` in
/// release). Display-only; the URL itself comes from the `resource_servers`
/// block.
pub fn build_env_name() -> &'static str {
    if cfg!(debug_assertions) {
        "dev"
    } else {
        "prod"
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
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
    use config::peppy_config::DEFAULT_API_URL;

    /// The default `resource_servers` block, the built-in fallback every test
    /// resolves against unless it overrides the URL explicitly.
    fn servers() -> ResourceServers {
        ResourceServers::default()
    }

    #[test]
    fn flag_beats_env_beats_block() {
        let servers = servers();
        // --api-url wins outright.
        let url = resolve_api_url_from(
            Some("http://localhost:9999"),
            Some("http://127.0.0.1:1".into()),
            &servers,
        )
        .expect("resolve");
        assert_eq!(url, "http://localhost:9999");

        // PEPPY_API_URL beats the block default.
        let url = resolve_api_url_from(None, Some("http://127.0.0.1:1".into()), &servers)
            .expect("resolve");
        assert_eq!(url, "http://127.0.0.1:1");

        // The block's api is the fallback.
        let url = resolve_api_url_from(None, None, &servers).expect("resolve");
        assert_eq!(url, DEFAULT_API_URL);
    }

    #[test]
    fn build_env_name_follows_the_build() {
        let name = if cfg!(debug_assertions) {
            "dev"
        } else {
            "prod"
        };
        assert_eq!(build_env_name(), name);
    }

    #[test]
    fn block_api_is_authoritative() {
        // Editing resource_servers.api changes what gets resolved.
        let servers = ResourceServers {
            api: "http://localhost:9000".into(),
        };
        let url = resolve_api_url_from(None, None, &servers).expect("resolve");
        assert_eq!(url, "http://localhost:9000");
    }

    #[test]
    fn trailing_slash_trimmed() {
        let url = resolve_api_url_from(Some("http://localhost:3000/"), None, &servers())
            .expect("resolve");
        assert_eq!(url, "http://localhost:3000");
    }

    #[test]
    fn rejects_plain_http_for_remote_host() {
        let err = resolve_api_url_from(Some("http://api.peppy.bot"), None, &servers()).unwrap_err();
        assert!(err.to_string().contains("plain http"));
    }

    #[test]
    fn allows_https_and_local_http() {
        let servers = servers();
        assert!(resolve_api_url_from(Some("https://api.peppy.bot"), None, &servers).is_ok());
        assert!(
            resolve_api_url_from(Some("http://auth.peppy.localhost:8080"), None, &servers).is_ok()
        );
        assert!(resolve_api_url_from(Some("http://127.0.0.1:3000"), None, &servers).is_ok());
    }
}
