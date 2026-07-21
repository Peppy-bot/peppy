//! Backend URL resolution for the CLI auth commands.
//!
//! The backend the commands talk to is fixed by the build: the local dev
//! backend in debug builds, the prod backend in release. Its URL is stored in
//! the `resource_servers` block of `peppy_config.json5` (seeded with the build
//! default) and can be overridden at runtime via `--api-url` / `PEPPY_API_URL`.

use std::sync::Once;

use crate::error::{Error, Result};
use daemon_config::peppy_config::ResourceServers;
use url::{Host, Url};

/// Transport policy for server-supplied control-plane URLs, fixed by the build
/// profile via [`build_transport_policy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportPolicy {
    /// https required for any non-local host. Release builds always use this.
    Strict,
    /// Plain http admitted for non-local hosts too, with a one-time warning.
    /// Debug builds use this: they talk to the dev backend, whose launcher may
    /// hand out plain-http control-plane URLs on a trusted network (LAN mDNS
    /// name or Tailscale IP), so only dev credentials are at stake.
    AllowInsecureHttp,
}

/// The policy for this build: strict in release, permissive in debug. A
/// runtime check rather than `#[cfg]` so both arms stay compiled and the
/// policy-dependent behavior stays testable in either build profile.
pub fn build_transport_policy() -> TransportPolicy {
    if cfg!(debug_assertions) {
        TransportPolicy::AllowInsecureHttp
    } else {
        TransportPolicy::Strict
    }
}

/// Resolves the backend base URL in precedence order: the `--api-url` flag,
/// then `PEPPY_API_URL`, then the build's URL from the `resource_servers`
/// block. Reads process env directly; pass the flag through from clap.
pub fn resolve_api_url(api_url_flag: Option<&str>, servers: &ResourceServers) -> Result<String> {
    resolve_api_url_from(
        api_url_flag,
        env_nonempty("PEPPY_API_URL"),
        build_transport_policy(),
        servers,
    )
}

/// Pure core of [`resolve_api_url`] with the env inputs made explicit, so
/// precedence and the transport guard can be unit-tested without mutating
/// process env.
pub fn resolve_api_url_from(
    api_url_flag: Option<&str>,
    env_api_url: Option<String>,
    policy: TransportPolicy,
    servers: &ResourceServers,
) -> Result<String> {
    let api_url = api_url_flag
        .map(str::to_string)
        .or(env_api_url)
        .unwrap_or_else(|| servers.api.clone());

    let api_url = api_url.trim_end_matches('/').to_string();
    let parsed = validate_https_or_local_with(&api_url, "platform API", policy)?;
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

/// Validates one server-supplied control-plane URL under this build's policy.
/// Plain `http` is allowed only for an actual loopback address, `localhost`,
/// or `*.localhost`; anything else must be `https` so tokens never travel in
/// cleartext. Debug builds relax the non-local restriction (with a one-time
/// warning) because they target the dev backend; see [`build_transport_policy`].
///
/// `what` names the subject ("platform API", "OIDC issuer", "OIDC token
/// endpoint") so a rejection is attributable to the URL the caller was about to
/// use. Returning the parsed [`Url`] lets a caller compare schemes and origins
/// without parsing a security-sensitive value a second, weaker way.
pub fn validate_https_or_local(raw: &str, what: &str) -> Result<Url> {
    validate_https_or_local_with(raw, what, build_transport_policy())
}

/// Pure core of [`validate_https_or_local`] with the transport policy made
/// explicit. The policy only widens the plain-http arm for non-local hosts;
/// every other check stays strict regardless of policy.
pub fn validate_https_or_local_with(raw: &str, what: &str, policy: TransportPolicy) -> Result<Url> {
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
        "http" if policy == TransportPolicy::AllowInsecureHttp => {
            warn_insecure_transport_once(what, &parsed);
            Ok(parsed)
        }
        "http" => Err(Error::Auth(format!(
            "refusing plain http for non-local {what} `{raw}` (use https; plain http to a \
             non-local host is allowed only in debug builds)"
        ))),
        other => Err(Error::Auth(format!(
            "unsupported URL scheme `{other}` in `{raw}`"
        ))),
    }
}

