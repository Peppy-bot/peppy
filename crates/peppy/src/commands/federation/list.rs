use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::Arc;

use daemon::control::{
    self as daemon_control, FederationStatus, PeerLinkState, QueryStatusOutcome,
};
use daemon_config::consts::PeppyDirs;
use peppylib::CoreNodePresenceMessenger;
use serde::Serialize;

use super::super::Command;
use crate::commands::stack::table::{render_section_panel, render_table};
use crate::context::AppContext;
use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct RouterRow {
    core_node: String,
    endpoint: Option<String>,
    status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct VisibleCoreNode {
    core_node: String,
    instances: usize,
    this_machine: bool,
}

#[derive(Debug, Clone)]
struct SavedBackendState {
    endpoint: Option<String>,
    logged_in: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ListDocument {
    federated_routers: Vec<RouterRow>,
    inbound_listener: Option<String>,
    pinned: bool,
    daemon_running: bool,
    visible_core_nodes: Vec<VisibleCoreNode>,
}

/// Everything the daemon could tell us about live federation state, resolved
/// from the status query plus whether a daemon was reachable at all. Replaces
/// a `(Option<FederationStatus>, availability)` pair whose invalid
/// combinations each renderer had to re-exclude.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DaemonFederationView {
    Live(FederationStatus),
    /// A daemon is running but its cached status could not be read.
    Unavailable,
    DaemonDown,
    /// An operator-run router owns federation; the daemon has no status socket.
    OperatorManaged,
}

impl DaemonFederationView {
    fn daemon_running(&self) -> bool {
        !matches!(self, Self::DaemonDown)
    }
}

pub(super) struct ListCommand {
    pub json: bool,
    pub peppy_dirs: Option<PeppyDirs>,
}

impl Command for ListCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        let dirs = self.peppy_dirs.unwrap_or_default();
        let config =
            daemon_config::peppy_config::load_or_create(&dirs).map_err(Error::DaemonConfig)?;
        let registry = federation::load(&federation::registry_path(&dirs))?;
        let credentials = auth::storage::load(&auth::storage::credentials_path(&dirs))?;
        let saved_backend = SavedBackendState {
            logged_in: credentials.session.is_some(),
            endpoint: credentials.router.map(|router| router.endpoint),
        };

        // The status query (control socket) and the presence listing (messaging
        // session) use unrelated channels, so run them concurrently; a degraded
        // daemon then costs max(status, presence) instead of their sum.
        let socket = daemon_control::federation_control_socket_path(&dirs);
        let (status_query, presence) = crate::commands::block_on(async {
            let status = tokio::task::spawn_blocking(move || {
                daemon_control::query_status(&socket, super::STATUS_TIMEOUT)
            });
            Ok(tokio::join!(status, visible_core_nodes(ctx)))
        })?;
        let status_query = status_query.map_err(|error| {
            Error::ExecutionFailed(format!("federation status query task failed: {error}"))
        })?;
        let (presence_daemon_running, visible_core_nodes) = presence.unwrap_or((false, Vec::new()));
        let external = config.zenoh.external_endpoint().is_some();
        let view = match status_query {
            QueryStatusOutcome::Status(status) => DaemonFederationView::Live(status),
            QueryStatusOutcome::TimedOut => DaemonFederationView::Unavailable,
            QueryStatusOutcome::DaemonNotRunning if external && presence_daemon_running => {
                DaemonFederationView::OperatorManaged
            }
            QueryStatusOutcome::DaemonNotRunning if presence_daemon_running => {
                DaemonFederationView::Unavailable
            }
            QueryStatusOutcome::DaemonNotRunning => DaemonFederationView::DaemonDown,
            QueryStatusOutcome::DaemonError(message) => {
                return Err(Error::ExecutionFailed(format!(
                    "the daemon could not report federation status: {message}. Restart the daemon \
                     after upgrading, then re-run `peppy federation list`"
                )));
            }
        };
        let daemon_running = view.daemon_running();
        let document = build_document(
            &registry,
            &view,
            saved_backend,
            config
                .zenoh
                .federation()
                .and_then(|federation| federation.listen_endpoint.clone()),
            daemon_running,
            visible_core_nodes,
        );

        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&document).map_err(|error| {
                    Error::ExecutionFailed(format!("could not serialize federation list: {error}"))
                })?
            );
        } else {
            print!("{}", format_human(&document));
        }
        Ok(())
    }
}

