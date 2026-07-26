//! `peppy platform list`: the core nodes registered to this workspace, and
//! whether each is alive right now.
//!
//! Two machines logged into the same account already federate to the platform's
//! shared router under one workspace namespace and can address each other.
//! Nothing in the CLI showed that; this does.
//!
//! # A pure HTTP client
//!
//! The command resolves the session credential, issues one
//! `GET /me/core-nodes`, and renders. It opens no zenoh session, connects to no
//! daemon, and runs no presence query of its own. Every status printed is the
//! platform's view, which is the only view that can see other sites at all: a
//! local query would describe this machine's own mesh, which is not what the
//! command claims to show.
//!
//! There is deliberately no local fallback when the backend is unreachable. A
//! fallback could only answer a different question, and it would double the
//! code and the tests to serve an outage.
//!
//! The one local read is the `(this machine)` marker, and it never fails the
//! command.

use std::sync::Arc;

use daemon::state::DaemonState;
use daemon_config::consts::PeppyDirs;
use daemon_config::peppy_config::PeppyConfig;

use crate::commands::Command;
use crate::commands::platform::PlatformSession;
use crate::context::AppContext;
use crate::error::{Error, Result};
use auth::client::{self, CoreNodeListing};
use auth::resolver;

pub struct ListCommand {
    pub api_url: Option<String>,
    /// Emit machine-readable JSON instead of human text.
    pub json: bool,
    /// Test seam: override the peppy data dirs (the credentials file and
    /// `peppy_config.json5` both derive from it).
    pub peppy_dirs: Option<PeppyDirs>,
}

impl Command for ListCommand {
    fn execute(self, _ctx: &Arc<AppContext>) -> Result<()> {
        let session = PlatformSession::resolve(self.peppy_dirs, self.api_url.as_deref())?;
        let mut cred =
            match resolver::resolve(&session.creds_path, &session.http, resolver::pat_from_env()) {
                Ok(cred) => cred,
                // Unlike `whoami`, whose output *is* the sign-in state, this
                // command cannot produce its output at all without a credential.
                // So it fails: a script gets the uniform "empty stdout, non-zero
                // exit" contract rather than having to tell a partial document from
                // a complete one.
                Err(auth::AuthError::NotAuthenticated) => {
                    return Err(Error::Auth(
                        "Not authenticated. Run `peppy platform login`.".to_string(),
                    ));
                }
                Err(e) => return Err(e.into()),
            };

        let listing = client::list_core_nodes(&session.http, &session.api_url, &mut cred)?;
        let this_machine = this_machine_name(
            session.daemon_state.as_ref(),
            &session.config,
            &listing.workspace_id,
        );

        if self.json {
            print!("{}", render_json(&listing, &session.api_url));
        } else {
            print!("{}", render_human(&listing, &session.api_url, this_machine));
        }
        if !listing.application_status_available {
            eprintln!(
                "Warning: the platform could not read liveliness, so application status is \
                 unknown for every core node. The roster itself is unaffected."
            );
        }
        Ok(())
    }
}

/// This machine's core-node name, when it can be established *and* this machine
/// belongs to the workspace being listed.
///
/// The namespace gate is the point. A daemon that has not restarted since a
/// login is still running under the previous workspace, so its recorded
/// core-node name says nothing about the workspace in this response, and a name
/// that happens to exist in both would otherwise be labelled as this machine
/// when it is someone else's. The daemon's state file already records its
/// namespace, so this is a comparison rather than new plumbing.
///
/// Falls back to the configured name when no daemon is running, and to nothing
/// when neither is available. Never fails the command: the marker is a
/// convenience, and a machine with no daemon and no configured name is a normal
/// case.
fn this_machine_name(
    daemon_state: Option<&DaemonState>,
    config: &PeppyConfig,
    workspace_id: &str,
) -> Option<String> {
    let Some(state) = daemon_state else {
        // No daemon running, so there is no namespace to compare against. The
        // configured name is what a daemon started now would claim.
        return config.core_node_name.clone();
    };
    match state.namespace.as_str() == workspace_id {
        true => Some(state.core_node_name.clone()),
        false => None,
    }
}

