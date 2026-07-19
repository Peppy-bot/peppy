//! `peppy platform federations`: report the daemon's platform federation state
//! and the core nodes reachable through the hub.
//!
//! The JSON contract is exactly:
//!
//! ```json
//! {
//!   "platform_federation": { "endpoint": "tls/router.example:7447", "status": "federated" },
//!   "daemon_running": true,
//!   "federated_core_nodes": [
//!     { "core_node": "daemon-b", "via": "platform-backend",
//!       "path": ["daemon-a", "platform-backend", "daemon-b"] }
//!   ]
//! }
//! ```
//!
//! Rows come from live, namespace-scoped core-node presence declarations (the
//! CLI session opens under the daemon's namespace, so the listing is inherently
//! workspace-scoped), grouped by core-node name with the local core node
//! excluded and sorted deterministically. Paths are LOGICAL hub paths inferred
//! from the enforced architecture: a managed router's only upstream is the
//! platform hub, so any visible remote presence factually traversed it. They
//! are never measured link-by-link.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use daemon::control::{self as daemon_control, LinkState, QueryStatusOutcome};
use daemon_config::consts::PeppyDirs;
use peppylib::CoreNodePresenceMessenger;
use pmi::CoreNodePresence;
use serde::Serialize;

use super::PLATFORM_HUB_NAME;
use crate::commands::Command;
use crate::commands::stack::table::{render_section_panel, render_table};
use crate::context::AppContext;
use crate::error::{Error, Result};

/// Client-side budget for a cached daemon status query. Kept strictly larger
/// than the daemon's `STATUS_ACK_TIMEOUT` so the daemon can always reply a
/// definite status first.
const STATUS_TIMEOUT: Duration = Duration::from_secs(2);

/// The `platform_federation` object of the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PlatformFederation {
    endpoint: Option<String>,
    status: &'static str,
}

/// One core node reachable through the hub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct FederatedCoreNode {
    core_node: String,
    via: &'static str,
    path: Vec<String>,
}

/// The whole report document. Every field always serializes (an absent
/// endpoint is an explicit `null`), so the JSON contract is byte-stable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct FederationsDocument {
    platform_federation: PlatformFederation,
    daemon_running: bool,
    federated_core_nodes: Vec<FederatedCoreNode>,
}

/// Everything the daemon could tell us about live federation state, resolved
/// from the status query plus whether a daemon was reachable at all.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PlatformView {
    /// The daemon answered the status query.
    Status(daemon_control::FederationStatus),
    /// The daemon acked that it is mid-restart into a new namespace.
    Restarting,
    /// A daemon is running (its messaging answered, or the status query timed
    /// out mid-flight) but its cached status could not be read.
    Unavailable,
    DaemonDown,
    /// An operator-run router (`zenoh.external`) owns federation; the daemon
    /// has no status socket and peppy infers nothing.
    OperatorManaged,
}

impl PlatformView {
    fn daemon_running(&self) -> bool {
        !matches!(self, Self::DaemonDown)
    }
}

/// The report inputs that are not the presence listing: the resolved daemon
/// view plus what the credentials say.
struct AuthState {
    /// A PAT or a stored OAuth session is present.
    authenticated: bool,
    /// The platform endpoint from the credentials' router cache, used only for
    /// states where the daemon could not report an applied endpoint. Never
    /// sufficient for a `federated` status on its own.
    cached_endpoint: Option<String>,
}

/// The built report plus the human-only annotations the JSON contract
/// deliberately excludes.
struct FederationsReport {
    document: FederationsDocument,
    /// The platform link's failure reason (human output only).
    link_error: Option<String>,
    /// The managed router is operator-pinned via `ZENOH_CONFIG`.
    pinned: bool,
    /// The daemon acked a mid-restart status query.
    restarting: bool,
    /// An operator-run router owns federation (external or pinned).
    operator_managed: bool,
    /// The presence listing could not be read from a running daemon.
    presence_unavailable: bool,
}

pub struct FederationsCommand {
    pub json: bool,
    /// Test seam: override the peppy data dirs (the credentials file,
    /// `peppy_config.json5`, the daemon state file, and the control socket all
    /// derive from it).
    pub peppy_dirs: Option<PeppyDirs>,
    /// The `PEPPY_API_KEY` PAT, injected by the dispatcher (never read from the
    /// environment here) so tests stay host-state-free.
    pub pat: Option<String>,
}

