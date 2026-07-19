//! Backend URL resolution for the CLI auth commands.
//!
//! The backend the commands talk to is fixed by the build: the local dev
//! backend in debug builds, the prod backend in release. Its URL is stored in
//! the `resource_servers` block of `peppy_config.json5` (seeded with the build
//! default) and can be overridden at runtime via `--api-url` / `PEPPY_API_URL`.

use crate::error::{Error, Result};
use daemon_config::peppy_config::ResourceServers;
use url::{Host, Url};

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
    let parsed = validate_https_or_local(&api_url, "platform API")?;
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(Error::Auth(
            "platform API URL must not contain a query string or fragment".to_string(),
        ));
    }
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

/// Validate one control-plane URL. Plain `http` is allowed only for an actual
/// loopback address, `localhost`, or `*.localhost`; anything else must use
/// `https` so tokens and certificate enrollment never travel in cleartext.
///
/// This is shared by platform API resolution and OIDC discovery. Returning the
/// parsed URL lets callers additionally compare schemes/origins without parsing
/// a security-sensitive value a second way.
pub fn validate_https_or_local(raw: &str, what: &str) -> Result<Url> {
    if raw.trim() != raw || raw.is_empty() {
        return Err(Error::Auth(format!(
            "invalid {what} URL: it must be non-empty and contain no surrounding whitespace"
        )));
    }
    let parsed = Url::parse(raw).map_err(|e| Error::Auth(format!("invalid {what} URL: {e}")))?;
    if parsed.host().is_none() || parsed.cannot_be_a_base() {
        return Err(Error::Auth(format!(
            "invalid {what} URL: an absolute URL with a host is required"
        )));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(Error::Auth(format!(
            "invalid {what} URL: embedded credentials are not allowed"
        )));
    }
    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" if is_local(&parsed) => Ok(parsed),
        "http" => Err(Error::Auth(format!(
            "refusing plain http for non-local {what} (use https)"
        ))),
        other => Err(Error::Auth(format!(
            "unsupported URL scheme `{other}` for {what}"
        ))),
    }
}

/// Canonical scheme/host/port binding used by the core-node certificate
/// metadata. Paths, queries, fragments, case differences, and explicit default
/// ports do not create a second platform origin.
pub fn normalize_api_origin(api_url: &str) -> Result<String> {
    Ok(validate_https_or_local(api_url, "platform API")?
        .origin()
        .ascii_serialization())
}

/// Whether a parsed host is explicitly local enough to permit cleartext
/// development traffic. Using `Host` avoids the old string-prefix pitfall where
/// a domain such as `127.example` could be mistaken for a loopback address.
fn is_local(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(host)) => host == "localhost" || host.ends_with(".localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use daemon_config::peppy_config::DEFAULT_API_URL;

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
    fn domain_names_that_merely_start_like_loopback_are_not_local() {
        for url in ["http://127.example", "http://127.evil.test"] {
            let error = resolve_api_url_from(Some(url), None, &servers()).unwrap_err();
            assert!(error.to_string().contains("plain http"), "{url}: {error}");
        }
    }

    #[test]
    fn allows_https_and_local_http() {
        let servers = servers();
        assert!(resolve_api_url_from(Some("https://api.peppy.bot"), None, &servers).is_ok());
        assert!(
            resolve_api_url_from(Some("http://auth.peppy.localhost:8080"), None, &servers).is_ok()
        );
        assert!(resolve_api_url_from(Some("http://127.0.0.1:3000"), None, &servers).is_ok());
        assert!(resolve_api_url_from(Some("http://127.42.7.9:3000"), None, &servers).is_ok());
        assert!(resolve_api_url_from(Some("http://[::1]:3000"), None, &servers).is_ok());
    }

    #[test]
    fn rejects_embedded_credentials_queries_and_fragments_for_the_api_base() {
        for url in [
            "https://user:secret@api.peppy.bot",
            "https://api.peppy.bot?token=secret",
            "https://api.peppy.bot/#fragment",
        ] {
            assert!(
                resolve_api_url_from(Some(url), None, &servers()).is_err(),
                "{url}"
            );
        }
    }

    #[test]
    fn normalizes_platform_origins() {
        assert_eq!(
            normalize_api_origin("HTTPS://API.PEPPY.BOT:443/v1").unwrap(),
            "https://api.peppy.bot"
        );
        assert_eq!(
            normalize_api_origin("http://LOCALHOST:3000/api").unwrap(),
            "http://localhost:3000"
        );
    }
}
