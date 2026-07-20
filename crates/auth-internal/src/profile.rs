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
        return Err(Error::Auth(format!(
            "invalid platform API `{api_url}`: a query string or fragment is not allowed"
        )));
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

/// Validates one server-supplied control-plane URL. Plain `http` is allowed
/// only for an actual loopback address, `localhost`, or `*.localhost`; anything
/// else must be `https` so tokens never travel in cleartext.
///
/// `what` names the subject ("platform API", "OIDC issuer", "OIDC token
/// endpoint") so a rejection is attributable to the URL the caller was about to
/// use. Returning the parsed [`Url`] lets a caller compare schemes and origins
/// without parsing a security-sensitive value a second, weaker way.
pub fn validate_https_or_local(raw: &str, what: &str) -> Result<Url> {
    if raw.is_empty() || raw.trim() != raw {
        return Err(Error::Auth(format!(
            "invalid {what}: it must be non-empty and carry no surrounding whitespace"
        )));
    }
    let parsed =
        Url::parse(raw).map_err(|e| Error::Auth(format!("invalid {what} `{raw}`: {e}")))?;
    if parsed.cannot_be_a_base() || parsed.host().is_none() {
        return Err(Error::Auth(format!(
            "invalid {what} `{raw}`: an absolute URL with a host is required"
        )));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(Error::Auth(format!(
            "invalid {what} `{raw}`: embedded credentials are not allowed"
        )));
    }
    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" if is_local(&parsed) => Ok(parsed),
        "http" => Err(Error::Auth(format!(
            "refusing plain http for non-local {what} `{raw}` (use https)"
        ))),
        other => Err(Error::Auth(format!(
            "unsupported URL scheme `{other}` in `{raw}`"
        ))),
    }
}

/// The canonical scheme, host, and port binding of the platform API. A path,
/// query, fragment, case difference, or explicit default port cannot spell one
/// platform origin two ways.
///
/// The input is validated here rather than trusted from the caller. Because
/// [`validate_https_or_local`] has already rejected a `cannot_be_a_base` URL and
/// a missing host, the origin is always a tuple origin and the serialization can
/// never be `"null"`.
pub fn normalize_api_origin(api_url: &str) -> Result<String> {
    Ok(validate_https_or_local(api_url, "platform API")?
        .origin()
        .ascii_serialization())
}

/// Whether a parsed host is local enough to permit cleartext development
/// traffic. Matching on [`Host`] avoids the string-prefix pitfall that let a
/// domain such as `127.example.com` pass for a loopback address.
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
    fn allows_https_and_local_http() {
        let servers = servers();
        assert!(resolve_api_url_from(Some("https://api.peppy.bot"), None, &servers).is_ok());
        assert!(
            resolve_api_url_from(Some("http://auth.peppy.localhost:8080"), None, &servers).is_ok()
        );
        assert!(resolve_api_url_from(Some("http://127.0.0.1:3000"), None, &servers).is_ok());
        // The old prefix test accepted these two for the wrong reason: it
        // matched the literal `127.` and the literal `::1` rather than asking
        // whether the parsed address is a loopback address.
        assert!(resolve_api_url_from(Some("http://127.42.7.9:3000"), None, &servers).is_ok());
        assert!(resolve_api_url_from(Some("http://[::1]:3000"), None, &servers).is_ok());
    }

    /// The whole point of matching on [`Host`]: `127.example` is a domain name
    /// that merely starts like a loopback literal, and the old
    /// `h.starts_with("127.")` check classified it as local and permitted an
    /// entire device flow in the clear against a remote host.
    #[test]
    fn domain_names_that_merely_start_like_loopback_are_not_local() {
        let servers = servers();
        for host in ["http://127.example", "http://127.evil.test"] {
            let err = resolve_api_url_from(Some(host), None, &servers).unwrap_err();
            assert!(
                err.to_string().contains("plain http"),
                "expected {host} to be refused as non-local, got: {err}"
            );
        }
    }

    #[test]
    fn rejects_embedded_credentials_queries_and_fragments_for_the_api_base() {
        let servers = servers();
        let err = resolve_api_url_from(Some("https://alice:hunter2@api.peppy.bot"), None, &servers)
            .unwrap_err();
        assert!(
            err.to_string().contains("embedded credentials"),
            "got: {err}"
        );

        let err = resolve_api_url_from(Some("https://api.peppy.bot?tenant=x"), None, &servers)
            .unwrap_err();
        assert!(
            err.to_string().contains("query string or fragment"),
            "got: {err}"
        );

        let err =
            resolve_api_url_from(Some("https://api.peppy.bot#frag"), None, &servers).unwrap_err();
        assert!(
            err.to_string().contains("query string or fragment"),
            "got: {err}"
        );
    }

    #[test]
    fn normalizes_platform_origins() {
        assert_eq!(
            normalize_api_origin("HTTPS://API.PEPPY.BOT:443/v1").expect("normalize"),
            "https://api.peppy.bot"
        );
        assert_eq!(
            normalize_api_origin("http://LOCALHOST:3000/api").expect("normalize"),
            "http://localhost:3000"
        );
    }
}
