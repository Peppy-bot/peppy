//! `peppy platform status`: this machine's enrollment, whether the daemon
//! runs under it with a verified link to the cloud router, and (when signed
//! in) what the platform reports about the router and this peer.

use std::sync::Arc;

use daemon::control::{self as daemon_control, PokeOutcome};
use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::commands::platform::peers::peer_status_label;
use crate::commands::platform::router::pending_change_lines;
use crate::commands::platform::{PlatformSession, date_of, federation_is_managed};
use crate::context::AppContext;
use crate::error::{Error, Result};
use auth::client::{self, RouterStatus};
use auth::enrollment::{self, Enrollment};
use auth::storage;

/// Certificates expiring within this many days are called out.
const EXPIRY_WARNING_DAYS: i64 = 14;
const DAY_SECS: i64 = 24 * 60 * 60;

pub struct StatusCommand {
    pub api_url: Option<String>,
    pub json: bool,
    /// Test seam: override the peppy data dirs.
    pub peppy_dirs: Option<PeppyDirs>,
}

/// What the daemon says about the enrollment, from its state file and a poke.
enum DaemonReport {
    NotRunning,
    /// Running, but under an operator-run external router: the enrollment
    /// only sets its session namespace.
    External {
        namespace_matches: bool,
    },
    /// Running with a managed router; `link` is the poke's answer.
    Managed {
        identity_matches: bool,
        link: PokeOutcome,
    },
}

/// The platform's view, when a session allowed asking for it.
struct PlatformReport {
    router: RouterStatus,
    /// This peer's status as the platform reports it, if it still lists it.
    peer_status: Option<String>,
}

impl Command for StatusCommand {
    fn execute(self, _ctx: &Arc<AppContext>) -> Result<()> {
        let session = PlatformSession::resolve(self.peppy_dirs, self.api_url.as_deref())?;
        let enrollment = enrollment::load(&session.dirs).map_err(Error::AuthEngine)?;
        let daemon = daemon_report(&session, enrollment.as_ref());
        let platform = match &enrollment {
            Some(enrollment) => platform_report(&session, enrollment),
            None => Ok(None),
        };
        let now = storage::now_unix();

        if self.json {
            println!(
                "{}",
                json_document(&session, enrollment.as_ref(), &daemon, &platform, now)
            );
            return Ok(());
        }
        print!(
            "{}",
            human_document(&session, enrollment.as_ref(), &daemon, &platform, now)
        );
        Ok(())
    }
}

fn daemon_report(session: &PlatformSession, enrollment: Option<&Enrollment>) -> DaemonReport {
    let Some(state) = session.daemon_state.as_ref().filter(|s| s.is_running()) else {
        return DaemonReport::NotRunning;
    };
    let expected_namespace = enrollment
        .map(|e| e.document.namespace.clone())
        .unwrap_or_else(config::namespace::Namespace::local);
    let namespace_matches = state.namespace == expected_namespace;
    if !federation_is_managed(Some(state), &session.config) {
        return DaemonReport::External { namespace_matches };
    }
    let identity_matches = namespace_matches
        && enrollment.is_none_or(|e| state.router_id.as_ref() == Some(&e.document.zenoh_id));
    let socket = daemon_control::federation_control_socket_path(&session.dirs);
    let link = daemon_control::poke_refederate(&socket, daemon_control::POKE_READ_TIMEOUT);
    DaemonReport::Managed {
        identity_matches,
        link,
    }
}

/// `Ok(None)` when there is no session to ask with; an API refusal is an error
/// so a broken platform view is never mistaken for a healthy one.
fn platform_report(
    session: &PlatformSession,
    enrollment: &Enrollment,
) -> Result<Option<PlatformReport>> {
    let mut cred = match session.credential() {
        Ok(cred) => cred,
        Err(Error::AuthEngine(auth::AuthError::NotAuthenticated)) => return Ok(None),
        Err(e) => return Err(e),
    };
    let document = &enrollment.document;
    let router = client::router_status(
        &session.http,
        &session.api_url,
        &mut cred,
        &document.workspace_id,
        &document.project_id,
    )?;
    let peers = client::list_peers(
        &session.http,
        &session.api_url,
        &mut cred,
        &document.workspace_id,
        &document.project_id,
    )?;
    let peer_status = peers
        .into_iter()
        .find(|p| p.id == document.peer_id)
        .map(|p| p.status);
    Ok(Some(PlatformReport {
        router,
        peer_status,
    }))
}

