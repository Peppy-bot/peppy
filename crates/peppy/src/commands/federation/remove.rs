use std::sync::Arc;

use daemon::control::{self as daemon_control, PeerLinkState, PokeOutcome};
use daemon_config::consts::PeppyDirs;

use super::super::Command;
use crate::context::AppContext;
use crate::error::{Error, Result};

pub(super) struct RemoveCommand {
    pub target: String,
    pub yes: bool,
    pub peppy_dirs: Option<PeppyDirs>,
}

enum RegistryRemoval {
    Removed(String),
    ExactEndpointAlreadyAbsent { endpoint: String, error: Error },
}

impl Command for RemoveCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        let dirs = self.peppy_dirs.unwrap_or_default();
        if self.target == federation::RESERVED_BACKEND_NAME {
            if !confirm_platform_backend_removal(self.yes)? {
                println!("Removal aborted.");
                return Ok(());
            }
            return crate::commands::auth::logout::LogoutCommand {
                api_url: None,
                yes: true,
                peppy_dirs: Some(dirs),
            }
            .execute(ctx);
        }

        let config =
            daemon_config::peppy_config::load_or_create(&dirs).map_err(Error::DaemonConfig)?;
        let read_timeout = super::managed_poke_read_timeout(&dirs, &config)?;

        let socket = daemon_control::federation_control_socket_path(&dirs);
        let registry_path = federation::registry_path(&dirs);
        let removal = federation::with_registry(&registry_path, |registry| {
            remove_registry_target(registry, &self.target)
        });
        let endpoint = match removal {
            Ok(RegistryRemoval::Removed(endpoint)) => endpoint,
            Ok(RegistryRemoval::ExactEndpointAlreadyAbsent { endpoint, error }) => {
                if matches!(
                    daemon_control::query_status(&socket, super::STATUS_TIMEOUT),
                    daemon_control::QueryStatusOutcome::Status(status)
                        if status.peers.iter().any(|peer| peer.endpoint == endpoint)
                ) {
                    // The durable entry is already gone but the previous apply
                    // failed. An exact-endpoint remove is an explicit retry.
                    endpoint
                } else {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        };

        match daemon_control::poke_refederate(&socket, read_timeout) {
            PokeOutcome::Applied(applied) => {
                let still_applied = applied
                    .peers
                    .iter()
                    .any(|report| report.endpoint == endpoint);
                if still_applied {
                    print_removal_pending(&endpoint, "the daemon still reports it");
                }
                for report in applied.peers {
                    if let PeerLinkState::Error(reason) = report.state {
                        println!(
                            "Note: remaining federation {} did not validate ({reason}).",
                            report.endpoint
                        );
                    }
                }
                if !still_applied {
                    println!("Removed federation with {endpoint}.");
                }
            }
            PokeOutcome::Pinned => {
                println!("{}", crate::commands::auth::PINNED_NOTE);
                println!("Removed {endpoint} from the saved federation registry.");
            }
            PokeOutcome::DaemonNotRunning => {
                println!("Removed {endpoint}; the daemon will apply the change on next start.")
            }
            PokeOutcome::Unreachable { reason, applied } => {
                if applied
                    .peers
                    .iter()
                    .any(|report| report.endpoint == endpoint)
                {
                    print_removal_pending(&endpoint, &reason);
                } else {
                    println!(
                        "Note: platform-backend did not validate ({reason}); the peer removal was applied."
                    );
                    println!("Removed federation with {endpoint}.");
                }
            }
            PokeOutcome::DaemonError(reason) => {
                println!(
                    "Removed {endpoint} from saved state; the daemon could not apply it now \
                     ({reason}) and will retry. Until then, `peppy federation list` shows it as \
                     removal pending."
                )
            }
            PokeOutcome::TimedOut => println!(
                "Removed {endpoint} from saved state; the federation poke timed out and the \
                 daemon will finish or retry it. Until then, `peppy federation list` shows it as \
                 removal pending."
            ),
            PokeOutcome::Restarting => println!(
                "Removed {endpoint}; the daemon is restarting before it applies the saved state."
            ),
        }
        Ok(())
    }
}

