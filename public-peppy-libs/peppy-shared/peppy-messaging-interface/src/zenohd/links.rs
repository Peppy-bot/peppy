//! Bounded wait for a managed router's configured `connect` links to
//! establish.
//!
//! A managed zenohd accepts local sessions as soon as its listener binds, but
//! dials its configured `connect` endpoints (operator-pinned federation links,
//! a just-applied cloud upstream) asynchronously. Anything that queries the
//! mesh right after startup — the daemon's boot-time core-node presence check
//! above all — would race that link formation and see only the local router.
//! [`RouterLinksProbe`] closes the race: it polls the local router's admin
//! space (`@/<zid>/router`) until every configured endpoint has an established
//! transport link, bounded by the caller's timeout and fail-open (an
//! unreachable peer degrades to a standalone-looking start, it never wedges
//! startup).

use std::net::IpAddr;
use std::str::FromStr;
use std::time::Duration;

use tokio::time::Instant;

use super::ZenohEndpoint;

/// How often the probe re-reads the admin space while links are still
/// pending. Establishment is normally a few tens of milliseconds after
/// zenohd spawns, so a short cadence keeps the added startup latency small.
const LINKS_POLL_INTERVAL: Duration = Duration::from_millis(150);

/// Bound on a single admin-space query. The probe session talks only to the
/// local router, which answers from memory, so this only protects the poll
/// loop from a stalled transport.
const ADMIN_QUERY_TIMEOUT: Duration = Duration::from_secs(1);

/// One configured `connect` endpoint, pre-parsed for matching against the
/// link destinations the router's admin space reports.
///
/// A link destination is reported with a *resolved* address (`tls/1.2.3.4:7443`
/// for a dialed `tls/router.example:7443`), so a DNS-named endpoint carries its
/// resolved IPs alongside the literal host for the comparison.
#[derive(Debug, Clone)]
struct ConfiguredEndpoint {
    /// `<proto>/<host>:<port>` with any `?config` / `#metadata` suffix removed.
    raw: String,
    protocol: String,
    /// Lowercased, without IPv6 brackets.
    host: String,
    port: u16,
    /// Addresses `host` resolves to (or parses to, for a literal IP). Empty
    /// until [`RouterLinksProbe::wait_established`] resolves it, and left empty
    /// when resolution fails — the literal comparison still applies then.
    ips: Vec<IpAddr>,
}

impl ConfiguredEndpoint {
    /// Parses a zenoh endpoint string (`<proto>/<host>:<port>[?config][#meta]`).
    /// Returns `None` for a form [`ZenohEndpoint`] cannot represent; the caller
    /// warns and skips it rather than failing the whole probe.
    fn parse(endpoint: &str) -> Option<Self> {
        // An `EndPoint` may carry a `?<config>` and/or `#<metadata>` suffix;
        // the locator part before them is what a link destination reports.
        let locator = endpoint.split(['?', '#']).next().unwrap_or(endpoint).trim();
        let parsed = ZenohEndpoint::from_str(locator).ok()?;
        Some(Self {
            raw: locator.to_string(),
            protocol: parsed.protocol().to_string(),
            host: unbracket(parsed.host()).to_ascii_lowercase(),
            port: parsed.port(),
            ips: Vec::new(),
        })
    }

    /// Whether `dst` (a link destination locator from the admin space) is this
    /// endpoint: same protocol and port, and a host that matches literally or
    /// through one of the resolved addresses.
    fn matches_dst(&self, dst: &str) -> bool {
        let locator = dst.split(['?', '#']).next().unwrap_or(dst).trim();
        let Ok(parsed) = ZenohEndpoint::from_str(locator) else {
            return false;
        };
        if parsed.protocol().to_string() != self.protocol || parsed.port() != self.port {
            return false;
        }
        let dst_host = unbracket(parsed.host()).to_ascii_lowercase();
        if dst_host == self.host {
            return true;
        }
        dst_host
            .parse::<IpAddr>()
            .is_ok_and(|ip| self.ips.contains(&ip))
    }
}

fn unbracket(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
}

