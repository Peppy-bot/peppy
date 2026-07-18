use std::collections::BTreeSet;
use std::sync::Arc;

use daemon::control::{self as daemon_control, PeerLinkState, PokeOutcome};
use daemon_config::consts::PeppyDirs;
use peppylib::CoreNodePresenceMessenger;

use super::super::Command;
use crate::context::AppContext;
use crate::error::{Error, Result};

pub(super) struct FederateCommand {
    pub endpoint: federation::FederationPeer,
    pub peppy_dirs: Option<PeppyDirs>,
}

impl Command for FederateCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        let dirs = self.peppy_dirs.unwrap_or_default();
        let config =
            daemon_config::peppy_config::load_or_create(&dirs).map_err(Error::DaemonConfig)?;
        let read_timeout = super::managed_poke_read_timeout(&dirs, &config)?;
        // A running daemon's recorded mode is authoritative. If the on-disk
        // config was changed to external after this managed generation started,
        // keep using the conventional identity paths that the CLI can still
        // resolve rather than misclassifying the live daemon as external.
        let federation_config = config.zenoh.federation().cloned().unwrap_or_default();
        let identity = federation::resolve_identity_paths(&dirs, &federation_config)?;
        ensure_identity_exists(&identity)?;

        let before = crate::commands::block_on(snapshot_visible(ctx)).ok();

        let requested = self.endpoint;
        let endpoint = requested.endpoint().as_str().to_string();
        let registry_path = federation::registry_path(&dirs);
        let socket = daemon_control::federation_control_socket_path(&dirs);
        federation::with_registry(&registry_path, |registry| {
            let existing = registry
                .peers()
                .iter()
                .any(|peer| peer.endpoint() == requested.endpoint());
            if !existing {
                registry.insert(requested.clone())?;
                return Ok(());
            }
            // A failed probe deliberately keeps the durable entry. Permit
            // the exact command to retry only while cached daemon status
            // says that entry failed or has not reached applied state.
            let retryable = match daemon_control::query_status(&socket, super::STATUS_TIMEOUT) {
                daemon_control::QueryStatusOutcome::Status(status) => status
                    .peers
                    .iter()
                    .find(|peer| peer.endpoint == endpoint)
                    .is_none_or(|peer| peer.state != PeerLinkState::Verified),
                daemon_control::QueryStatusOutcome::DaemonError(message) => {
                    return Err(Error::ExecutionFailed(format!(
                        "the daemon could not report federation status: {message}. Restart \
                         the daemon after upgrading, then re-run `peppy federation federate \
                         {endpoint}`"
                    )));
                }
                daemon_control::QueryStatusOutcome::TimedOut => {
                    return Err(Error::ExecutionFailed(format!(
                        "could not determine whether endpoint {endpoint:?} is live because the \
                         daemon status request timed out; retry when the daemon responds"
                    )));
                }
                daemon_control::QueryStatusOutcome::DaemonNotRunning => false,
            };
            if !retryable {
                return Err(Error::ExecutionFailed(format!(
                    "endpoint {endpoint:?} is already federated"
                )));
            }
            Ok(())
        })?;

        let spinner = crate::terminal::spinner("Waiting for federation link to establish");
        let outcome = daemon_control::poke_refederate(&socket, read_timeout);
        if let Some(spinner) = spinner {
            spinner.finish_and_clear();
        }

        let backend_warning = match outcome {
            PokeOutcome::Applied(applied) => {
                validate_requested_peer(&endpoint, &applied)?;
                report_other_peer_errors(&endpoint, &applied);
                None
            }
            PokeOutcome::Unreachable { reason, applied } => {
                validate_requested_peer(&endpoint, &applied)?;
                report_other_peer_errors(&endpoint, &applied);
                Some(reason)
            }
            PokeOutcome::Pinned => {
                println!("{}", crate::commands::auth::PINNED_NOTE);
                return Ok(());
            }
            PokeOutcome::DaemonNotRunning => {
                println!(
                    "Federation with {endpoint} saved; the daemon will apply it on next start."
                );
                return Ok(());
            }
            PokeOutcome::DaemonError(reason) => {
                return Err(saved_error(&endpoint, &reason));
            }
            PokeOutcome::TimedOut => {
                return Err(saved_error(
                    &endpoint,
                    "the daemon did not apply federation within the timeout",
                ));
            }
            PokeOutcome::Restarting => {
                return Err(saved_error(
                    &endpoint,
                    "the daemon is restarting to apply an organization namespace change",
                ));
            }
        };
        if let Some(reason) = backend_warning {
            println!(
                "Note: the platform-backend federation did not validate ({reason}); the requested peer did."
            );
        }

        let discovered = match before {
            Some(before) => crate::commands::block_on(async {
                tokio::time::sleep(core_node::NAME_CLAIM_LINKED_SETTLE).await;
                let after = snapshot_visible(ctx).await?;
                Ok(new_core_nodes(&before, &after))
            })
            .unwrap_or_default(),
            None => Vec::new(),
        };

        if discovered.is_empty() {
            println!("Federated with {endpoint}; the peer core-node name was not discovered.");
        } else {
            println!(
                "Federated with {endpoint}; newly visible core nodes (not cached because the \
                 daemon cannot correlate them to this endpoint): {}.",
                discovered.join(", ")
            );
        }
        Ok(())
    }
}