fn remove_registry_target(
    registry: &mut federation::Federations,
    target: &str,
) -> Result<RegistryRemoval> {
    if federation::FederationPeer::new(target.to_string(), None).is_ok()
        && !registry
            .peers()
            .iter()
            .any(|peer| peer.endpoint().as_str() == target)
    {
        let error = resolve_target(registry, target)
            .expect_err("an absent exact endpoint cannot resolve from the registry");
        return Ok(RegistryRemoval::ExactEndpointAlreadyAbsent {
            endpoint: target.to_string(),
            error,
        });
    }
    let endpoint = resolve_target(registry, target)?;
    registry.remove(&endpoint)?;
    Ok(RegistryRemoval::Removed(endpoint))
}

fn print_removal_pending(endpoint: &str, reason: &str) {
    println!(
        "removed {endpoint} from saved state, but the live link remains ({reason}). The daemon \
         will retry in the background; re-run `peppy federation remove {endpoint}` to retry now"
    );
}

fn confirm_platform_backend_removal(yes: bool) -> Result<bool> {
    use std::io::{IsTerminal, Write};

    eprintln!(
        "Removing platform-backend is equivalent to `peppy auth logout`. It signs this machine \
         out and may restart the daemon under the local namespace, wiping its running node stack."
    );
    let stdin_is_terminal = std::io::stdin().is_terminal();
    require_confirmation_channel(yes, stdin_is_terminal)?;
    if yes {
        return Ok(true);
    }
    eprint!("Continue? [y/N] ");
    std::io::stderr().flush().ok();
    crate::commands::confirm::read_yes_no(None)
}

fn require_confirmation_channel(yes: bool, stdin_is_terminal: bool) -> Result<()> {
    if !yes && !stdin_is_terminal {
        return Err(Error::ExecutionFailed(
            "cannot confirm removal of platform-backend from non-interactive input; re-run with \
             `--yes` to sign out and allow the daemon restart"
                .to_string(),
        ));
    }
    Ok(())
}

