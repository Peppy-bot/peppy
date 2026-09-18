//! The sockets a node announces during setup under the labels its manifest
//! declares in `execution.endpoints`.
//!
//! A node owns the address it binds; the manifest only says that a socket is
//! served under a label. During `setup_fn` the node announces each binding,
//! and when setup returns the runtime seals the set and checks it against
//! the declaration: every declared label was announced, nothing else was.
//! The sealed set is what the `node_endpoints` service answers with, and
//! what the daemon expands into the URLs it reports.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;

use config::node::EndpointLabel;

use crate::error::{Error, Result};

/// The socket a node bound for one declared endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointBinding {
    /// URI scheme token: `[a-z][a-z0-9+.-]*`, e.g. `http` or `https`.
    pub scheme: String,
    /// What the listener bound: the address as the operating system reports
    /// it back, so a port the system picked is the one announced.
    pub address: SocketAddr,
    /// `""` or a path starting with `/`.
    pub path: String,
}

impl EndpointBinding {
    /// Refuses a scheme that is not a URI scheme token and a path that does
    /// not start with `/`, naming what is wrong.
    fn validate(&self) -> std::result::Result<(), String> {
        if !is_scheme_token(&self.scheme) {
            return Err(format!(
                "scheme `{}` is not a URI scheme token (lowercase letters, digits, `+`, `.` and `-`, starting with a letter)",
                self.scheme
            ));
        }
        if !self.path.is_empty() && !self.path.starts_with('/') {
            return Err(format!(
                "path `{}` must be empty or start with `/`",
                self.path
            ));
        }
        Ok(())
    }
}

fn is_scheme_token(scheme: &str) -> bool {
    let mut chars = scheme.chars();
    chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '+' | '.' | '-'))
}

/// One announced endpoint: the declared label and the socket bound for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnouncedEndpoint {
    pub label: String,
    pub binding: EndpointBinding,
}

/// The announced set of one instance, from the first announcement to the
/// seal that ends setup.
#[derive(Debug)]
pub(crate) struct AnnouncedEndpoints {
    declared: BTreeSet<EndpointLabel>,
    announced: BTreeMap<String, EndpointBinding>,
    sealed: bool,
}

impl AnnouncedEndpoints {
    pub(crate) fn new(declared: BTreeSet<EndpointLabel>) -> Self {
        Self {
            declared,
            announced: BTreeMap::new(),
            sealed: false,
        }
    }

    /// Whether the manifest declares any endpoint at all.
    pub(crate) fn declares_any(&self) -> bool {
        !self.declared.is_empty()
    }

    pub(crate) fn announce(&mut self, label: &str, binding: EndpointBinding) -> Result<()> {
        if !self.declared.contains(label) {
            return Err(Error::UndeclaredEndpoint {
                label: label.to_string(),
            });
        }
        if self.sealed {
            return Err(Error::EndpointsSealed {
                label: label.to_string(),
            });
        }
        if self.announced.contains_key(label) {
            return Err(Error::EndpointAlreadyAnnounced {
                label: label.to_string(),
            });
        }
        binding
            .validate()
            .map_err(|reason| Error::InvalidEndpointBinding {
                label: label.to_string(),
                reason,
            })?;
        self.announced.insert(label.to_string(), binding);
        Ok(())
    }

    /// Ends the announcement window and checks the set against the
    /// declaration. A declared label with no announcement is the failure;
    /// an undeclared label never got in. Sealing an already sealed set
    /// answers the same set again.
    pub(crate) fn seal(&mut self) -> Result<Vec<AnnouncedEndpoint>> {
        for label in &self.declared {
            if !self.announced.contains_key(label.as_str()) {
                return Err(Error::EndpointNotAnnounced {
                    label: label.to_string(),
                });
            }
        }
        let sealed = self.snapshot();
        // Named once, on the transition, so a node's own log says what it
        // serves. Under a daemon the reported URLs expand these against every
        // host address; standalone there is no daemon to report them at all,
        // and this line is the only place the operator reads them.
        if !self.sealed {
            for endpoint in &sealed {
                let EndpointBinding {
                    scheme,
                    address,
                    path,
                } = &endpoint.binding;
                tracing::info!(
                    "endpoint `{}` bound at {scheme}://{address}{path}",
                    endpoint.label
                );
            }
        }
        self.sealed = true;
        Ok(sealed)
    }