fn validate_requested_peer(
    endpoint: &str,
    applied: &daemon_control::AppliedFederation,
) -> Result<()> {
    let own = applied
        .peers
        .iter()
        .find(|report| report.endpoint == endpoint)
        .ok_or_else(|| {
            saved_error(
                endpoint,
                "the daemon did not report the requested peer after applying",
            )
        })?;
    match &own.state {
        PeerLinkState::Verified => Ok(()),
        PeerLinkState::Unverified => Err(saved_error(
            endpoint,
            "the daemon did not verify the peer link",
        )),
        PeerLinkState::Error(reason) => Err(saved_error(endpoint, reason)),
    }
}

fn report_other_peer_errors(endpoint: &str, applied: &daemon_control::AppliedFederation) {
    for report in applied
        .peers
        .iter()
        .filter(|report| report.endpoint != endpoint)
    {
        if let PeerLinkState::Error(reason) = &report.state {
            println!(
                "Note: existing federation {} did not validate ({reason}).",
                report.endpoint
            );
        }
    }
}

fn ensure_identity_exists(identity: &federation::IdentityPaths) -> Result<()> {
    let missing = identity.missing_files();
    if missing.is_empty() {
        return Ok(());
    }
    Err(Error::ExecutionFailed(format!(
        "federation identity is incomplete; missing {}. Run `peppy federation ca init` and \
         `peppy federation ca issue`, or copy a fleet-issued ca.pem, cert.pem, and key.pem into \
         {}",
        missing
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        identity
            .cert
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .display()
    )))
}

fn saved_error(endpoint: &str, reason: &str) -> Error {
    Error::ExecutionFailed(format!(
        "federation with {endpoint} could not be established: {reason}. The federation was saved; \
         fix the problem and re-run `peppy federation federate {endpoint}`"
    ))
}

type PresenceKey = (String, String);

async fn snapshot_visible(ctx: &Arc<AppContext>) -> Result<BTreeSet<PresenceKey>> {
    let conn = ctx.connect_to_daemon().await?;
    let live = CoreNodePresenceMessenger::list_live(
        conn.messenger,
        None,
        CoreNodePresenceMessenger::LIST_TIMEOUT,
    )
    .await?;
    Ok(live
        .into_iter()
        .map(|presence| (presence.core_node, presence.instance_id))
        .collect())
}