impl Command for FederationsCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        let dirs = self.peppy_dirs.unwrap_or_default();
        let config =
            daemon_config::peppy_config::load_or_create(&dirs).map_err(Error::DaemonConfig)?;

        // Managed vs external follows the RUNNING daemon's mode (its state file),
        // the same single source login/logout use, so a disk config edited after
        // the daemon started can never misclassify the report.
        let managed = super::federation_poke_timeout_secs(&dirs, &config).is_some();

        // Load-resilient: a stale or unsupported credentials file reads as
        // not-authenticated (with a warning) rather than failing the report.
        let auth_state = match auth::storage::load(&auth::storage::credentials_path(&dirs)) {
            Ok(credentials) => AuthState {
                authenticated: self.pat.is_some() || credentials.session.is_some(),
                cached_endpoint: credentials.router.map(|router| router.endpoint),
            },
            Err(error) => {
                eprintln!("Warning: could not read stored credentials ({error}).");
                AuthState {
                    authenticated: self.pat.is_some(),
                    cached_endpoint: None,
                }
            }
        };

        // The status query (control socket) and the presence listing (messaging
        // session) use unrelated channels, so run them concurrently; a degraded
        // daemon then costs max(status, presence) instead of their sum. External
        // mode has no control socket, so only the presence side runs.
        let socket = daemon_control::federation_control_socket_path(&dirs);
        let (status_query, presence) = crate::commands::block_on(async {
            let status = tokio::task::spawn_blocking(move || {
                managed.then(|| daemon_control::query_status(&socket, STATUS_TIMEOUT))
            });
            Ok(tokio::join!(status, gather_presence(ctx)))
        })?;
        let status_query = status_query.map_err(|error| {
            Error::ExecutionFailed(format!("federation status query task failed: {error}"))
        })?;
        let (presence_daemon_running, local_core_node, presences, presence_unavailable) =
            presence.unwrap_or((false, None, Vec::new(), false));

        let view = match status_query {
            // External mode: an operator-run router owns federation; peppy
            // neither manages nor infers platform topology. Daemon liveness
            // comes from the messaging probe alone.
            None if presence_daemon_running => PlatformView::OperatorManaged,
            None => PlatformView::DaemonDown,
            Some(QueryStatusOutcome::Status(status)) => PlatformView::Status(status),
            Some(QueryStatusOutcome::Restarting { .. }) => PlatformView::Restarting,
            Some(QueryStatusOutcome::TimedOut) => PlatformView::Unavailable,
            Some(QueryStatusOutcome::DaemonNotRunning) if presence_daemon_running => {
                PlatformView::Unavailable
            }
            Some(QueryStatusOutcome::DaemonNotRunning) => PlatformView::DaemonDown,
            Some(QueryStatusOutcome::DaemonError(message)) => {
                // A malformed daemon reply is a hard CLI error, never an
                // `"error"` status in the report: that status is reserved for
                // a failed platform link.
                return Err(Error::ExecutionFailed(format!(
                    "the daemon could not report federation status: {message}"
                )));
            }
        };

        let report = build_report(
            &view,
            &auth_state,
            local_core_node.as_deref(),
            &presences,
            presence_unavailable,
        );

        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&report.document).map_err(|error| {
                    Error::ExecutionFailed(format!(
                        "could not serialize the federations report: {error}"
                    ))
                })?
            );
        } else {
            print!("{}", format_human(&report));
        }
        Ok(())
    }
}

/// Lists live core-node presence over the daemon's messaging session. Returns
/// `(daemon reachable over messaging, local core-node name, presences,
/// presence unavailable)`. A failed connect is "daemon not reachable"; a
/// connect that succeeds but whose listing fails keeps `daemon_running` true
/// and flags the listing as unavailable.
async fn gather_presence(
    ctx: &Arc<AppContext>,
) -> Result<(bool, Option<String>, Vec<CoreNodePresence>, bool)> {
    let conn = match ctx.connect_to_daemon().await {
        Ok(conn) => conn,
        Err(_) => return Ok((false, None, Vec::new(), false)),
    };
    let local = conn.core_node_name.clone();
    match CoreNodePresenceMessenger::list_live(
        conn.messenger,
        None,
        CoreNodePresenceMessenger::LIST_TIMEOUT,
    )
    .await
    {
        Ok(live) => Ok((true, Some(local), live, false)),
        Err(_) => Ok((true, Some(local), Vec::new(), true)),
    }
}