    /// The announced set so far, in label order.
    pub(crate) fn snapshot(&self) -> Vec<AnnouncedEndpoint> {
        self.announced
            .iter()
            .map(|(label, binding)| AnnouncedEndpoint {
                label: label.clone(),
                binding: binding.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared(labels: &[&str]) -> AnnouncedEndpoints {
        AnnouncedEndpoints::new(
            labels
                .iter()
                .map(|label| EndpointLabel::new(*label).expect("a valid label"))
                .collect(),
        )
    }

    fn binding(scheme: &str, address: &str, path: &str) -> EndpointBinding {
        EndpointBinding {
            scheme: scheme.to_string(),
            address: address.parse().expect("a socket address"),
            path: path.to_string(),
        }
    }

    #[test]
    fn a_declared_label_is_announced_once_and_sealed_in_label_order() {
        let mut endpoints = declared(&["viewer", "panel"]);
        endpoints
            .announce("viewer", binding("https", "0.0.0.0:8080", "/"))
            .expect("declared");
        endpoints
            .announce("panel", binding("http", "127.0.0.1:8765", ""))
            .expect("declared");
        let sealed = endpoints.seal().expect("every declared label is announced");
        assert_eq!(
            sealed,
            vec![
                AnnouncedEndpoint {
                    label: "panel".to_string(),
                    binding: binding("http", "127.0.0.1:8765", ""),
                },
                AnnouncedEndpoint {
                    label: "viewer".to_string(),
                    binding: binding("https", "0.0.0.0:8080", "/"),
                },
            ]
        );
        assert_eq!(
            endpoints.seal().expect("sealing again answers the set"),
            sealed
        );
    }

    #[test]
    fn an_undeclared_label_is_refused() {
        let mut endpoints = declared(&["panel"]);
        let error = endpoints
            .announce("viewer", binding("http", "127.0.0.1:8765", ""))
            .expect_err("undeclared");
        assert!(matches!(error, Error::UndeclaredEndpoint { ref label } if label == "viewer"));
        assert!(error.to_string().contains("`viewer`"), "{error}");
    }

    #[test]
    fn a_second_announcement_of_one_label_is_refused() {
        let mut endpoints = declared(&["panel"]);
        endpoints
            .announce("panel", binding("http", "127.0.0.1:8765", ""))
            .expect("first");
        let error = endpoints
            .announce("panel", binding("http", "127.0.0.1:8766", ""))
            .expect_err("second");
        assert!(matches!(error, Error::EndpointAlreadyAnnounced { ref label } if label == "panel"));
        assert_eq!(
            endpoints.snapshot()[0].binding.address.port(),
            8765,
            "the first announcement stands"
        );
    }

    #[test]
    fn an_announcement_after_the_seal_is_refused() {
        let mut endpoints = declared(&["panel", "viewer"]);
        endpoints
            .announce("panel", binding("http", "127.0.0.1:8765", ""))
            .expect("first");
        endpoints
            .announce("viewer", binding("https", "127.0.0.1:8080", "/"))
            .expect("second");
        endpoints.seal().expect("complete");
        let error = endpoints
            .announce("panel", binding("http", "127.0.0.1:8765", ""))
            .expect_err("sealed");
        assert!(matches!(error, Error::EndpointsSealed { ref label } if label == "panel"));
    }

    #[test]
    fn a_malformed_scheme_or_path_is_refused_naming_the_label() {
        for scheme in ["Http", "1http", ""] {
            let mut endpoints = declared(&["panel"]);
            let error = endpoints
                .announce("panel", binding(scheme, "127.0.0.1:8765", ""))
                .expect_err("malformed scheme");
            assert!(
                matches!(error, Error::InvalidEndpointBinding { ref label, .. } if label == "panel"),
                "{error}"
            );
            assert!(error.to_string().contains("scheme"), "{error}");
        }
        let mut endpoints = declared(&["panel"]);
        let error = endpoints
            .announce("panel", binding("http", "127.0.0.1:8765", "mcp"))
            .expect_err("path without a leading slash");
        assert!(
            matches!(error, Error::InvalidEndpointBinding { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("path"), "{error}");
        assert!(
            endpoints.snapshot().is_empty(),
            "a refused announcement leaves nothing behind"
        );
    }

    #[test]
    fn a_scheme_token_admits_the_uri_alphabet() {
        for scheme in ["http", "https", "ws", "coap+tcp", "x-custom.1"] {
            let mut endpoints = declared(&["panel"]);
            endpoints
                .announce("panel", binding(scheme, "127.0.0.1:8765", "/"))
                .unwrap_or_else(|error| panic!("scheme `{scheme}` is a token: {error}"));
        }
    }

    #[test]
    fn a_declared_label_left_unannounced_fails_the_seal() {
        let mut endpoints = declared(&["panel", "viewer"]);
        endpoints
            .announce("panel", binding("http", "127.0.0.1:8765", ""))
            .expect("first");
        let error = endpoints.seal().expect_err("viewer is missing");
        assert!(matches!(error, Error::EndpointNotAnnounced { ref label } if label == "viewer"));
        assert!(error.to_string().contains("`viewer`"), "{error}");
    }

    #[test]
    fn a_manifest_declaring_nothing_seals_empty() {
        let mut endpoints = declared(&[]);
        assert!(!endpoints.declares_any());
        assert!(endpoints.seal().expect("nothing to check").is_empty());
    }
}