/// One warning per process, at the moment an insecure URL is actually
/// admitted. The URL is safe to print: credential-bearing URLs were already
/// rejected before the scheme match.
fn warn_insecure_transport_once(what: &str, url: &Url) {
    static WARNED: Once = Once::new();
    WARNED.call_once(|| {
        println!(
            "Warning: allowing plain http for {what} {url} (debug build; a release build \
             requires https)"
        );
    });
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
            TransportPolicy::Strict,
            &servers,
        )
        .expect("resolve");
        assert_eq!(url, "http://localhost:9999");

        // PEPPY_API_URL beats the block default.
        let url = resolve_api_url_from(
            None,
            Some("http://127.0.0.1:1".into()),
            TransportPolicy::Strict,
            &servers,
        )
        .expect("resolve");
        assert_eq!(url, "http://127.0.0.1:1");

        // The block's api is the fallback.
        let url =
            resolve_api_url_from(None, None, TransportPolicy::Strict, &servers).expect("resolve");
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
        let url =
            resolve_api_url_from(None, None, TransportPolicy::Strict, &servers).expect("resolve");
        assert_eq!(url, "http://localhost:9000");
    }

    #[test]
    fn trailing_slash_trimmed() {
        let url = resolve_api_url_from(
            Some("http://localhost:3000/"),
            None,
            TransportPolicy::Strict,
            &servers(),
        )
        .expect("resolve");
        assert_eq!(url, "http://localhost:3000");
    }

    #[test]
    fn rejects_plain_http_for_remote_host() {
        let err = resolve_api_url_from(
            Some("http://api.peppy.bot"),
            None,
            TransportPolicy::Strict,
            &servers(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("plain http"));
    }

    #[test]
    fn allows_https_and_local_http() {
        let servers = servers();
        assert!(
            resolve_api_url_from(
                Some("https://api.peppy.bot"),
                None,
                TransportPolicy::Strict,
                &servers
            )
            .is_ok()
        );
        assert!(
            resolve_api_url_from(
                Some("http://auth.peppy.localhost:8080"),
                None,
                TransportPolicy::Strict,
                &servers
            )
            .is_ok()
        );
        assert!(
            resolve_api_url_from(
                Some("http://127.0.0.1:3000"),
                None,
                TransportPolicy::Strict,
                &servers
            )
            .is_ok()
        );
        // The old prefix test accepted these two for the wrong reason: it
        // matched the literal `127.` and the literal `::1` rather than asking
        // whether the parsed address is a loopback address.
        assert!(
            resolve_api_url_from(
                Some("http://127.42.7.9:3000"),
                None,
                TransportPolicy::Strict,
                &servers
            )
            .is_ok()
        );
        assert!(
            resolve_api_url_from(
                Some("http://[::1]:3000"),
                None,
                TransportPolicy::Strict,
                &servers
            )
            .is_ok()
        );
    }

    /// The whole point of matching on [`Host`]: `127.example` is a domain name
    /// that merely starts like a loopback literal, and the old
    /// `h.starts_with("127.")` check classified it as local and permitted an
    /// entire device flow in the clear against a remote host.
    #[test]
    fn domain_names_that_merely_start_like_loopback_are_not_local() {
        let servers = servers();
        for host in ["http://127.example", "http://127.evil.test"] {
            let err = resolve_api_url_from(Some(host), None, TransportPolicy::Strict, &servers)
                .unwrap_err();
            assert!(
                err.to_string().contains("plain http"),
                "expected {host} to be refused as non-local, got: {err}"
            );
        }
    }

    #[test]
    fn rejects_embedded_credentials_queries_and_fragments_for_the_api_base() {
        let servers = servers();
        let err = resolve_api_url_from(
            Some("https://alice:hunter2@api.peppy.bot"),
            None,
            TransportPolicy::Strict,
            &servers,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("embedded credentials"),
            "got: {err}"
        );

        let err = resolve_api_url_from(
            Some("https://api.peppy.bot?tenant=x"),
            None,
            TransportPolicy::Strict,
            &servers,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("query string or fragment"),
            "got: {err}"
        );

        let err = resolve_api_url_from(
            Some("https://api.peppy.bot#frag"),
            None,
            TransportPolicy::Strict,
            &servers,
        )
        .unwrap_err();
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

    #[test]
    fn transport_policy_follows_the_build_profile() {
        let expected = if cfg!(debug_assertions) {
            TransportPolicy::AllowInsecureHttp
        } else {
            TransportPolicy::Strict
        };
        assert_eq!(build_transport_policy(), expected);
    }

    /// An address from `100.64.0.0/10`, the shared address space Tailscale
    /// allocates from. It stands in for the issuer a TS-mode dev stack
    /// advertises, and it must be an IP literal rather than a name so this
    /// exercises the `Host::Ipv4` non-loopback branch of [`is_local`].
    const TAILSCALE_STYLE_ISSUER: &str = "http://100.64.0.7:8080";

    #[test]
    fn remote_plain_http_is_policy_gated() {
        let err = validate_https_or_local_with(
            TAILSCALE_STYLE_ISSUER,
            "OIDC issuer",
            TransportPolicy::Strict,
        )
        .unwrap_err();
        assert!(err.to_string().contains("plain http"), "got: {err}");

        let url = validate_https_or_local_with(
            TAILSCALE_STYLE_ISSUER,
            "OIDC issuer",
            TransportPolicy::AllowInsecureHttp,
        )
        .expect("admitted under the permissive policy");
        assert_eq!(url.as_str(), "http://100.64.0.7:8080/");
    }

    /// The permissive policy only widens the plain-http arm for non-local
    /// hosts; every other rejection stays in force.
    #[test]
    fn permissive_policy_does_not_relax_other_url_checks() {
        let policy = TransportPolicy::AllowInsecureHttp;

        let err =
            validate_https_or_local_with("http://alice:hunter2@host.test", "OIDC issuer", policy)
                .unwrap_err();
        assert!(
            err.to_string().contains("embedded credentials"),
            "got: {err}"
        );

        let err =
            validate_https_or_local_with("ftp://host.test", "OIDC issuer", policy).unwrap_err();
        assert!(
            err.to_string().contains("unsupported URL scheme"),
            "got: {err}"
        );

        let err =
            validate_https_or_local_with(" http://host.test", "OIDC issuer", policy).unwrap_err();
        assert!(
            err.to_string().contains("surrounding whitespace"),
            "got: {err}"
        );
    }

    #[test]
    fn resolve_api_url_admits_remote_http_only_under_the_permissive_policy() {
        let servers = servers();
        // Same shared-address-space rationale as TAILSCALE_STYLE_ISSUER, on
        // the port the dev backend binds.
        let flag = Some("http://100.64.0.7:3000");

        let err = resolve_api_url_from(flag, None, TransportPolicy::Strict, &servers).unwrap_err();
        assert!(err.to_string().contains("plain http"), "got: {err}");

        let url = resolve_api_url_from(flag, None, TransportPolicy::AllowInsecureHttp, &servers)
            .expect("admitted under the permissive policy");
        assert_eq!(url, "http://100.64.0.7:3000");
    }
}