fn build_document(
    registry: &federation::Federations,
    view: &DaemonFederationView,
    saved_backend: SavedBackendState,
    configured_listener: Option<String>,
    daemon_running: bool,
    visible_core_nodes: Vec<VisibleCoreNode>,
) -> ListDocument {
    let mut federated_routers = Vec::with_capacity(registry.peers().len() + 1);
    let backend = match view {
        DaemonFederationView::Live(status) => status.backend.clone(),
        _ => saved_backend.endpoint.clone(),
    };
    let backend_status = match view {
        DaemonFederationView::Live(status) if status.pinned && status.backend.is_some() => {
            "pinned (operator-managed)"
        }
        DaemonFederationView::Live(status) if status.backend.is_some() => "federated",
        DaemonFederationView::Live(_) if saved_backend.logged_in => "pending",
        DaemonFederationView::Live(_) => "logged out",
        DaemonFederationView::OperatorManaged => "operator-managed",
        DaemonFederationView::Unavailable => "status unavailable",
        DaemonFederationView::DaemonDown if saved_backend.logged_in => {
            "pending (daemon not running)"
        }
        DaemonFederationView::DaemonDown => "logged out",
    };
    federated_routers.push(RouterRow {
        core_node: federation::RESERVED_BACKEND_NAME.to_string(),
        endpoint: backend,
        status: backend_status.to_string(),
    });

    for peer in registry.peers() {
        let status = match view {
            DaemonFederationView::Live(status) if status.pinned => {
                "pinned (operator-managed)".to_string()
            }
            DaemonFederationView::Live(status) => {
                let report = status
                    .peers
                    .iter()
                    .find(|report| report.endpoint == peer.endpoint().as_str());
                match report {
                    Some(report) => match &report.state {
                        PeerLinkState::Unverified => "pending verification".to_string(),
                        PeerLinkState::Error(reason) => format!("error: {reason}"),
                        PeerLinkState::Verified => "federated".to_string(),
                    },
                    None => "pending".to_string(),
                }
            }
            DaemonFederationView::OperatorManaged => "operator-managed".to_string(),
            DaemonFederationView::Unavailable => "status unavailable".to_string(),
            DaemonFederationView::DaemonDown => "pending (daemon not running)".to_string(),
        };
        federated_routers.push(RouterRow {
            core_node: peer.core_node().unwrap_or("-").to_string(),
            endpoint: Some(peer.endpoint().to_string()),
            status,
        });
    }

    // A durable removal is published before the live apply. If that apply is
    // still retrying, retain the currently applied endpoint in the report so a
    // network link can never disappear from observability while it is live.
    if let DaemonFederationView::Live(status) = view {
        let saved: BTreeSet<&str> = registry
            .peers()
            .iter()
            .map(|peer| peer.endpoint().as_str())
            .collect();
        for report in status
            .peers
            .iter()
            .filter(|report| !saved.contains(report.endpoint.as_str()))
        {
            federated_routers.push(RouterRow {
                core_node: "-".to_string(),
                endpoint: Some(report.endpoint.clone()),
                status: "removal pending".to_string(),
            });
        }
    }

    let live_status = match view {
        DaemonFederationView::Live(status) => Some(status),
        _ => None,
    };
    ListDocument {
        federated_routers,
        inbound_listener: match live_status {
            Some(status) => status.listen_endpoint.clone(),
            None => configured_listener,
        },
        pinned: live_status.is_some_and(|status| status.pinned),
        daemon_running,
        visible_core_nodes,
    }
}

async fn visible_core_nodes(ctx: &Arc<AppContext>) -> Result<(bool, Vec<VisibleCoreNode>)> {
    let conn = match ctx.connect_to_daemon().await {
        Ok(conn) => conn,
        Err(_) => return Ok((false, Vec::new())),
    };
    let live = match CoreNodePresenceMessenger::list_live(
        conn.messenger,
        None,
        CoreNodePresenceMessenger::LIST_TIMEOUT,
    )
    .await
    {
        Ok(live) => live,
        Err(_) => return Ok((true, Vec::new())),
    };
    let mut claims: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for presence in live {
        claims
            .entry(presence.core_node)
            .or_default()
            .insert(presence.instance_id);
    }
    let nodes = claims
        .into_iter()
        .map(|(core_node, instances)| VisibleCoreNode {
            this_machine: core_node == conn.core_node_name,
            core_node,
            instances: instances.len(),
        })
        .collect();
    Ok((true, nodes))
}