fn new_core_nodes(before: &BTreeSet<PresenceKey>, after: &BTreeSet<PresenceKey>) -> Vec<String> {
    after
        .difference(before)
        .map(|(core_node, _)| core_node.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    fn peer(endpoint: &str) -> federation::FederationPeer {
        federation::FederationPeer::new(endpoint, None).unwrap()
    }

    #[test]
    fn presence_diff_deduplicates_instances_by_core_node_name() {
        let before = BTreeSet::from([("local".to_string(), "a".to_string())]);
        let after = BTreeSet::from([
            ("local".to_string(), "a".to_string()),
            ("peer".to_string(), "b".to_string()),
            ("peer".to_string(), "c".to_string()),
        ]);
        assert_eq!(new_core_nodes(&before, &after), vec!["peer"]);
    }

    #[test]
    fn requested_peer_probe_error_is_strict_and_registry_is_kept() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temp.path());
        let identity_dir = federation::federation_dir(&dirs);
        federation::ca_init(&identity_dir).unwrap();
        federation::issue(&identity_dir, &["peer.example".to_string()], &identity_dir).unwrap();

        let socket = daemon_control::federation_control_socket_path(&dirs);
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            assert_eq!(request.trim(), daemon_control::REFEDERATE_VERB);
            stream
                .write_all(
                    b"{\"status\":\"ok\",\"applied\":null,\"peers\":[{\"endpoint\":\"tls/peer.example:7449\",\"state\":{\"error\":\"UnknownIssuer\"}}]}\n",
                )
                .unwrap();
        });

        let ctx = Arc::new(AppContext::new(temp.path()));
        let error = FederateCommand {
            endpoint: peer("tls/peer.example:7449"),
            peppy_dirs: Some(dirs.clone()),
        }
        .execute(&ctx)
        .expect_err("the requested peer's failed probe must fail the command");
        server.join().unwrap();

        assert!(error.to_string().contains("UnknownIssuer"));
        let registry = federation::load(&federation::registry_path(&dirs)).unwrap();
        assert_eq!(registry.peers().len(), 1, "failed apply keeps saved state");

        std::fs::remove_file(&socket).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let retry_server = std::thread::spawn(move || {
            for expected in [daemon_control::STATUS_VERB, daemon_control::REFEDERATE_VERB] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut request)
                    .unwrap();
                assert_eq!(request.trim(), expected);
                let reply = if expected == daemon_control::STATUS_VERB {
                    b"{\"status\":\"federation_status\",\"backend\":null,\"peers\":[{\"endpoint\":\"tls/peer.example:7449\",\"state\":{\"error\":\"UnknownIssuer\"}}],\"listen_endpoint\":null,\"pinned\":false}\n".as_slice()
                } else {
                    b"{\"status\":\"ok\",\"applied\":null,\"peers\":[{\"endpoint\":\"tls/peer.example:7449\",\"state\":\"verified\"}]}\n".as_slice()
                };
                stream.write_all(reply).unwrap();
            }
        });
        FederateCommand {
            endpoint: peer("tls/peer.example:7449"),
            peppy_dirs: Some(dirs.clone()),
        }
        .execute(&ctx)
        .expect("re-running a saved failed federation retries the apply");
        retry_server.join().unwrap();
        let registry = federation::load(&federation::registry_path(&dirs)).unwrap();
        assert_eq!(registry.peers().len(), 1, "retry does not duplicate state");

        let duplicate = FederateCommand {
            endpoint: peer("tls/peer.example:7449"),
            peppy_dirs: Some(dirs),
        }
        .execute(&ctx)
        .expect_err("an entry without cached failed status remains a duplicate");
        assert!(duplicate.to_string().contains("already federated"));
    }

    #[test]
    fn saved_peer_retry_against_an_old_daemon_requests_restart_after_upgrade() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temp.path());
        let identity_dir = federation::federation_dir(&dirs);
        federation::ca_init(&identity_dir).unwrap();
        federation::issue(&identity_dir, &["peer.example".to_string()], &identity_dir).unwrap();

        let endpoint = "tls/peer.example:7449";
        let mut registry = federation::Federations::default();
        registry
            .insert(federation::FederationPeer::new(endpoint, None).unwrap())
            .unwrap();
        federation::save(&federation::registry_path(&dirs), &registry).unwrap();

        let socket = daemon_control::federation_control_socket_path(&dirs);
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            assert_eq!(request.trim(), daemon_control::STATUS_VERB);
            stream
                .write_all(b"{\"status\":\"error\",\"message\":\"unknown command\"}\n")
                .unwrap();
        });

        let error = FederateCommand {
            endpoint: peer(endpoint),
            peppy_dirs: Some(dirs),
        }
        .execute(&Arc::new(AppContext::new(temp.path())))
        .expect_err("an old daemon cannot safely classify the saved retry");
        server.join().unwrap();

        let message = error.to_string();
        assert!(message.contains("Restart the daemon after upgrading"));
        assert!(message.contains("peppy federation federate"));
        assert!(!message.contains("already federated"));
    }
}
