//! `peppy platform status`: this machine's enrollment and the renewal of its
//! certificate, whether the daemon runs under it with a verified link to the
//! cloud router, the context the commands use, and (when signed in) what the
//! platform reports about the router and this peer.

use std::sync::Arc;

use daemon::control::{self as daemon_control, PokeOutcome};
use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::commands::platform::context::{context_json, enrollment_note};
use crate::commands::platform::peers::peer_status_label;
use crate::commands::platform::router::pending_change_lines;
use crate::commands::platform::{PlatformSession, date_of, federation_is_managed};
use crate::context::AppContext;
use crate::error::{Error, Result};
use auth::client::{self, PeerStatus, RouterPhase, RouterStatus};
use auth::enrollment::{Enrollment, EnrollmentDocument};
use auth::{FederationIdentity, storage};

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

/// What the daemon needs to renew the certificate, and does not have. The
/// daemon renews with the session of this machine, so it needs the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RenewalMeans {
    session: bool,
    daemon_running: bool,
}

impl RenewalMeans {
    fn of(daemon: &DaemonReport, platform: &Result<Option<PlatformReport>>) -> Self {
        Self {
            // The platform view is `Ok(None)` exactly when there is no session
            // to ask with.
            session: !matches!(platform, Ok(None)),
            daemon_running: !matches!(daemon, DaemonReport::NotRunning),
        }
    }

    /// Each thing that is absent: its name for a machine, and the command
    /// that adds it for a person.
    fn absent(self) -> impl Iterator<Item = (&'static str, &'static str)> {
        [
            (
                self.session,
                "no_session",
                "needs a session: run `peppy platform login`",
            ),
            (
                self.daemon_running,
                "daemon_not_running",
                "needs the daemon: start it with `peppy service serve`",
            ),
        ]
        .into_iter()
        .filter(|(present, _, _)| !present)
        .map(|(_, name, remedy)| (name, remedy))
    }

    fn absent_names(self) -> Vec<&'static str> {
        self.absent().map(|(name, _)| name).collect()
    }

    fn absent_remedies(self) -> Vec<&'static str> {
        self.absent().map(|(_, remedy)| remedy).collect()
    }
}

/// The platform's view, when a session allowed asking for it.
struct PlatformReport {
    router: RouterStatus,
    /// This peer's status as the platform reports it, if it still lists it.
    peer_status: Option<PeerStatus>,
}