fn format_human(document: &ListDocument) -> String {
    let mut out = String::new();
    let mut routers = String::new();
    let rows: Vec<Vec<String>> = document
        .federated_routers
        .iter()
        .map(|row| {
            vec![
                row.core_node.clone(),
                row.endpoint.clone().unwrap_or_else(|| "-".to_string()),
                row.status.clone(),
            ]
        })
        .collect();
    render_table(&mut routers, &["CORE NODE", "ENDPOINT", "STATUS"], &[rows]);
    let listener = document.inbound_listener.as_deref().unwrap_or("disabled");
    let _ = writeln!(routers, "inbound listener: {listener}");
    if document.pinned {
        let _ = writeln!(routers, "{}", crate::commands::auth::PINNED_NOTE);
    }
    render_section_panel(&mut out, "Federated routers", &routers);
    let _ = writeln!(out);

    let mut nodes = String::new();
    if !document.daemon_running {
        let _ = writeln!(nodes, "(daemon not running)");
    } else if document.visible_core_nodes.is_empty() {
        let _ = writeln!(nodes, "(none visible)");
    } else {
        let rows = document
            .visible_core_nodes
            .iter()
            .map(|node| {
                vec![
                    node.core_node.clone(),
                    node.instances.to_string(),
                    if node.this_machine {
                        "(this machine)".to_string()
                    } else {
                        String::new()
                    },
                ]
            })
            .collect();
        render_table(&mut nodes, &["CORE NODE", "INSTANCES", "LOCATION"], &[rows]);
    }
    render_section_panel(&mut out, "Visible core nodes", &nodes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use daemon::control::PeerReport;

    fn registry() -> federation::Federations {
        let mut registry = federation::Federations::default();
        registry
            .insert(
                federation::FederationPeer::new(
                    "tls/peer:7449".to_string(),
                    Some("daemon-b".to_string()),
                )
                .unwrap(),
            )
            .unwrap();
        registry
    }

    fn saved_backend(logged_in: bool) -> SavedBackendState {
        SavedBackendState {
            endpoint: None,
            logged_in,
        }
    }

    #[test]
    fn formatter_has_both_sections_and_peer_status() {
        let document = build_document(
            &registry(),
            &DaemonFederationView::Live(FederationStatus {
                backend: None,
                peers: vec![PeerReport {
                    endpoint: "tls/peer:7449".to_string(),
                    state: PeerLinkState::Verified,
                }],
                listen_endpoint: None,
                pinned: false,
            }),
            saved_backend(false),
            None,
            true,
            vec![VisibleCoreNode {
                core_node: "daemon-b".to_string(),
                instances: 1,
                this_machine: false,
            }],
        );
        let rendered = format_human(&document);
        assert!(rendered.contains("Federated routers"));
        assert!(rendered.contains("Visible core nodes"));
        assert!(rendered.contains("daemon-b"));
        assert!(rendered.contains("federated"));
        assert!(rendered.contains("platform-backend"));
        assert!(rendered.contains("logged out"));
    }

    #[test]
    fn daemon_down_marks_saved_links_pending() {
        let document = build_document(
            &registry(),
            &DaemonFederationView::DaemonDown,
            saved_backend(false),
            Some("tls/0.0.0.0:7449".to_string()),
            false,
            Vec::new(),
        );
        assert_eq!(
            document.federated_routers[1].status,
            "pending (daemon not running)"
        );
        assert!(format_human(&document).contains("(daemon not running)"));
    }

    #[test]
    fn daemon_liveness_comes_from_the_resolved_view() {
        assert!(DaemonFederationView::Live(FederationStatus::default()).daemon_running());
        assert!(DaemonFederationView::Unavailable.daemon_running());
        assert!(DaemonFederationView::OperatorManaged.daemon_running());
        assert!(!DaemonFederationView::DaemonDown.daemon_running());
    }

    #[test]
    fn saved_login_without_a_cached_router_is_pending_not_logged_out() {
        let down = build_document(
            &federation::Federations::default(),
            &DaemonFederationView::DaemonDown,
            saved_backend(true),
            None,
            false,
            Vec::new(),
        );
        assert_eq!(
            down.federated_routers[0].status,
            "pending (daemon not running)"
        );

        let live = build_document(
            &federation::Federations::default(),
            &DaemonFederationView::Live(FederationStatus::default()),
            saved_backend(true),
            None,
            true,
            Vec::new(),
        );
        assert_eq!(live.federated_routers[0].status, "pending");
    }

    #[test]
    fn startup_seeded_peer_is_pending_until_an_explicit_verification() {
        let document = build_document(
            &registry(),
            &DaemonFederationView::Live(FederationStatus {
                backend: None,
                peers: vec![PeerReport {
                    endpoint: "tls/peer:7449".to_string(),
                    state: PeerLinkState::Unverified,
                }],
                listen_endpoint: None,
                pinned: false,
            }),
            saved_backend(false),
            Some("tls/0.0.0.0:7449".to_string()),
            true,
            Vec::new(),
        );

        assert_eq!(document.federated_routers[1].status, "pending verification");
    }

    #[test]
    fn live_removed_endpoint_stays_visible_as_removal_pending() {
        let document = build_document(
            &federation::Federations::default(),
            &DaemonFederationView::Live(FederationStatus {
                backend: None,
                peers: vec![PeerReport {
                    endpoint: "tls/old-peer:7449".to_string(),
                    state: PeerLinkState::Verified,
                }],
                listen_endpoint: None,
                pinned: false,
            }),
            saved_backend(false),
            Some("tls/0.0.0.0:7555".to_string()),
            true,
            Vec::new(),
        );
        assert_eq!(document.federated_routers[1].status, "removal pending");
        assert_eq!(document.inbound_listener, None);
    }

    #[test]
    fn pinned_status_never_claims_desired_links_are_federated() {
        let document = build_document(
            &registry(),
            &DaemonFederationView::Live(FederationStatus {
                backend: Some("tls/backend:7443".to_string()),
                peers: vec![PeerReport {
                    endpoint: "tls/peer:7449".to_string(),
                    state: PeerLinkState::Verified,
                }],
                listen_endpoint: None,
                pinned: true,
            }),
            saved_backend(true),
            Some("tls/0.0.0.0:7449".to_string()),
            true,
            Vec::new(),
        );
        assert!(
            document
                .federated_routers
                .iter()
                .all(|row| row.status != "federated")
        );
        assert_eq!(document.inbound_listener, None);
    }
}