/// The certificate line: when it expires, and a warning when that is near or
/// past.
fn certificate_line(expires_at: i64, now: i64) -> String {
    let days_left = (expires_at - now).div_euclid(DAY_SECS);
    if now >= expires_at {
        return format!(
            "EXPIRED on {}; run `peppy platform enroll --replace`",
            date_of(expires_at)
        );
    }
    if days_left < EXPIRY_WARNING_DAYS {
        return format!(
            "expires on {} (in {days_left} days); run `peppy platform enroll --replace` soon",
            date_of(expires_at)
        );
    }
    format!("expires on {} (in {days_left} days)", date_of(expires_at))
}

fn link_line(link: &PokeOutcome) -> String {
    match link {
        PokeOutcome::Applied(Some(locator)) => format!("verified ({locator})"),
        PokeOutcome::Applied(None) => "standalone (not enrolled)".to_string(),
        PokeOutcome::Pinned => "operator-pinned ZENOH_CONFIG (not managed)".to_string(),
        PokeOutcome::Unreachable(reason) => format!("unreachable: {reason}"),
        PokeOutcome::DaemonError(msg) => format!("error: {msg}"),
        PokeOutcome::DaemonNotRunning => "no daemon answered".to_string(),
        PokeOutcome::TimedOut => "the daemon did not answer within the timeout".to_string(),
        PokeOutcome::Restarting => "the daemon is restarting to apply the enrollment".to_string(),
    }
}

fn human_document(
    session: &PlatformSession,
    enrollment: Option<&Enrollment>,
    daemon: &DaemonReport,
    platform: &Result<Option<PlatformReport>>,
    now: i64,
) -> String {
    let mut out = String::new();
    match enrollment {
        None => {
            out.push_str("Not enrolled. Run `peppy platform enroll` to join a project router.\n")
        }
        Some(enrollment) => {
            let d = &enrollment.document;
            out.push_str("Enrollment\n");
            out.push_str(&format!(
                "  project   : {} (workspace {})\n",
                d.project_id, d.workspace_id
            ));
            out.push_str(&format!("  peer      : {} ({})\n", d.peer_name, d.peer_id));
            out.push_str(&format!("  router    : {}\n", d.router.locator()));
            out.push_str(&format!("  zenoh id  : {}\n", d.zenoh_id));
            out.push_str(&format!(
                "  cert      : {}\n",
                certificate_line(d.certificate_expires_at, now)
            ));
            out.push_str(&format!("  backend   : {}\n", d.api_url));
            out.push_str(&format!(
                "  files     : {}\n",
                session.dirs.peer_dir().display()
            ));
        }
    }
    out.push_str("Daemon\n");
    match daemon {
        DaemonReport::NotRunning => out.push_str("  running   : no\n"),
        DaemonReport::External { namespace_matches } => {
            out.push_str("  running   : yes (operator-run router; federation is the operator's)\n");
            out.push_str(&format!(
                "  namespace : {}\n",
                if *namespace_matches {
                    "matches the enrollment"
                } else {
                    "differs; restart the daemon"
                }
            ));
        }
        DaemonReport::Managed {
            identity_matches,
            link,
        } => {
            out.push_str("  running   : yes\n");
            out.push_str(&format!(
                "  identity  : {}\n",
                if *identity_matches {
                    "matches the enrollment"
                } else {
                    "differs; the daemon is restarting or needs a restart"
                }
            ));
            out.push_str(&format!("  link      : {}\n", link_line(link)));
        }
    }
    if enrollment.is_some() {
        out.push_str("Platform\n");
        match platform {
            Ok(None) => out.push_str(
                "  not signed in; run `peppy platform login` to see the platform's view\n",
            ),
            Ok(Some(report)) => {
                let address = report
                    .router
                    .address
                    .as_ref()
                    .map(|a| format!(" at {}:{}", a.host, a.port))
                    .unwrap_or_default();
                out.push_str(&format!("  router    : {}{address}\n", report.router.phase));
                if report.router.phase == "stopped" {
                    out.push_str("              start it with `peppy platform router start`\n");
                }
                for line in pending_change_lines(&report.router) {
                    out.push_str(&format!("  pending   : {line}\n"));
                }
                out.push_str(&format!(
                    "  this peer : {}\n",
                    report
                        .peer_status
                        .as_deref()
                        .map(peer_status_label)
                        .unwrap_or("not listed (removed?)")
                ));
            }
            Err(e) => out.push_str(&format!("  unavailable: {e}\n")),
        }
    }
    out
}

