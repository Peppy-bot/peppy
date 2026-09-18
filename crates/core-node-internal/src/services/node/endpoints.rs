//! The endpoints a started instance serves: the sockets it announced during
//! setup, expanded by the daemon hosting it into the URLs an operator opens.
//!
//! A node announces what it bound, never where it is reachable: a socket on
//! an unspecified address (`0.0.0.0`, `::`) answers on every interface of
//! the machine, so the daemon that spawned the instance, the one machine
//! that can enumerate those interfaces, turns each binding into one URL per
//! host address. Apptainer instances share the host network namespace, so a
//! socket bound inside a container is reachable at the host's addresses too.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv6Addr};

use config::node::EndpointDeclarations;
use core_node_api::InstanceEndpoint;
use nix::net::if_::InterfaceFlags;
use peppylib::runtime::{AnnouncedEndpoint, EndpointBinding};

/// The host addresses an instance's endpoints expand against: `None` reads
/// this machine's interfaces, `Some` is the fixed list a test injects so an
/// expansion is deterministic.
pub type HostAddressSource = Option<Vec<IpAddr>>;

/// The addresses `source` expands against, reading the system's interfaces
/// when it names none.
pub fn host_addresses(source: &HostAddressSource) -> Vec<IpAddr> {
    source.clone().unwrap_or_else(read_host_addresses)
}

/// Reads the addresses of this machine's interfaces that are up, skipping
/// IPv6 link-local addresses, loopback first, deduplicated. The one function
/// here that touches the system.
pub fn read_host_addresses() -> Vec<IpAddr> {
    let interfaces = match nix::ifaddrs::getifaddrs() {
        Ok(interfaces) => interfaces,
        Err(error) => {
            tracing::warn!("cannot enumerate the host's interfaces: {error}");
            return Vec::new();
        }
    };
    let mut addresses = Vec::new();
    for interface in interfaces {
        let Some(address) = interface.address else {
            continue;
        };
        let ip = if let Some(v4) = address.as_sockaddr_in() {
            IpAddr::V4(v4.ip())
        } else if let Some(v6) = address.as_sockaddr_in6() {
            IpAddr::V6(v6.ip())
        } else {
            continue;
        };
        if !keep_interface(interface.flags, ip) {
            continue;
        }
        addresses.push(ip);
    }
    order_host_addresses(addresses)
}

/// The fixed addresses a test expands against, so an expected URL is the same
/// whatever machine runs the test: loopback, a LAN address and a tailscale
/// one. Lives beside the reader it stands in for, since the URLs asserted in
/// this crate's and `peppy`'s tests are written against exactly this list.
pub fn test_host_addresses() -> Vec<IpAddr> {
    ["127.0.0.1", "192.168.1.5", "100.123.58.116"]
        .iter()
        .map(|ip| ip.parse().expect("an IP literal"))
        .collect()
}

/// The reader's filter: an interface that is up, and an address an operator
/// can type into a browser, which an IPv6 link-local address (scoped to one
/// link, unusable without a zone id) is not.
fn keep_interface(flags: InterfaceFlags, ip: IpAddr) -> bool {
    if !flags.contains(InterfaceFlags::IFF_UP) {
        return false;
    }
    match ip {
        IpAddr::V4(_) => true,
        IpAddr::V6(v6) => !is_link_local(v6),
    }
}