/// The pure decision core: resolves the report from the daemon view, the
/// credentials state, and the presence listing. Every row of the status
/// decision table is unit-tested against this function.
fn build_report(
    view: &PlatformView,
    auth_state: &AuthState,
    local_core_node: Option<&str>,
    presences: &[CoreNodePresence],
    presence_unavailable: bool,
) -> FederationsReport {
    let mut link_error = None;
    let mut pinned = false;
    let mut operator_managed = false;
    let mut restarting = false;

    // Presence rows are meaningful only while peppy owns the topology: under an
    // operator-run or pinned router the hub-path inference would be a lie, and
    // mid-restart or daemon-down there is no live session to trust.
    let mut rows_allowed = true;

    let (status, endpoint) = match view {
        PlatformView::OperatorManaged => {
            operator_managed = true;
            rows_allowed = false;
            ("operator_managed", None)
        }
        PlatformView::Status(daemon_status) if daemon_status.pinned => {
            pinned = true;
            operator_managed = true;
            rows_allowed = false;
            ("operator_managed", None)
        }
        PlatformView::Status(daemon_status) => match &daemon_status.link_state {
            LinkState::Verified => ("federated", daemon_status.endpoint.clone()),
            LinkState::Unverified => ("connecting", daemon_status.endpoint.clone()),
            LinkState::Error(reason) => {
                link_error = Some(reason.clone());
                ("error", daemon_status.endpoint.clone())
            }
            LinkState::NotConfigured if auth_state.authenticated => {
                ("connecting", auth_state.cached_endpoint.clone())
            }
            LinkState::NotConfigured => ("logged_out", None),
        },
        PlatformView::Restarting => {
            restarting = true;
            rows_allowed = false;
            (
                "status_unavailable",
                auth_state
                    .cached_endpoint
                    .clone()
                    .filter(|_| auth_state.authenticated),
            )
        }
        PlatformView::Unavailable => (
            "status_unavailable",
            auth_state
                .cached_endpoint
                .clone()
                .filter(|_| auth_state.authenticated),
        ),
        PlatformView::DaemonDown if auth_state.authenticated => {
            rows_allowed = false;
            ("daemon_not_running", auth_state.cached_endpoint.clone())
        }
        PlatformView::DaemonDown => {
            rows_allowed = false;
            ("logged_out", None)
        }
    };

    let federated_core_nodes = if rows_allowed {
        federated_core_nodes(local_core_node, presences)
    } else {
        Vec::new()
    };

    FederationsReport {
        document: FederationsDocument {
            platform_federation: PlatformFederation { endpoint, status },
            daemon_running: view.daemon_running(),
            federated_core_nodes,
        },
        link_error,
        pinned,
        restarting,
        operator_managed,
        presence_unavailable,
    }
}

/// Groups live presence into deterministic hub rows: one row per core-node
/// name (instances collapse), the local core node excluded, ascending name
/// order, each path the logical `local -> hub -> remote` relay.
fn federated_core_nodes(
    local_core_node: Option<&str>,
    presences: &[CoreNodePresence],
) -> Vec<FederatedCoreNode> {
    let Some(local) = local_core_node else {
        return Vec::new();
    };
    let names: BTreeSet<&str> = presences
        .iter()
        .map(|presence| presence.core_node.as_str())
        .filter(|name| *name != local)
        .collect();
    names
        .into_iter()
        .map(|name| FederatedCoreNode {
            core_node: name.to_string(),
            via: PLATFORM_HUB_NAME,
            path: vec![
                local.to_string(),
                PLATFORM_HUB_NAME.to_string(),
                name.to_string(),
            ],
        })
        .collect()
}