fn json_document(
    session: &PlatformSession,
    enrollment: Option<&Enrollment>,
    daemon: &DaemonReport,
    platform: &Result<Option<PlatformReport>>,
    now: i64,
) -> serde_json::Value {
    let enrollment_json = enrollment.map(|e| {
        let d = &e.document;
        serde_json::json!({
            "api_url": d.api_url,
            "workspace_id": d.workspace_id,
            "project_id": d.project_id,
            "peer_id": d.peer_id,
            "peer_name": d.peer_name,
            "zenoh_id": d.zenoh_id.as_str(),
            "namespace": d.namespace.as_str(),
            "router": d.router.locator(),
            "certificate_expires_at": d.certificate_expires_at,
            "certificate_expired": d.is_expired(now),
            "enrolled_at": d.enrolled_at,
            "peer_dir": session.dirs.peer_dir(),
        })
    });
    let daemon_json = match daemon {
        DaemonReport::NotRunning => serde_json::json!({ "running": false }),
        DaemonReport::External { namespace_matches } => serde_json::json!({
            "running": true, "router": "external", "namespace_matches": namespace_matches,
        }),
        DaemonReport::Managed {
            identity_matches,
            link,
        } => serde_json::json!({
            "running": true, "router": "managed", "identity_matches": identity_matches,
            "link": link_json(link),
        }),
    };
    let platform_json = match platform {
        Ok(None) => serde_json::Value::Null,
        Ok(Some(report)) => serde_json::json!({
            "router": {
                "phase": report.router.phase,
                "desired_state": report.router.desired_state,
                "address": report.router.address.as_ref().map(|a| serde_json::json!({ "host": a.host, "port": a.port })),
                "pending_changes": report.router.pending_changes,
            },
            "this_peer_status": report.peer_status,
        }),
        Err(e) => serde_json::json!({ "error": e.to_string() }),
    };
    serde_json::json!({
        "enrolled": enrollment.is_some(),
        "enrollment": enrollment_json,
        "daemon": daemon_json,
        "platform": platform_json,
    })
}

fn link_json(link: &PokeOutcome) -> serde_json::Value {
    let (state, detail) = match link {
        PokeOutcome::Applied(Some(locator)) => ("verified", Some(locator.clone())),
        PokeOutcome::Applied(None) => ("standalone", None),
        PokeOutcome::Pinned => ("pinned", None),
        PokeOutcome::Unreachable(reason) => ("unreachable", Some(reason.clone())),
        PokeOutcome::DaemonError(msg) => ("error", Some(msg.clone())),
        PokeOutcome::DaemonNotRunning => ("no_daemon", None),
        PokeOutcome::TimedOut => ("timed_out", None),
        PokeOutcome::Restarting => ("restarting", None),
    };
    serde_json::json!({ "state": state, "detail": detail })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_certificate_line_warns_near_and_past_expiry() {
        let expires = 1_800_000_000;
        assert_eq!(
            certificate_line(expires, expires - 30 * DAY_SECS),
            format!("expires on {} (in 30 days)", date_of(expires))
        );
        assert!(certificate_line(expires, expires - 3 * DAY_SECS).contains("in 3 days"));
        assert!(certificate_line(expires, expires - 3 * DAY_SECS).contains("--replace"));
        assert!(certificate_line(expires, expires).starts_with("EXPIRED"));
    }
}