/// `fe80::/10`.
fn is_link_local(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

/// Loopback addresses first, then the rest in the order they were read,
/// each address once.
fn order_host_addresses(addresses: Vec<IpAddr>) -> Vec<IpAddr> {
    let mut seen = BTreeSet::new();
    let (loopback, others): (Vec<_>, Vec<_>) = addresses
        .into_iter()
        .filter(|ip| seen.insert(*ip))
        .partition(IpAddr::is_loopback);
    loopback.into_iter().chain(others).collect()
}

/// The URLs an operator opens to reach `binding` on a machine with `host`'s
/// addresses. An unspecified IPv4 address expands to every IPv4 address of
/// the host, an unspecified IPv6 address to every address; any other
/// address yields the one URL for that literal.
pub fn expand_binding(binding: &EndpointBinding, host: &[IpAddr]) -> Vec<String> {
    let bound = binding.address.ip();
    let reachable: Vec<IpAddr> = match bound {
        IpAddr::V4(v4) if v4.is_unspecified() => {
            host.iter().copied().filter(IpAddr::is_ipv4).collect()
        }
        IpAddr::V6(v6) if v6.is_unspecified() => host.to_vec(),
        literal => vec![literal],
    };
    reachable
        .into_iter()
        .map(|ip| render_url(&binding.scheme, ip, binding.address.port(), &binding.path))
        .collect()
}

fn render_url(scheme: &str, ip: IpAddr, port: u16, path: &str) -> String {
    match ip {
        IpAddr::V4(v4) => format!("{scheme}://{v4}:{port}{path}"),
        IpAddr::V6(v6) => format!("{scheme}://[{v6}]:{port}{path}"),
    }
}

/// Turns the set an instance announced into the endpoints the daemon
/// reports: every announced label must be declared and every declared label
/// announced, and each binding is expanded against `host`. The result is in
/// label order, each endpoint carrying the kind its declaration gives it.
pub fn expand_announcements(
    instance_id: &str,
    declared: &EndpointDeclarations,
    announced: Vec<AnnouncedEndpoint>,
    host: &[IpAddr],
) -> std::result::Result<Vec<InstanceEndpoint>, String> {
    // Compared as sets: a node is free to announce in whatever order its
    // setup binds, and the result is sorted by label below.
    let announced_labels: BTreeSet<&str> = announced
        .iter()
        .map(|endpoint| endpoint.label.as_str())
        .collect();
    let declared_labels: BTreeSet<&str> = declared.keys().map(|label| label.as_str()).collect();
    if announced_labels != declared_labels {
        let joined =
            |labels: &BTreeSet<&str>| labels.iter().copied().collect::<Vec<_>>().join(", ");
        return Err(format!(
            "instance '{instance_id}' announced endpoints [{}] but its manifest declares [{}]",
            joined(&announced_labels),
            joined(&declared_labels),
        ));
    }
    let mut endpoints: Vec<InstanceEndpoint> = announced
        .into_iter()
        .map(|endpoint| InstanceEndpoint {
            kind: declared[endpoint.label.as_str()].kind,
            urls: expand_binding(&endpoint.binding, host),
            label: endpoint.label,
        })
        .collect();
    endpoints.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(endpoints)
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::node::EndpointKind;

    /// The addresses the expansion tests inject: loopback first, then two
    /// interfaces, IPv6 included so the bracketing and the v4-only filter are
    /// both covered.
    fn test_host() -> Vec<IpAddr> {
        [
            "127.0.0.1",
            "::1",
            "192.168.1.5",
            "2001:db8::7",
            "100.123.58.116",
        ]
        .iter()
        .map(|ip| ip.parse().expect("an IP literal"))
        .collect()
    }

    fn binding(scheme: &str, address: &str, path: &str) -> EndpointBinding {
        EndpointBinding {
            scheme: scheme.to_string(),
            address: address.parse().expect("a socket address"),
            path: path.to_string(),
        }
    }

    #[test]
    fn an_unspecified_ipv4_binding_expands_to_every_ipv4_address_loopback_first() {
        assert_eq!(
            expand_binding(&binding("http", "0.0.0.0:8765", ""), &test_host()),
            [
                "http://127.0.0.1:8765",
                "http://192.168.1.5:8765",
                "http://100.123.58.116:8765",
            ]
        );
    }

    #[test]
    fn an_unspecified_ipv6_binding_expands_to_every_address_with_ipv6_in_brackets() {
        assert_eq!(
            expand_binding(&binding("https", "[::]:8080", "/"), &test_host()),
            [
                "https://127.0.0.1:8080/",
                "https://[::1]:8080/",
                "https://192.168.1.5:8080/",
                "https://[2001:db8::7]:8080/",
                "https://100.123.58.116:8080/",
            ]
        );
    }

    #[test]
    fn a_literal_binding_yields_one_url() {
        assert_eq!(
            expand_binding(
                &binding("http", "127.0.0.1:8900", "/camera/v1/mcp"),
                &test_host()
            ),
            ["http://127.0.0.1:8900/camera/v1/mcp"]
        );
        assert_eq!(
            expand_binding(
                &binding("https", "[2001:db8::7]:4443", "/task"),
                &test_host()
            ),
            ["https://[2001:db8::7]:4443/task"]
        );
    }

    #[test]
    fn a_host_with_no_addresses_expands_an_unspecified_binding_to_nothing() {
        assert!(expand_binding(&binding("http", "0.0.0.0:8765", ""), &[]).is_empty());
    }

    #[test]
    fn the_reader_filter_skips_down_interfaces_and_ipv6_link_local() {
        let up = InterfaceFlags::IFF_UP | InterfaceFlags::IFF_RUNNING;
        let down = InterfaceFlags::IFF_RUNNING;
        assert!(keep_interface(up, "192.168.1.5".parse().unwrap()));
        assert!(keep_interface(up, "2001:db8::7".parse().unwrap()));
        assert!(!keep_interface(down, "192.168.1.5".parse().unwrap()));
        assert!(!keep_interface(up, "fe80::1".parse().unwrap()));
        assert!(!keep_interface(up, "febf::1".parse().unwrap()));
        assert!(keep_interface(up, "fec0::1".parse().unwrap()));
    }

    #[test]
    fn host_addresses_come_loopback_first_and_deduplicated() {
        let ip = |literal: &str| literal.parse::<IpAddr>().expect("an IP literal");
        // `192.168.1.5` twice stands for one address read from two aliased
        // interfaces: it survives once, in the position it was first read.
        let ordered = order_host_addresses(vec![
            ip("192.168.1.5"),
            ip("127.0.0.1"),
            ip("192.168.1.5"),
            ip("::1"),
            ip("10.0.0.9"),
        ]);
        assert_eq!(
            ordered,
            [
                ip("127.0.0.1"),
                ip("::1"),
                ip("192.168.1.5"),
                ip("10.0.0.9")
            ]
        );
        assert!(ordered[0].is_loopback() && !ordered[2].is_loopback());
    }

    #[test]
    fn the_system_reader_answers_loopback_first() {
        let addresses = read_host_addresses();
        assert!(
            addresses.first().is_some_and(IpAddr::is_loopback),
            "every host has an up loopback interface: {addresses:?}"
        );
        assert!(
            addresses.iter().all(|ip| match ip {
                IpAddr::V6(v6) => !is_link_local(*v6),
                IpAddr::V4(_) => true,
            }),
            "link-local addresses are filtered: {addresses:?}"
        );
    }

    fn declarations(json5: &str) -> EndpointDeclarations {
        let config: config::node::NodeConfig = serde_json5::from_str(&format!(
            r#"{{
                peppy_schema: "node/v1",
                manifest: {{ name: "panel_node", tag: "v1" }},
                execution: {{ language: "rust", run_cmd: ["./bin"], endpoints: {json5} }}
            }}"#
        ))
        .expect("a valid manifest");
        config.execution.endpoints
    }

    fn announced(label: &str, binding: EndpointBinding) -> AnnouncedEndpoint {
        AnnouncedEndpoint {
            label: label.to_string(),
            binding,
        }
    }

    #[test]
    fn announcements_become_endpoints_with_their_declared_kind_in_label_order() {
        let declared = declarations(
            r#"{
                viewer: { kind: "page", description: "The viewer." },
                camera_v1: { kind: "mcp", description: "The camera exposure." },
            }"#,
        );
        let endpoints = expand_announcements(
            "sim_inst",
            &declared,
            vec![
                announced("viewer", binding("https", "0.0.0.0:8080", "/")),
                announced(
                    "camera_v1",
                    binding("http", "127.0.0.1:8900", "/camera/v1/mcp"),
                ),
            ],
            &test_host(),
        )
        .expect("the sets agree");
        assert_eq!(
            endpoints,
            vec![
                InstanceEndpoint {
                    label: "camera_v1".to_string(),
                    kind: EndpointKind::Mcp,
                    urls: vec!["http://127.0.0.1:8900/camera/v1/mcp".to_string()],
                },
                InstanceEndpoint {
                    label: "viewer".to_string(),
                    kind: EndpointKind::Page,
                    urls: vec![
                        "https://127.0.0.1:8080/".to_string(),
                        "https://192.168.1.5:8080/".to_string(),
                        "https://100.123.58.116:8080/".to_string(),
                    ],
                },
            ]
        );
    }

    #[test]
    fn a_label_set_that_differs_from_the_declaration_is_refused_naming_both_sets() {
        let declared = declarations(r#"{ panel: { kind: "page", description: "The panel." } }"#);
        let error = expand_announcements(
            "panel_inst",
            &declared,
            vec![announced("admin", binding("http", "0.0.0.0:9000", ""))],
            &test_host(),
        )
        .expect_err("the sets differ");
        assert_eq!(
            error,
            "instance 'panel_inst' announced endpoints [admin] but its manifest declares [panel]"
        );
        let error = expand_announcements("panel_inst", &declared, Vec::new(), &test_host())
            .expect_err("nothing announced");
        assert_eq!(
            error,
            "instance 'panel_inst' announced endpoints [] but its manifest declares [panel]"
        );
    }
}
