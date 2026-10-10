//! `peppy platform status`: this machine's enrollment and the renewal of its
//! certificate, whether the daemon runs under it with a verified link to the
//! cloud router, the workspace and the project the commands use, and (when
//! signed in) what the platform reports about the router and this peer.

use std::sync::Arc;

use daemon::control::{self as daemon_control, PokeOutcome};
use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::commands::platform::peers::peer_status_label;
use crate::commands::platform::router::pending_change_lines;
use crate::commands::platform::selection::{enrollment_note, selection_json};
use crate::commands::platform::{PlatformSession, date_of, federation_is_managed};
use crate::context::AppContext;
use crate::error::Result;
use auth::client::{PeerStatus, RouterPhase, RouterStatus};
use auth::enrollment::Enrollment;
use auth::{CertificateValidity, FederationIdentity, storage};

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
    has_session: bool,
    is_daemon_running: bool,
}

impl RenewalMeans {
    fn of(daemon: &DaemonReport, platform: &Result<Option<PlatformReport>>) -> Self {
        Self {
            // The platform view is `Ok(None)` exactly when there is no session
            // to ask with.
            has_session: !matches!(platform, Ok(None)),
            is_daemon_running: !matches!(daemon, DaemonReport::NotRunning),
        }
    }

    /// Each thing that is absent: its name for a machine, and the command
    /// that adds it for a person.
    fn absent(self) -> impl Iterator<Item = (&'static str, &'static str)> {
        [
            (
                self.has_session,
                "no_session",
                "needs a session: run `peppy platform login`",
            ),
            (
                self.is_daemon_running,
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
    let Some(mut api) = session.signed_in_api()? else {
        return Ok(None);
    };
    let document = &enrollment.document;
    let router = api.router_status(&document.workspace_id, &document.project_id)?;
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
fn certificate_line(certificate: &CertificateValidity, now: i64) -> String {
    let expires_at = certificate.not_after;
    if certificate.is_expired(now) {
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
fn renewal_lines(certificate: &CertificateValidity, now: i64, means: RenewalMeans) -> Vec<String> {
    let due_on = date_of(certificate.renewal_due_at());
    let absent = means.absent_remedies();
    let schedule = match (certificate.is_renewal_due(now), absent.is_empty()) {
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
            out.push_str(&format!(
                "  cert      : {}\n",
                certificate_line(&enrollment.certificate, now)
            ));
            let means = RenewalMeans::of(daemon, platform);
            for (index, line) in renewal_lines(&enrollment.certificate, now, means)
                .iter()
                .enumerate()
            {
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
    out.push_str("Selection\n");
    match session.selection() {
        Ok(None) => out.push_str("  none; run `peppy platform project use` to select a project\n"),
        Ok(Some(selection)) => {
            match selection.project_label() {
                Some(project) => out.push_str(&format!("  {project}\n")),
                None => out.push_str(&format!(
                    "  {}; no project is selected\n",
                    selection.workspace_label()
                )),
            }
            if let Some(note) = enrollment_note(&selection, enrollment.map(|e| &e.document)) {
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
        let certificate = &e.certificate;
        serde_json::json!({
            "api_url": d.api_url,
            "workspace_id": d.workspace_id,
            "project_id": d.project_id,
            "peer_id": d.peer_id,
            "peer_name": d.peer_name,
            "zenoh_id": d.zenoh_id.as_str(),
            "namespace": d.namespace.as_str(),
            "router": d.router.locator(),
            "certificate_not_before": certificate.not_before,
            "certificate_not_after": certificate.not_after,
            "certificate_expired": certificate.is_expired(now),
            "certificate_renewal_due_at": certificate.renewal_due_at(),
            "certificate_renewal_due": certificate.is_renewal_due(now),
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
    let selection = match session.selection() {
        Ok(selection) => selection_json(selection.as_ref()),
        Err(e) => serde_json::json!({ "error": e.to_string() }),
    };
    serde_json::json!({
        "enrolled": enrollment.is_some(),
        "enrollment": enrollment_json,
        "daemon": daemon_json,
        "selection": selection,
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
        has_session: true,
        is_daemon_running: true,
    };

    /// A certificate issued at [`ISSUED_AT`] for 90 days.
    fn certificate() -> CertificateValidity {
        CertificateValidity {
            not_before: ISSUED_AT,
            not_after: EXPIRES_AT,
        }
    }

    #[test]
    fn the_certificate_line_gives_the_expiry_and_says_when_it_is_past() {
        let certificate = certificate();
        assert_eq!(
            certificate_line(&certificate, EXPIRES_AT - 30 * DAY_SECS),
            format!("expires on {} (in 30 days)", date_of(EXPIRES_AT))
        );
        let expired = certificate_line(&certificate, EXPIRES_AT);
        assert!(expired.starts_with("EXPIRED"), "{expired}");
        assert!(
            expired.contains("peppy platform enroll --replace"),
            "{expired}"
        );
    }

    #[test]
    fn the_renewal_lines_give_the_schedule() {
        let certificate = certificate();
        assert_eq!(
            renewal_lines(&certificate, DUE_AT - 1, ALL_MEANS),
            [format!("automatic, from {}", date_of(DUE_AT))]
        );
        assert_eq!(
            renewal_lines(&certificate, DUE_AT, ALL_MEANS),
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
        let certificate = certificate();
        let no_session = RenewalMeans {
            has_session: false,
            is_daemon_running: true,
        };
        let nothing = RenewalMeans {
            has_session: false,
            is_daemon_running: false,
        };

        let lines = renewal_lines(&certificate, ISSUED_AT, no_session);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[1].contains("peppy platform login"), "{lines:?}");

        let lines = renewal_lines(&certificate, DUE_AT, nothing);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0], format!("due since {}", date_of(DUE_AT)));
        assert!(lines[1].contains("peppy platform login"), "{lines:?}");
        assert!(lines[2].contains("peppy service serve"), "{lines:?}");
        assert_eq!(nothing.absent_names(), ["no_session", "daemon_not_running"]);
        assert!(ALL_MEANS.absent_names().is_empty());
    }

    /// A session over `dir` whose platform API no test reaches: every test
    /// below stops at the credentials file.
    fn session_in(dir: &tempfile::TempDir) -> PlatformSession {
        PlatformSession::resolve(Some(PeppyDirs::new(dir.path())), Some("http://127.0.0.1:9"))
            .expect("session")
    }

    #[test]
    fn the_platform_view_is_absent_without_a_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let report = platform_report(&session_in(&dir), &auth::test_support::enrollment());
        assert!(matches!(report, Ok(None)));
    }

    /// A credentials file that cannot be read is not the absence of a
    /// session, so the platform view is an error instead of left out.
    #[test]
    fn an_unreadable_session_is_an_error_of_the_platform_view() {
        let dir = tempfile::tempdir().expect("temp dir");
        let session = session_in(&dir);
        let creds_dir = session.creds_path.parent().expect("credentials dir");
        std::fs::create_dir_all(creds_dir).expect("mkdir conf");
        std::fs::write(
            &session.creds_path,
            r#"{ session: { api_url: "http://x" } }"#,
        )
        .expect("write malformed creds");
        let report = platform_report(&session, &auth::test_support::enrollment());
        assert!(report.is_err());
    }

    /// [`session_in`], signed in as the subject of
    /// [`auth::test_support::platform_selection`], so a stored selection of
    /// this backend is the selection of the session.
    fn signed_in_session(dir: &tempfile::TempDir) -> PlatformSession {
        let session = session_in(dir);
        let creds = auth::ProfileCreds {
            api_url: session.api_url.clone(),
            issuer: session.api_url.clone(),
            client_id: "cli-client-id".to_string(),
            access_token: storage::secret("access".to_string()),
            refresh_token: storage::secret("refresh".to_string()),
            expires_at: auth::test_support::EXPIRES_AT,
            token_type: "Bearer".to_string(),
            scope: "openid".to_string(),
            subject: "user-123".to_string(),
            username: "alice".to_string(),
        };
        let credentials = storage::Credentials {
            session: Some(creds),
            ..Default::default()
        };
        storage::save(&session.creds_path, &credentials).expect("seed creds");
        session
    }

    /// Stores `project` in workspace `ws-2` as the selection of
    /// [`signed_in_session`].
    fn select(session: &PlatformSession, project: Option<auth::selection::Named>) {
        let selection = auth::PlatformSelection {
            api_origin: session.api_origin().expect("api origin"),
            project,
            ..auth::test_support::platform_selection()
        };
        auth::selection::save(&session.dirs, &selection).expect("save selection");
    }

    fn router(phase: RouterPhase) -> RouterStatus {
        RouterStatus {
            phase,
            desired_state: String::new(),
            address: None,
            pending_changes: false,
            pending_change_entries: Vec::new(),
            peers: Vec::new(),
        }
    }

    /// A machine that is not enrolled reads one line for it, then the daemon
    /// and the selection, and no platform section.
    #[test]
    fn the_status_of_a_machine_not_enrolled_has_no_platform_section() {
        let dir = tempfile::tempdir().expect("temp dir");
        let session = session_in(&dir);

        let text = human_document(
            &session,
            None,
            &DaemonReport::NotRunning,
            &Ok(None),
            ISSUED_AT,
        );

        let expected = [
            "Not enrolled. Run `peppy platform enroll` to join a project router.",
            "Daemon",
            "  running   : no",
            "Selection",
            "  none; run `peppy platform project use` to select a project",
            "",
        ]
        .join("\n");
        assert_eq!(text, expected);
    }

    /// An enrolled machine under an operator-run router, with a platform view
    /// that had no session to ask with: the enrollment with each renewal line,
    /// the namespace check, the selected project with the note of a different
    /// enrollment, and the command that signs in. `human_document` takes the
    /// platform view as given, so the test signs in only because the session
    /// reads a stored selection for the signed-in identity alone.
    #[test]
    fn the_status_under_an_external_router_reads_the_namespace_and_the_selection() {
        let dir = tempfile::tempdir().expect("temp dir");
        let session = signed_in_session(&dir);
        select(&session, auth::test_support::platform_selection().project);
        let enrollment = auth::test_support::enrollment();

        let text = human_document(
            &session,
            Some(&enrollment),
            &DaemonReport::External {
                namespace_matches: false,
            },
            &Ok(None),
            ISSUED_AT + DAY_SECS,
        );

        let expected = [
            "Enrollment",
            "  project   : 550e8400-e29b-41d4-a716-446655440000 (workspace ws-1)",
            "  peer      : robot-7 (peer-1)",
            "  router    : tls/rtr-p.example:7447",
            "  zenoh id  : 2f6c1d8e9a0b4c5d6e7f8091a2b3c4d5",
            "  cert      : expires on 2033-05-18 (in 3471 days)",
            "  renewal   : automatic, from 2030-03-17",
            "              needs a session: run `peppy platform login`",
            "  backend   : https://api.example",
            &format!("  files     : {}", session.dirs.peer_dir().display()),
            "Daemon",
            "  running   : yes (operator-run router; federation is the operator's)",
            "  namespace : differs; restart the daemon",
            "Selection",
            "  project Field (p-2) in workspace Robotics lab",
            "  This machine is enrolled in project 550e8400-e29b-41d4-a716-446655440000. \
             The selection does not move it. To move it: `peppy platform enroll --replace`",
            "Platform",
            "  not signed in; run `peppy platform login` to see the platform's view",
            "",
        ]
        .join("\n");
        assert_eq!(text, expected);
    }

    /// An enrolled machine under a managed router: the identity and the
    /// link, a workspace selected alone, and the platform's view of a
    /// stopped router that no longer lists this peer.
    #[test]
    fn the_status_under_a_managed_router_reads_the_link_and_the_platform_view() {
        let dir = tempfile::tempdir().expect("temp dir");
        let session = signed_in_session(&dir);
        select(&session, None);
        let enrollment = auth::test_support::enrollment();

        let text = human_document(
            &session,
            Some(&enrollment),
            &DaemonReport::Managed {
                identity_matches: true,
                link: PokeOutcome::Pinned,
            },
            &Ok(Some(PlatformReport {
                router: router(RouterPhase::Stopped),
                peer_status: None,
            })),
            ISSUED_AT + DAY_SECS,
        );

        let expected = [
            "Enrollment",
            "  project   : 550e8400-e29b-41d4-a716-446655440000 (workspace ws-1)",
            "  peer      : robot-7 (peer-1)",
            "  router    : tls/rtr-p.example:7447",
            "  zenoh id  : 2f6c1d8e9a0b4c5d6e7f8091a2b3c4d5",
            "  cert      : expires on 2033-05-18 (in 3471 days)",
            "  renewal   : automatic, from 2030-03-17",
            "  backend   : https://api.example",
            &format!("  files     : {}", session.dirs.peer_dir().display()),
            "Daemon",
            "  running   : yes",
            "  identity  : matches the enrollment",
            "  link      : operator-pinned ZENOH_CONFIG (not managed)",
            "Selection",
            "  workspace Robotics lab (ws-2); no project is selected",
            "Platform",
            "  router    : stopped",
            "              start it with `peppy platform router start`",
            "  this peer : not listed (removed?)",
            "",
        ]
        .join("\n");
        assert_eq!(text, expected);
    }

    /// A running router with its address, a change that waits for a restart
    /// and this peer's status; an identity that differs; a selection that
    /// cannot be read.
    #[test]
    fn the_status_reads_a_running_router_and_an_unreadable_selection() {
        let dir = tempfile::tempdir().expect("temp dir");
        let session = signed_in_session(&dir);
        std::fs::write(
            session
                .dirs
                .conf_dir()
                .join(daemon_config::consts::PLATFORM_SELECTION_FILE),
            "not a selection",
        )
        .expect("write a malformed selection");
        let enrollment = auth::test_support::enrollment();
        let staged_at = chrono::DateTime::from_timestamp(ISSUED_AT, 0).expect("a time");
        let running = RouterStatus {
            address: Some(
                auth::enrollment::RouterEndpoint::parse("rtr-p.example", 7447)
                    .expect("an endpoint"),
            ),
            pending_changes: true,
            pending_change_entries: vec![auth::client::PendingChange {
                description: "Peer robot-8 was removed".to_string(),
                staged_at,
                deadline: None,
            }],
            ..router(RouterPhase::Running)
        };

        let text = human_document(
            &session,
            Some(&enrollment),
            &DaemonReport::Managed {
                identity_matches: false,
                link: PokeOutcome::TimedOut,
            },
            &Ok(Some(PlatformReport {
                router: running,
                peer_status: Some(PeerStatus::Connected),
            })),
            ISSUED_AT + DAY_SECS,
        );

        let unreadable = session.selection().expect_err("a malformed selection");
        let expected = [
            "Enrollment",
            "  project   : 550e8400-e29b-41d4-a716-446655440000 (workspace ws-1)",
            "  peer      : robot-7 (peer-1)",
            "  router    : tls/rtr-p.example:7447",
            "  zenoh id  : 2f6c1d8e9a0b4c5d6e7f8091a2b3c4d5",
            "  cert      : expires on 2033-05-18 (in 3471 days)",
            "  renewal   : automatic, from 2030-03-17",
            "  backend   : https://api.example",
            &format!("  files     : {}", session.dirs.peer_dir().display()),
            "Daemon",
            "  running   : yes",
            "  identity  : differs; the daemon is restarting or needs a restart",
            "  link      : the daemon did not answer within the timeout",
            "Selection",
            &format!("  unavailable: {unreadable}"),
            "Platform",
            "  router    : running at rtr-p.example:7447",
            "  pending   : Peer robot-8 was removed (staged 2023-11-14)",
            "  this peer : connected",
            "",
        ]
        .join("\n");
        assert_eq!(text, expected);
    }

    /// A platform view that failed says so in the platform section, and a
    /// daemon that does not run is what the renewal needs.
    #[test]
    fn the_status_reads_a_platform_view_that_failed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let session = session_in(&dir);
        let enrollment = auth::test_support::enrollment();

        let text = human_document(
            &session,
            Some(&enrollment),
            &DaemonReport::NotRunning,
            &Err(crate::error::Error::ExecutionFailed(
                "the router read failed".to_string(),
            )),
            ISSUED_AT + DAY_SECS,
        );

        let expected = [
            "Enrollment",
            "  project   : 550e8400-e29b-41d4-a716-446655440000 (workspace ws-1)",
            "  peer      : robot-7 (peer-1)",
            "  router    : tls/rtr-p.example:7447",
            "  zenoh id  : 2f6c1d8e9a0b4c5d6e7f8091a2b3c4d5",
            "  cert      : expires on 2033-05-18 (in 3471 days)",
            "  renewal   : automatic, from 2030-03-17",
            "              needs the daemon: start it with `peppy service serve`",
            "  backend   : https://api.example",
            &format!("  files     : {}", session.dirs.peer_dir().display()),
            "Daemon",
            "  running   : no",
            "Selection",
            "  none; run `peppy platform project use` to select a project",
            "Platform",
            "  unavailable: the router read failed",
            "",
        ]
        .join("\n");
        assert_eq!(text, expected);
    }
}