impl Command for StatusCommand {
    fn execute(self, _ctx: &Arc<AppContext>) -> Result<()> {
        let session = PlatformSession::resolve(self.peppy_dirs, self.api_url.as_deref())?;
        let enrollment = session.enrollment()?;
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
    let expected = FederationIdentity::of(enrollment);
    let namespace_matches = state.namespace == expected.namespace;
    if !federation_is_managed(Some(state), &session.config) {
        return DaemonReport::External { namespace_matches };
    }
    let identity_matches = state.runs_under(&expected);
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
    // The router read lists every enrolled peer with its status, so this
    // peer's status is in it, when the platform still lists the peer.
    let peer_status = router
        .peers
        .iter()
        .find(|peer| peer.id == document.peer_id)
        .map(|peer| peer.status.clone());
    Ok(Some(PlatformReport {
        router,
        peer_status,
    }))
}

/// The certificate line: when it expires, or that it did.
fn certificate_line(document: &EnrollmentDocument, now: i64) -> String {
    let expires_at = document.certificate_expires_at;
    if document.is_expired(now) {
        return format!(
            "EXPIRED on {}; if the renewal does not succeed, run `peppy platform enroll \
             --replace`",
            date_of(expires_at)
        );
    }
    let days_left = (expires_at - now).div_euclid(DAY_SECS);
    format!("expires on {} (in {days_left} days)", date_of(expires_at))
}

/// The renewal lines: when the daemon renews the certificate, and what it
/// needs to do so and does not have.
fn renewal_lines(document: &EnrollmentDocument, now: i64, means: RenewalMeans) -> Vec<String> {
    let due_on = date_of(document.renewal_due_at());
    let absent = means.absent_remedies();
    let schedule = match (document.is_renewal_due(now), absent.is_empty()) {
        (false, _) => format!("automatic, from {due_on}"),
        (true, true) => format!("due since {due_on}; the daemon tries again each hour"),
        (true, false) => format!("due since {due_on}"),
    };
    std::iter::once(schedule)
        .chain(absent.into_iter().map(str::to_string))
        .collect()
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
            out.push_str(&format!("  cert      : {}\n", certificate_line(d, now)));
            let means = RenewalMeans::of(daemon, platform);
            for (index, line) in renewal_lines(d, now, means).iter().enumerate() {
                let label = if index == 0 {
                    "renewal   :"
                } else {
                    "           "
                };
                out.push_str(&format!("  {label} {line}\n"));
            }
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
    out.push_str("Context\n");
    match session.context() {
        Ok(None) => out.push_str("  none; run `peppy platform configure` to select a project\n"),
        Ok(Some(context)) => {
            out.push_str(&format!("  {}\n", context.label()));
            if let Some(note) = enrollment_note(&context, enrollment.map(|e| &e.document)) {
                out.push_str(&format!("  {note}\n"));
            }
        }
        Err(e) => out.push_str(&format!("  unavailable: {e}\n")),
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
                    .map(|address| format!(" at {}:{}", address.host(), address.port()))
                    .unwrap_or_default();
                out.push_str(&format!("  router    : {}{address}\n", report.router.phase));
                if report.router.phase == RouterPhase::Stopped {
                    out.push_str("              start it with `peppy platform router start`\n");
                }
                for line in pending_change_lines(&report.router) {
                    out.push_str(&format!("  pending   : {line}\n"));
                }
                out.push_str(&format!(
                    "  this peer : {}\n",
                    report
                        .peer_status
                        .as_ref()
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
            "certificate_issued_at": d.certificate_issued_at,
            "certificate_expires_at": d.certificate_expires_at,
            "certificate_expired": d.is_expired(now),
            "certificate_renewal_due_at": d.renewal_due_at(),
            "certificate_renewal_due": d.is_renewal_due(now),
            "certificate_renewal_needs": RenewalMeans::of(daemon, platform).absent_names(),
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
                "address": report.router.address.as_ref().map(|address| {
                    serde_json::json!({ "host": address.host(), "port": address.port() })
                }),
                "pending_changes": report.router.pending_changes,
            },
            "this_peer_status": report.peer_status,
        }),
        Err(e) => serde_json::json!({ "error": e.to_string() }),
    };
    let context = match session.context() {
        Ok(context) => context_json(context.as_ref()),
        Err(e) => serde_json::json!({ "error": e.to_string() }),
    };
    serde_json::json!({
        "enrolled": enrollment.is_some(),
        "enrollment": enrollment_json,
        "daemon": daemon_json,
        "context": context,
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

    use auth::test_support::ISSUED_AT;

    const EXPIRES_AT: i64 = ISSUED_AT + 90 * DAY_SECS;
    const DUE_AT: i64 = ISSUED_AT + 60 * DAY_SECS;
    const ALL_MEANS: RenewalMeans = RenewalMeans {
        session: true,
        daemon_running: true,
    };

    /// The record of a certificate issued at [`ISSUED_AT`] for 90 days.
    fn document() -> EnrollmentDocument {
        EnrollmentDocument {
            certificate_issued_at: ISSUED_AT,
            certificate_expires_at: EXPIRES_AT,
            ..auth::test_support::enrollment_document()
        }
    }

    #[test]
    fn the_certificate_line_gives_the_expiry_and_says_when_it_is_past() {
        let document = document();
        assert_eq!(
            certificate_line(&document, EXPIRES_AT - 30 * DAY_SECS),
            format!("expires on {} (in 30 days)", date_of(EXPIRES_AT))
        );
        let expired = certificate_line(&document, EXPIRES_AT);
        assert!(expired.starts_with("EXPIRED"), "{expired}");
        assert!(
            expired.contains("peppy platform enroll --replace"),
            "{expired}"
        );
    }

    #[test]
    fn the_renewal_lines_give_the_schedule() {
        let document = document();
        assert_eq!(
            renewal_lines(&document, DUE_AT - 1, ALL_MEANS),
            [format!("automatic, from {}", date_of(DUE_AT))]
        );
        assert_eq!(
            renewal_lines(&document, DUE_AT, ALL_MEANS),
            [format!(
                "due since {}; the daemon tries again each hour",
                date_of(DUE_AT)
            )]
        );
    }

    /// Each thing the daemon needs and does not have is one line with its
    /// command, before the renewal is due and after.
    #[test]
    fn the_renewal_lines_name_what_the_daemon_needs() {
        let document = document();
        let no_session = RenewalMeans {
            session: false,
            daemon_running: true,
        };
        let nothing = RenewalMeans {
            session: false,
            daemon_running: false,
        };

        let lines = renewal_lines(&document, ISSUED_AT, no_session);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[1].contains("peppy platform login"), "{lines:?}");

        let lines = renewal_lines(&document, DUE_AT, nothing);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0], format!("due since {}", date_of(DUE_AT)));
        assert!(lines[1].contains("peppy platform login"), "{lines:?}");
        assert!(lines[2].contains("peppy service serve"), "{lines:?}");
        assert_eq!(nothing.absent_names(), ["no_session", "daemon_not_running"]);
        assert!(ALL_MEANS.absent_names().is_empty());
    }
}