fn format_human(report: &FederationsReport) -> String {
    let mut out = String::new();

    let mut platform = String::new();
    let endpoint = report
        .document
        .platform_federation
        .endpoint
        .as_deref()
        .unwrap_or("-");
    let _ = writeln!(platform, "endpoint: {endpoint}");
    let _ = writeln!(
        platform,
        "status  : {}",
        report.document.platform_federation.status
    );
    if let Some(reason) = &report.link_error {
        let _ = writeln!(platform, "reason  : {reason}");
    }
    if report.pinned {
        let _ = writeln!(platform, "{PINNED_HUMAN_NOTE}");
    } else if report.operator_managed {
        let _ = writeln!(platform, "{OPERATOR_MANAGED_NOTE}");
    }
    render_section_panel(&mut out, "Platform federation", &platform);
    let _ = writeln!(out);

    let mut nodes = String::new();
    if report.operator_managed {
        let _ = writeln!(
            nodes,
            "(not inferred: an operator-run router owns federation)"
        );
    } else if report.restarting {
        let _ = writeln!(nodes, "(daemon restarting)");
    } else if !report.document.daemon_running {
        let _ = writeln!(nodes, "(daemon not running)");
    } else if report.presence_unavailable {
        let _ = writeln!(nodes, "(core-node presence unavailable; re-run)");
    } else if report.document.federated_core_nodes.is_empty() {
        let _ = writeln!(nodes, "(none visible)");
    } else {
        let rows: Vec<Vec<String>> = report
            .document
            .federated_core_nodes
            .iter()
            .map(|node| {
                vec![
                    node.core_node.clone(),
                    node.via.to_string(),
                    node.path.join(" -> "),
                ]
            })
            .collect();
        render_table(&mut nodes, &["CORE NODE", "VIA", "PATH"], &[rows]);
        let _ = writeln!(
            nodes,
            "Topology is logically inferred from the platform architecture (all traffic \
             relays through the hub); paths are not measured link-by-link."
        );
    }
    render_section_panel(&mut out, "Federated core nodes", &nodes);
    out
}

/// Human note for a pinned managed router (mirrors login/logout wording).
const PINNED_HUMAN_NOTE: &str =
    "note    : an operator-pinned ZENOH_CONFIG owns this router; federation is not auto-managed.";

/// Human note for `zenoh.external` operation.
const OPERATOR_MANAGED_NOTE: &str =
    "note    : an operator-run router owns federation; peppy neither manages nor infers it.";

#[cfg(test)]
mod tests {
    use super::*;

    const HUB: &str = "tls/router.example:7447";
    const CACHED: &str = "tls/cached.example:7447";

    fn status(
        endpoint: Option<&str>,
        link_state: LinkState,
        pinned: bool,
    ) -> daemon_control::FederationStatus {
        daemon_control::FederationStatus {
            endpoint: endpoint.map(str::to_string),
            link_state,
            pinned,
        }
    }

    fn authed_with_cache() -> AuthState {
        AuthState {
            authenticated: true,
            cached_endpoint: Some(CACHED.to_string()),
        }
    }

    fn logged_out() -> AuthState {
        AuthState {
            authenticated: false,
            cached_endpoint: None,
        }
    }

    fn presence(core_node: &str, instance: &str) -> CoreNodePresence {
        CoreNodePresence {
            core_node: core_node.to_string(),
            instance_id: instance.to_string(),
        }
    }

    fn two_remote_presences() -> Vec<CoreNodePresence> {
        vec![
            presence("daemon-c", "gen-1"),
            presence("daemon-b", "gen-1"),
            presence("daemon-b", "gen-2"),
            presence("daemon-a", "gen-1"),
        ]
    }

    #[test]
    fn a_verified_link_reports_federated_with_the_daemon_applied_endpoint() {
        // The A/B report, pinned exactly against the spec's JSON contract.
        let report = build_report(
            &PlatformView::Status(status(Some(HUB), LinkState::Verified, false)),
            &authed_with_cache(),
            Some("daemon-a"),
            &[presence("daemon-a", "gen-1"), presence("daemon-b", "gen-1")],
            false,
        );
        assert_eq!(
            serde_json::to_value(&report.document).unwrap(),
            serde_json::json!({
                "platform_federation": { "endpoint": HUB, "status": "federated" },
                "daemon_running": true,
                "federated_core_nodes": [
                    {
                        "core_node": "daemon-b",
                        "via": "platform-backend",
                        "path": ["daemon-a", "platform-backend", "daemon-b"],
                    }
                ],
            })
        );
    }

    #[test]
    fn an_unverified_link_reports_connecting() {
        let report = build_report(
            &PlatformView::Status(status(Some(HUB), LinkState::Unverified, false)),
            &authed_with_cache(),
            Some("daemon-a"),
            &[],
            false,
        );
        assert_eq!(report.document.platform_federation.status, "connecting");
        assert_eq!(
            report.document.platform_federation.endpoint.as_deref(),
            Some(HUB)
        );
    }

