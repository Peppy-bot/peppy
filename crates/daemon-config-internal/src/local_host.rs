//! Which hosts are the machine itself.

use url::{Host, Url};

/// Whether the host of `url` is the machine itself, where cleartext traffic
/// stays off the network. A domain is local when it is `localhost` or under
/// it, and an address when it is a loopback one: `127.example.com` is a
/// domain.
pub fn is_local(url: &Url) -> bool {
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

    #[test]
    fn loopback_addresses_and_localhost_names_are_local() {
        for local in [
            "http://localhost:4318",
            "http://collector.localhost/",
            "http://127.0.0.1:4318",
            "http://127.8.9.1/",
            "http://[::1]:4318",
        ] {
            assert!(is_local(&Url::parse(local).unwrap()), "{local}");
        }
        for remote in [
            "http://127.example.com/",
            "http://localhost.example.com/",
            "http://192.168.1.20:4318",
            "http://collector.lan:4318",
        ] {
            assert!(!is_local(&Url::parse(remote).unwrap()), "{remote}");
        }
    }
}