/// Extracts every transport link destination from a router admin-space reply
/// (`@/<zid>/router`): `sessions[].links[].dst`. Outgoing links report the
/// dialed endpoint there, which is what configured `connect` endpoints match.
fn link_dsts(admin_reply: &serde_json::Value) -> Vec<String> {
    admin_reply["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|session| session["links"].as_array().into_iter().flatten())
        .filter_map(|link| link["dst"].as_str())
        .map(str::to_string)
        .collect()
}

/// Whether every configured endpoint has a matching established link.
fn all_established(endpoints: &[ConfiguredEndpoint], dsts: &[String]) -> bool {
    endpoints
        .iter()
        .all(|endpoint| dsts.iter().any(|dst| endpoint.matches_dst(dst)))
}

/// Lock-free probe that waits for the managed router's configured `connect`
/// links to establish. Built by
/// [`crate::Messenger::router_links_probe`] (via the Zenoh adapter) from the
/// same fail-fast probe config as [`super::RouterHealthChecker`], plus the
/// active router config's `connect.endpoints`; `None` there means there is
/// nothing to wait for.
pub struct RouterLinksProbe {
    probe_config: zenoh::config::Config,
    endpoints: Vec<ConfiguredEndpoint>,
}

impl RouterLinksProbe {
    /// Builds a probe for `endpoints`, or `None` when none of them parses (an
    /// unparseable endpoint is the operator's to dial, not ours to wait for —
    /// it is warned about and skipped).
    pub(crate) fn new(probe_config: zenoh::config::Config, endpoints: Vec<String>) -> Option<Self> {
        let endpoints: Vec<ConfiguredEndpoint> = endpoints
            .iter()
            .filter_map(|endpoint| {
                let parsed = ConfiguredEndpoint::parse(endpoint);
                if parsed.is_none() {
                    tracing::warn!(
                        endpoint,
                        "unparseable router connect endpoint; not waiting for its link"
                    );
                }
                parsed
            })
            .collect();
        if endpoints.is_empty() {
            return None;
        }
        Some(Self {
            probe_config,
            endpoints,
        })
    }

    /// The configured endpoints this probe waits for, for logging.
    pub fn endpoints(&self) -> Vec<String> {
        self.endpoints
            .iter()
            .map(|endpoint| endpoint.raw.clone())
            .collect()
    }

    /// Waits until every configured endpoint has an established link on the
    /// local router, bounded by `timeout`. Returns `true` when they all did,
    /// `false` when the bound elapsed first (callers proceed — fail-open — and
    /// log). DNS-named endpoints are resolved once up front so they compare
    /// against the resolved addresses the admin space reports.
    pub async fn wait_established(mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;

        // Resolve DNS-named endpoints. A failed or slow resolution leaves that
        // endpoint literal-only (it can then only match a same-named dst),
        // never wedges the wait past the deadline.
        for endpoint in &mut self.endpoints {
            if let Ok(ip) = endpoint.host.parse::<IpAddr>() {
                endpoint.ips = vec![ip];
                continue;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let lookup = tokio::net::lookup_host((endpoint.host.as_str(), endpoint.port));
            if let Ok(Ok(addrs)) = tokio::time::timeout(remaining, lookup).await {
                endpoint.ips = addrs.map(|addr| addr.ip()).collect();
            }
        }

        // One probe session for the whole wait; per-attempt admin queries.
        let remaining = deadline.saturating_duration_since(Instant::now());
        let session =
            match tokio::time::timeout(remaining, zenoh::open(self.probe_config.clone())).await {
                Ok(Ok(session)) => session,
                Ok(Err(_)) | Err(_) => return false,
            };

        let established = loop {
            if let Some(dsts) = query_link_dsts(&session).await
                && all_established(&self.endpoints, &dsts)
            {
                break true;
            }
            if Instant::now() + LINKS_POLL_INTERVAL > deadline {
                break false;
            }
            tokio::time::sleep(LINKS_POLL_INTERVAL).await;
        };

        // The probe served its purpose either way; don't let a slow close
        // stall the caller (same policy as the router health checker).
        let _ = tokio::time::timeout(Duration::from_secs(1), session.close()).await;
        established
    }
}

/// One admin-space read: queries `@/<zid>/router` on the (single) router this
/// probe session is connected to and returns the link destinations it reports.
/// `None` when the router did not answer within the attempt bound.
async fn query_link_dsts(session: &zenoh::Session) -> Option<Vec<String>> {
    let zid = session.info().routers_zid().await.next()?;
    let keyexpr = format!("@/{zid}/router");
    // Callback handler per this crate's convention (see `adapters::zenoh` module
    // docs): the sender is dropped when the query finalizes, ending the drain.
    let (tx, rx) = flume::bounded::<serde_json::Value>(1);
    session
        .get(&keyexpr)
        .timeout(ADMIN_QUERY_TIMEOUT)
        .callback(move |reply| {
            if let Ok(sample) = reply.into_result()
                && let Ok(text) = sample.payload().try_to_string()
                && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
            {
                let _ = tx.try_send(value);
            }
        })
        .await
        .ok()?;
    rx.recv_async().await.ok().map(|value| link_dsts(&value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(raw: &str) -> ConfiguredEndpoint {
        ConfiguredEndpoint::parse(raw).expect("test endpoint parses")
    }

    #[test]
    fn parse_strips_config_and_metadata_suffixes() {
        let parsed = endpoint("tls/router.example:7443#iface=eth0");
        assert_eq!(parsed.raw, "tls/router.example:7443");
        assert_eq!(parsed.protocol, "tls");
        assert_eq!(parsed.host, "router.example");
        assert_eq!(parsed.port, 7443);

        let with_config = endpoint("tcp/10.0.0.7:7447?rx_buffer_size=65536");
        assert_eq!(with_config.raw, "tcp/10.0.0.7:7447");
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(ConfiguredEndpoint::parse("not-an-endpoint").is_none());
        assert!(ConfiguredEndpoint::parse("").is_none());
    }

    #[test]
    fn literal_ip_endpoint_matches_its_dst_exactly() {
        let parsed = endpoint("tcp/172.17.0.2:7447");
        assert!(parsed.matches_dst("tcp/172.17.0.2:7447"));
        // Port, protocol, and host must all agree.
        assert!(!parsed.matches_dst("tcp/172.17.0.2:7448"));
        assert!(!parsed.matches_dst("tls/172.17.0.2:7447"));
        assert!(!parsed.matches_dst("tcp/172.17.0.3:7447"));
        // An inbound link's dst is the peer's ephemeral port; never a match.
        assert!(!parsed.matches_dst("tcp/172.17.0.9:40214"));
    }

    #[test]
    fn dns_endpoint_matches_through_resolved_ips() {
        let mut parsed = endpoint("tls/router.example:7443");
        // Unresolved: only the literal name would match.
        assert!(parsed.matches_dst("tls/router.example:7443"));
        assert!(!parsed.matches_dst("tls/198.51.100.7:7443"));

        parsed.ips = vec!["198.51.100.7".parse().expect("test ip")];
        assert!(parsed.matches_dst("tls/198.51.100.7:7443"));
        assert!(!parsed.matches_dst("tls/198.51.100.8:7443"));
    }

    #[test]
    fn ipv6_hosts_compare_without_brackets() {
        let parsed = endpoint("tcp/[::1]:7447");
        assert!(parsed.matches_dst("tcp/[::1]:7447"));
        assert_eq!(parsed.host, "::1");
    }

    #[test]
    fn link_dsts_come_from_every_session() {
        let admin_reply = serde_json::json!({
            "zid": "abc",
            "sessions": [
                {
                    "peer": "r1",
                    "whatami": "router",
                    "links": [
                        {"src": "tcp/172.17.0.4:52034", "dst": "tcp/172.17.0.2:7447"}
                    ]
                },
                {
                    "peer": "p1",
                    "whatami": "peer",
                    "links": [
                        {"src": "tcp/127.0.0.1:7447", "dst": "tcp/127.0.0.1:41822"}
                    ]
                }
            ]
        });
        assert_eq!(
            link_dsts(&admin_reply),
            vec![
                "tcp/172.17.0.2:7447".to_string(),
                "tcp/127.0.0.1:41822".to_string()
            ]
        );
        // A reply without sessions (or of an unexpected shape) yields no dsts.
        assert!(link_dsts(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn all_established_requires_every_endpoint() {
        let endpoints = vec![
            endpoint("tcp/172.17.0.2:7447"),
            endpoint("tcp/172.17.0.3:7447"),
        ];
        let one_up = vec!["tcp/172.17.0.2:7447".to_string()];
        let both_up = vec![
            "tcp/172.17.0.2:7447".to_string(),
            "tcp/172.17.0.3:7447".to_string(),
            // Unrelated inbound link noise must not confuse the check.
            "tcp/172.17.0.9:40214".to_string(),
        ];
        assert!(!all_established(&endpoints, &one_up));
        assert!(all_established(&endpoints, &both_up));
        assert!(all_established(&[], &[]));
    }

    #[test]
    fn probe_construction_skips_unparseable_and_collapses_to_none() {
        let config = zenoh::config::Config::default();
        let probe = RouterLinksProbe::new(
            config.clone(),
            vec!["tcp/172.17.0.2:7447".to_string(), "garbage".to_string()],
        )
        .expect("one parseable endpoint keeps the probe");
        assert_eq!(probe.endpoints(), vec!["tcp/172.17.0.2:7447".to_string()]);

        assert!(
            RouterLinksProbe::new(config.clone(), vec!["garbage".to_string()]).is_none(),
            "nothing parseable ⇒ nothing to wait for"
        );
        assert!(RouterLinksProbe::new(config, Vec::new()).is_none());
    }
}