    #[test]
    fn a_link_error_reports_error_and_the_human_output_carries_the_reason() {
        let report = build_report(
            &PlatformView::Status(status(
                Some(HUB),
                LinkState::Error("received fatal alert: UnknownCA".to_string()),
                false,
            )),
            &authed_with_cache(),
            Some("daemon-a"),
            &[],
            false,
        );
        assert_eq!(report.document.platform_federation.status, "error");
        // The JSON stays contract-exact: no reason field.
        let json = serde_json::to_value(&report.document).unwrap();
        assert!(json["platform_federation"].get("reason").is_none());
        assert!(format_human(&report).contains("UnknownCA"));
    }

    #[test]
    fn endpoint_presence_alone_never_reports_federated() {
        // A cached endpoint with no daemon-verified link must not read as
        // federated: NotConfigured reads as connecting, daemon-down as
        // daemon_not_running.
        let not_configured = build_report(
            &PlatformView::Status(status(None, LinkState::NotConfigured, false)),
            &authed_with_cache(),
            Some("daemon-a"),
            &[],
            false,
        );
        assert_eq!(
            not_configured.document.platform_federation.status,
            "connecting"
        );
        assert_eq!(
            not_configured
                .document
                .platform_federation
                .endpoint
                .as_deref(),
            Some(CACHED)
        );

        let down = build_report(
            &PlatformView::DaemonDown,
            &authed_with_cache(),
            None,
            &[],
            false,
        );
        assert_eq!(
            down.document.platform_federation.status,
            "daemon_not_running"
        );
        assert_eq!(
            down.document.platform_federation.endpoint.as_deref(),
            Some(CACHED)
        );
        assert!(!down.document.daemon_running);
    }

    #[test]
    fn logged_out_reports_logged_out_with_a_null_endpoint() {
        let live = build_report(
            &PlatformView::Status(status(None, LinkState::NotConfigured, false)),
            &logged_out(),
            Some("daemon-a"),
            &[],
            false,
        );
        assert_eq!(live.document.platform_federation.status, "logged_out");
        assert_eq!(live.document.platform_federation.endpoint, None);
        assert!(live.document.daemon_running);

        let down = build_report(&PlatformView::DaemonDown, &logged_out(), None, &[], false);
        assert_eq!(down.document.platform_federation.status, "logged_out");
        assert_eq!(down.document.platform_federation.endpoint, None);
        assert!(!down.document.daemon_running);
    }

    #[test]
    fn a_pinned_managed_router_reports_operator_managed_and_infers_nothing() {
        let report = build_report(
            &PlatformView::Status(status(Some(HUB), LinkState::Verified, true)),
            &authed_with_cache(),
            Some("daemon-a"),
            &two_remote_presences(),
            false,
        );
        assert_eq!(
            report.document.platform_federation.status,
            "operator_managed"
        );
        assert_eq!(report.document.platform_federation.endpoint, None);
        assert!(
            report.document.federated_core_nodes.is_empty(),
            "an operator-owned topology must not be inferred"
        );
        assert!(report.pinned);
    }

    #[test]
    fn an_external_router_reports_operator_managed_and_infers_nothing() {
        let report = build_report(
            &PlatformView::OperatorManaged,
            &authed_with_cache(),
            Some("daemon-a"),
            &two_remote_presences(),
            false,
        );
        assert_eq!(
            report.document.platform_federation.status,
            "operator_managed"
        );
        assert_eq!(report.document.platform_federation.endpoint, None);
        assert!(report.document.federated_core_nodes.is_empty());
        assert!(report.document.daemon_running);
    }

    #[test]
    fn a_restarting_daemon_reports_status_unavailable() {
        let report = build_report(
            &PlatformView::Restarting,
            &authed_with_cache(),
            Some("daemon-a"),
            &two_remote_presences(),
            false,
        );
        assert_eq!(
            report.document.platform_federation.status,
            "status_unavailable"
        );
        assert!(report.document.federated_core_nodes.is_empty());
        assert!(report.document.daemon_running);
        assert!(format_human(&report).contains("(daemon restarting)"));
    }