/// Human-readable output. Pure, so the column layout, the marker, the collision
/// suffix, and the empty case are all testable without a backend.
fn render_human(listing: &CoreNodeListing, api_url: &str, this_machine: Option<String>) -> String {
    let mut out = format!("Workspace {} (backend {api_url})\n\n", listing.workspace_id);
    if listing.core_nodes.is_empty() {
        out.push_str(
            "No core nodes registered in this workspace. \
             Run 'peppy platform login' on a machine to register its daemon.\n",
        );
        return out;
    }

    let rows: Vec<(String, String, String, bool)> = ordered_rows(listing, this_machine.as_deref())
        .into_iter()
        .map(|(entry, is_this_machine)| {
            (
                entry.core_node_name.clone(),
                application_column(entry),
                registered_column(entry),
                is_this_machine,
            )
        })
        .collect();

    let name_width = column_width("CORE NODE", rows.iter().map(|(name, ..)| name.as_str()));
    let status_width = column_width(
        "APPLICATION",
        rows.iter().map(|(_, status, ..)| status.as_str()),
    );

    out.push_str(&format!(
        "{:<name_width$}  {:<status_width$}  {}\n",
        "CORE NODE", "APPLICATION", "REGISTERED"
    ));
    for (name, status, registered, is_this_machine) in rows {
        let marker = match is_this_machine {
            true => "   (this machine)",
            false => "",
        };
        out.push_str(&format!(
            "{name:<name_width$}  {status:<status_width$}  {registered}{marker}\n"
        ));
    }
    out
}

/// The entries in render order: this machine first, then by name ascending.
///
/// The backend already orders by name, so this only hoists the local row.
/// Deterministic either way, which is what lets the tests assert on it
/// directly.
fn ordered_rows<'a>(
    listing: &'a CoreNodeListing,
    this_machine: Option<&str>,
) -> Vec<(&'a client::CoreNodeEntry, bool)> {
    let is_this_machine = |entry: &client::CoreNodeEntry| {
        this_machine.is_some_and(|name| name == entry.core_node_name)
    };
    let mut rows: Vec<(&client::CoreNodeEntry, bool)> = listing
        .core_nodes
        .iter()
        .map(|entry| (entry, is_this_machine(entry)))
        .collect();
    rows.sort_by_key(|(entry, is_this_machine)| (!is_this_machine, entry.core_node_name.clone()));
    rows
}

/// The APPLICATION column: the status, with the claimant count appended only
/// when a name is contested. A collision must be visible, since the losing
/// daemon refuses to boot and that is frequently why a site will not come up.
fn application_column(entry: &client::CoreNodeEntry) -> String {
    let Some(status) = &entry.application.status else {
        return "unknown".to_string();
    };
    match entry.application.live_claimants {
        Some(claimants) if claimants > 1 => format!("{status} ({claimants} claimants)"),
        _ => status.clone(),
    }
}

/// The REGISTERED column: the UTC date this name was first registered, or `-`
/// for a name that is live but has no registry row.
///
/// Deliberately not the config-pull timestamp. Rendering that as "last seen"
/// would read as liveness, and it is not one. UTC rather than local time, so
/// the output does not depend on the machine's timezone.
fn registered_column(entry: &client::CoreNodeEntry) -> String {
    entry
        .first_seen_at
        .as_deref()
        .and_then(|timestamp| timestamp.split('T').next())
        .unwrap_or("-")
        .to_string()
}

fn column_width<'a>(header: &str, values: impl Iterator<Item = &'a str>) -> usize {
    values
        .map(str::len)
        .chain([header.len()])
        .max()
        .unwrap_or(0)
}