fn resolve_target(registry: &federation::Federations, target: &str) -> Result<String> {
    if registry
        .peers()
        .iter()
        .any(|peer| peer.endpoint().as_str() == target)
    {
        return Ok(target.to_string());
    }

    let matches: Vec<&federation::FederationPeer> = registry
        .peers()
        .iter()
        .filter(|peer| peer.core_node() == Some(target))
        .collect();
    match matches.as_slice() {
        [peer] => Ok(peer.endpoint().to_string()),
        [] => {
            let candidates = candidates(registry);
            Err(Error::ExecutionFailed(format!(
                "unknown federation target `{target}`. Available targets: {candidates}. Use the \
                 exact tls/<host>:<port> endpoint when a core-node name is unavailable"
            )))
        }
        many => Err(Error::ExecutionFailed(format!(
            "core-node name `{target}` matches multiple federation endpoints: {}. Remove one by \
             its exact endpoint",
            many.iter()
                .map(|peer| peer.endpoint().as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn candidates(registry: &federation::Federations) -> String {
    if registry.peers().is_empty() {
        return "(none)".to_string();
    }
    registry
        .peers()
        .iter()
        .map(|peer| match peer.core_node() {
            Some(name) => format!("{name} ({})", peer.endpoint()),
            None => peer.endpoint().to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    fn registry(peers: &[(&str, Option<&str>)]) -> federation::Federations {
        let mut registry = federation::Federations::default();
        for (endpoint, core_node) in peers {
            registry
                .insert(
                    federation::FederationPeer::new(
                        (*endpoint).to_string(),
                        core_node.map(str::to_string),
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        registry
    }

    #[test]
    fn resolves_exact_endpoint_before_display_name() {
        let registry = registry(&[("tls/peer-a:7449", Some("robot-a"))]);
        assert_eq!(
            resolve_target(&registry, "tls/peer-a:7449").unwrap(),
            "tls/peer-a:7449"
        );
    }

    #[test]
    fn resolves_a_unique_cached_name() {
        let registry = registry(&[("tls/peer-a:7449", Some("robot-a"))]);
        assert_eq!(
            resolve_target(&registry, "robot-a").unwrap(),
            "tls/peer-a:7449"
        );
    }

    #[test]
    fn rejects_ambiguous_and_unknown_names_with_candidates() {
        let registry = registry(&[
            ("tls/peer-a:7449", Some("robot")),
            ("tls/peer-b:7449", Some("robot")),
        ]);
        let ambiguous = resolve_target(&registry, "robot").unwrap_err().to_string();
        assert!(ambiguous.contains("tls/peer-a:7449"));
        assert!(ambiguous.contains("tls/peer-b:7449"));
        let unknown = resolve_target(&registry, "missing")
            .unwrap_err()
            .to_string();
        assert!(unknown.contains("Available targets"));
    }

    #[test]
    fn only_an_absent_exact_endpoint_gets_the_retry_outcome() {
        let mut saved = registry(&[("tls/peer-a:7449", Some("robot-a"))]);
        assert!(matches!(
            remove_registry_target(&mut saved, "tls/missing:7449").unwrap(),
            RegistryRemoval::ExactEndpointAlreadyAbsent { endpoint, .. }
                if endpoint == "tls/missing:7449"
        ));

        let error = remove_registry_target(&mut saved, "missing")
            .err()
            .expect("a missing display name must remain an error");
        assert!(error.to_string().contains("unknown federation target"));

        assert!(matches!(
            remove_registry_target(&mut saved, "robot-a").unwrap(),
            RegistryRemoval::Removed(endpoint) if endpoint == "tls/peer-a:7449"
        ));
    }

    #[test]
    fn non_interactive_backend_removal_requires_yes() {
        let error = require_confirmation_channel(false, false)
            .expect_err("non-interactive removal without --yes must be rejected");
        assert!(error.to_string().contains("--yes"));
        require_confirmation_channel(true, false)
            .expect("--yes explicitly authorizes non-interactive removal");
        require_confirmation_channel(false, true)
            .expect("an interactive terminal can present the confirmation prompt");
    }

    #[test]
    fn peer_removal_is_saved_when_the_live_link_is_still_applied() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temporary.path());
        let endpoint = "tls/peer-a.example:7449";
        let mut saved = federation::Federations::default();
        saved
            .insert(federation::FederationPeer::new(endpoint, Some("peer-a".into())).unwrap())
            .unwrap();
        federation::save(&federation::registry_path(&dirs), &saved).unwrap();

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
                    b"{\"status\":\"ok\",\"applied\":null,\"peers\":[{\"endpoint\":\"tls/peer-a.example:7449\",\"state\":\"verified\"}]}\n",
                )
                .unwrap();
        });

        RemoveCommand {
            target: endpoint.to_string(),
            yes: false,
            peppy_dirs: Some(dirs.clone()),
        }
        .execute(&Arc::new(AppContext::new(temporary.path())))
        .expect("durable removal is best effort even while the old link remains live");
        server.join().unwrap();

        let saved = federation::load(&federation::registry_path(&dirs)).unwrap();
        assert!(saved.peers().is_empty());
    }

    #[test]
    fn absent_exact_endpoint_retries_only_a_reported_live_removal() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temporary.path());
        let endpoint = "tls/peer-a.example:7449";
        let socket = daemon_control::federation_control_socket_path(&dirs);
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            for expected in [daemon_control::STATUS_VERB, daemon_control::REFEDERATE_VERB] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut request)
                    .unwrap();
                assert_eq!(request.trim(), expected);
                let response = if expected == daemon_control::STATUS_VERB {
                    b"{\"status\":\"federation_status\",\"backend\":null,\"peers\":[{\"endpoint\":\"tls/peer-a.example:7449\",\"state\":\"verified\"}],\"listen_endpoint\":null,\"pinned\":false}\n".as_slice()
                } else {
                    b"{\"status\":\"ok\",\"applied\":null,\"peers\":[]}\n".as_slice()
                };
                stream.write_all(response).unwrap();
            }
        });

        RemoveCommand {
            target: endpoint.to_string(),
            yes: false,
            peppy_dirs: Some(dirs),
        }
        .execute(&Arc::new(AppContext::new(temporary.path())))
        .expect("an already-saved exact removal may retry a still-live link");
        server.join().unwrap();
    }

    #[test]
    fn corrupt_registry_error_does_not_enter_the_live_retry_fallback() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temporary.path());
        let registry_path = federation::registry_path(&dirs);
        std::fs::create_dir_all(registry_path.parent().unwrap()).unwrap();
        std::fs::write(&registry_path, "not a registry").unwrap();

        let socket = daemon_control::federation_control_socket_path(&dirs);
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();

        let error = RemoveCommand {
            target: "tls/peer-a.example:7449".to_string(),
            yes: false,
            peppy_dirs: Some(dirs),
        }
        .execute(&Arc::new(AppContext::new(temporary.path())))
        .expect_err("registry corruption must be propagated");
        assert!(error.to_string().contains("failed to parse"));

        listener.set_nonblocking(true).unwrap();
        assert!(matches!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        ));
    }
}