    #[test]
    fn a_status_timeout_reports_status_unavailable_with_the_cached_endpoint() {
        let report = build_report(
            &PlatformView::Unavailable,
            &authed_with_cache(),
            Some("daemon-a"),
            &[presence("daemon-a", "gen-1"), presence("daemon-b", "gen-1")],
            false,
        );
        assert_eq!(
            report.document.platform_federation.status,
            "status_unavailable"
        );
        assert_eq!(
            report.document.platform_federation.endpoint.as_deref(),
            Some(CACHED)
        );
        assert_eq!(
            report.document.federated_core_nodes.len(),
            1,
            "rows presence answered with are still reported"
        );
    }

    #[test]
    fn daemon_liveness_follows_the_resolved_view() {
        assert!(
            PlatformView::Status(status(None, LinkState::NotConfigured, false)).daemon_running()
        );
        assert!(PlatformView::Restarting.daemon_running());
        assert!(PlatformView::Unavailable.daemon_running());
        assert!(PlatformView::OperatorManaged.daemon_running());
        assert!(!PlatformView::DaemonDown.daemon_running());
    }

    #[test]
    fn rows_collapse_instances_exclude_the_local_core_node_and_sort_deterministically() {
        let rows = federated_core_nodes(Some("daemon-a"), &two_remote_presences());
        assert_eq!(
            rows,
            vec![
                FederatedCoreNode {
                    core_node: "daemon-b".to_string(),
                    via: PLATFORM_HUB_NAME,
                    path: vec![
                        "daemon-a".to_string(),
                        PLATFORM_HUB_NAME.to_string(),
                        "daemon-b".to_string()
                    ],
                },
                FederatedCoreNode {
                    core_node: "daemon-c".to_string(),
                    via: PLATFORM_HUB_NAME,
                    path: vec![
                        "daemon-a".to_string(),
                        PLATFORM_HUB_NAME.to_string(),
                        "daemon-c".to_string()
                    ],
                },
            ],
            "instances collapse, the local node is excluded, and names sort ascending"
        );
    }

    #[test]
    fn presence_failure_after_connect_yields_no_rows_but_daemon_running() {
        let report = build_report(
            &PlatformView::Status(status(Some(HUB), LinkState::Verified, false)),
            &authed_with_cache(),
            Some("daemon-a"),
            &[],
            true,
        );
        assert!(report.document.daemon_running);
        assert!(report.document.federated_core_nodes.is_empty());
        assert!(format_human(&report).contains("core-node presence unavailable"));
    }

    #[test]
    fn the_json_contract_is_exact_for_a_three_node_federation() {
        // The A/B/C report: two sorted rows, hub paths throughout.
        let report = build_report(
            &PlatformView::Status(status(Some(HUB), LinkState::Verified, false)),
            &authed_with_cache(),
            Some("daemon-a"),
            &two_remote_presences(),
            false,
        );
        assert_eq!(
            serde_json::to_value(&report.document).unwrap(),
            serde_json::json!({
                "platform_federation": { "endpoint": HUB, "status": "federated" },
                "daemon_running": true,
                "federated_core_nodes": [
                    {
                        "core_node": "daemon-b",
                        "via": "platform-backend",
                        "path": ["daemon-a", "platform-backend", "daemon-b"],
                    },
                    {
                        "core_node": "daemon-c",
                        "via": "platform-backend",
                        "path": ["daemon-a", "platform-backend", "daemon-c"],
                    },
                ],
            })
        );
    }

    #[test]
    fn the_human_report_matches_the_fixture() {
        let report = build_report(
            &PlatformView::Status(status(Some(HUB), LinkState::Verified, false)),
            &authed_with_cache(),
            Some("daemon-a"),
            &two_remote_presences(),
            false,
        );
        let rendered = format_human(&report);
        assert!(rendered.contains("Platform federation"));
        assert!(rendered.contains("endpoint: tls/router.example:7447"));
        assert!(rendered.contains("status  : federated"));
        assert!(rendered.contains("Federated core nodes"));
        assert!(rendered.contains("CORE NODE"));
        assert!(rendered.contains("VIA"));
        assert!(rendered.contains("PATH"));
        assert!(rendered.contains("daemon-a -> platform-backend -> daemon-b"));
        assert!(rendered.contains("daemon-a -> platform-backend -> daemon-c"));
        assert!(
            rendered.contains("logically inferred"),
            "the human report must label the topology as inferred: {rendered}"
        );
    }
}