/// Machine-readable output. Passes the platform's entries through with the
/// `api_url` this CLI called added, so a script can tell which backend answered.
fn render_json(listing: &CoreNodeListing, api_url: &str) -> String {
    let core_nodes: Vec<serde_json::Value> = listing
        .core_nodes
        .iter()
        .map(|entry| {
            serde_json::json!({
                "core_node_name": entry.core_node_name,
                "registered": entry.registered,
                "first_seen_at": entry.first_seen_at,
                "last_config_pull_at": entry.last_config_pull_at,
                "application": {
                    "status": entry.application.status,
                    "live_claimants": entry.application.live_claimants,
                },
            })
        })
        .collect();
    let doc = serde_json::json!({
        "workspace_id": listing.workspace_id,
        "api_url": api_url,
        "application_status_available": listing.application_status_available,
        "core_nodes": core_nodes,
    });
    format!("{doc}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use auth::client::{ApplicationStatus, CoreNodeEntry};

    const WORKSPACE: &str = "4f1b2e2c-9a71-4d0e-b3c8-0d2b9f6a11c4";
    const API_URL: &str = "https://api.peppy.dev";

    fn entry(
        name: &str,
        registered: bool,
        status: Option<&str>,
        claimants: Option<u32>,
    ) -> CoreNodeEntry {
        registered_on(name, registered, status, claimants, "2026-07-01")
    }

    /// [`entry`] with an explicit registration date, for the layout test.
    fn registered_on(
        name: &str,
        registered: bool,
        status: Option<&str>,
        claimants: Option<u32>,
        first_seen_date: &str,
    ) -> CoreNodeEntry {
        CoreNodeEntry {
            core_node_name: name.to_string(),
            registered,
            first_seen_at: registered.then(|| format!("{first_seen_date}T10:00:00Z")),
            last_config_pull_at: registered.then(|| "2026-07-24T09:12:33Z".to_string()),
            application: ApplicationStatus {
                status: status.map(str::to_string),
                live_claimants: claimants,
            },
        }
    }

    fn listing(entries: Vec<CoreNodeEntry>, available: bool) -> CoreNodeListing {
        CoreNodeListing {
            workspace_id: WORKSPACE.to_string(),
            application_status_available: available,
            core_nodes: entries,
        }
    }

    #[test]
    fn human_output_renders_status_the_marker_and_a_collision() {
        let listing = listing(
            vec![
                registered_on("cn-a1b2c3d4e5", true, Some("online"), Some(1), "2026-07-01"),
                registered_on(
                    "cn-9f8e7d6c5b",
                    true,
                    Some("offline"),
                    Some(0),
                    "2026-07-14",
                ),
                entry("lab-bench-external", false, Some("online"), Some(2)),
            ],
            true,
        );

        let out = render_human(&listing, API_URL, Some("cn-a1b2c3d4e5".to_string()));

        assert_eq!(
            out,
            "Workspace 4f1b2e2c-9a71-4d0e-b3c8-0d2b9f6a11c4 (backend https://api.peppy.dev)\n\
             \n\
             CORE NODE           APPLICATION           REGISTERED\n\
             cn-a1b2c3d4e5       online                2026-07-01   (this machine)\n\
             cn-9f8e7d6c5b       offline               2026-07-14\n\
             lab-bench-external  online (2 claimants)  -\n"
        );
    }

    #[test]
    fn this_machine_sorts_first_and_the_rest_by_name() {
        let listing = listing(
            vec![
                entry("cn-aaa", true, Some("online"), Some(1)),
                entry("cn-bbb", true, Some("online"), Some(1)),
                entry("cn-zzz", true, Some("online"), Some(1)),
            ],
            true,
        );

        let ordered: Vec<&str> = ordered_rows(&listing, Some("cn-zzz"))
            .into_iter()
            .map(|(entry, _)| entry.core_node_name.as_str())
            .collect();

        assert_eq!(ordered, vec!["cn-zzz", "cn-aaa", "cn-bbb"]);
    }

    #[test]
    fn without_a_local_name_nothing_is_marked() {
        let listing = listing(vec![entry("cn-aaa", true, Some("online"), Some(1))], true);

        let out = render_human(&listing, API_URL, None);

        assert!(
            !out.contains("(this machine)"),
            "no marker without a resolved local name: {out}"
        );
    }

    /// A single claimant is the normal case and must not be annotated: only a
    /// contested name earns the suffix.
    #[test]
    fn only_a_contested_name_shows_a_claimant_count() {
        assert_eq!(
            application_column(&entry("cn", true, Some("online"), Some(1))),
            "online"
        );
        assert_eq!(
            application_column(&entry("cn", true, Some("online"), Some(3))),
            "online (3 claimants)"
        );
    }

    #[test]
    fn an_unavailable_observer_renders_unknown_in_every_row() {
        let listing = listing(
            vec![
                entry("cn-aaa", true, None, None),
                entry("cn-bbb", true, None, None),
            ],
            false,
        );

        let out = render_human(&listing, API_URL, None);

        assert_eq!(out.matches("unknown").count(), 2);
        assert!(!out.contains("offline"), "unknown is not offline: {out}");
    }

    #[test]
    fn an_empty_roster_explains_how_to_populate_it() {
        let out = render_human(&listing(vec![], true), API_URL, None);

        assert!(out.contains("No core nodes registered in this workspace"));
        assert!(out.contains("peppy platform login"));
        assert!(!out.contains("CORE NODE"), "no header for an empty roster");
    }

    #[test]
    fn an_unregistered_entry_shows_no_registration_date() {
        assert_eq!(
            registered_column(&entry("lab-bench", false, Some("online"), Some(1))),
            "-"
        );
        assert_eq!(
            registered_column(&entry("cn-a", true, Some("online"), Some(1))),
            "2026-07-01",
            "the date only, in UTC, so output does not depend on the machine's timezone"
        );
    }

    #[test]
    fn json_output_carries_the_backend_and_the_honest_timestamp_name() {
        let listing = listing(
            vec![entry("cn-a1b2c3d4e5", true, Some("online"), Some(1))],
            true,
        );

        let doc: serde_json::Value =
            serde_json::from_str(&render_json(&listing, API_URL)).expect("valid json");

        assert_eq!(doc["workspace_id"], serde_json::json!(WORKSPACE));
        assert_eq!(doc["api_url"], serde_json::json!(API_URL));
        assert_eq!(doc["application_status_available"], serde_json::json!(true));
        let entry = &doc["core_nodes"][0];
        assert_eq!(entry["registered"], serde_json::json!(true));
        assert_eq!(
            entry["last_config_pull_at"],
            serde_json::json!("2026-07-24T09:12:33Z")
        );
        assert!(
            entry.get("last_seen_at").is_none(),
            "the pull timestamp never appears under a name that reads as liveness"
        );
        assert_eq!(entry["application"]["status"], serde_json::json!("online"));
    }

    // ─── The `(this machine)` marker ──────────────────────────────────────

    fn daemon_in(core_node_name: &str, namespace: &str) -> DaemonState {
        DaemonState::new(
            core_node_name,
            "127.0.0.1",
            7447,
            "test-git-hash",
            30,
            config::namespace::Namespace::parse(namespace).expect("valid namespace"),
            Some(30),
        )
    }

    fn config_named(core_node_name: Option<&str>) -> PeppyConfig {
        PeppyConfig {
            core_node_name: core_node_name.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn the_marker_uses_the_running_daemons_name() {
        let daemon = daemon_in("cn-local", WORKSPACE);

        assert_eq!(
            this_machine_name(Some(&daemon), &config_named(None), WORKSPACE),
            Some("cn-local".to_string())
        );
    }

    /// The mid-login case, and the reason the marker is gated on the namespace
    /// at all: the daemon has not restarted yet, so it is still running under
    /// the previous workspace. Its name says nothing about the workspace being
    /// listed, and an identical name in both would otherwise be labelled as
    /// this machine when it belongs to someone else.
    #[test]
    fn the_marker_is_withheld_when_the_daemon_runs_in_another_workspace() {
        let daemon = daemon_in("cn-local", "local");

        assert_eq!(
            this_machine_name(Some(&daemon), &config_named(Some("cn-local")), WORKSPACE),
            None,
            "a daemon in another namespace must not be marked, even by an exact name match, \
             and must not fall through to the configured name either"
        );
    }

    #[test]
    fn the_marker_falls_back_to_the_configured_name_with_no_daemon_running() {
        assert_eq!(
            this_machine_name(None, &config_named(Some("cn-configured")), WORKSPACE),
            Some("cn-configured".to_string()),
            "with no daemon there is no namespace to compare, so the configured name is what a \
             daemon started now would claim"
        );
    }

    #[test]
    fn the_marker_is_simply_absent_when_neither_source_resolves() {
        assert_eq!(
            this_machine_name(None, &config_named(None), WORKSPACE),
            None
        );
    }

    #[test]
    fn json_nulls_the_status_when_the_observer_could_not_answer() {
        let listing = listing(vec![entry("cn-a", true, None, None)], false);

        let doc: serde_json::Value =
            serde_json::from_str(&render_json(&listing, API_URL)).expect("valid json");

        assert_eq!(
            doc["application_status_available"],
            serde_json::json!(false)
        );
        assert!(doc["core_nodes"][0]["application"]["status"].is_null());
        assert!(doc["core_nodes"][0]["application"]["live_claimants"].is_null());
    }
}
