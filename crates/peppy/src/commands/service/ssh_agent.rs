//! Binds the daemon process to the ssh agent `ssh` selects for hosts in
//! general, so git over ssh inside the daemon reaches the agent `ssh` would.
//! libssh2 connects to the agent `SSH_AUTH_SOCK` names and reads no ssh
//! configuration, so an `IdentityAgent` directive in `~/.ssh/config` (how
//! 1Password, Secretive and gpg-agent are wired in) only reaches it through
//! that variable. The binding happens once, before any other thread exists,
//! and holds for the daemon's lifetime; a host whose configuration selects a
//! different agent is refused by name when it is cloned.

use std::env;
use std::path::PathBuf;

use core_node::{IdentityAgent, SshTarget, resolve_host_config};
use tracing::{info, warn};

/// The environment variable libssh2 reads the agent socket from.
const SSH_AUTH_SOCK: &str = "SSH_AUTH_SOCK";

/// Points `SSH_AUTH_SOCK` at the agent `~/.ssh/config` selects for a host
/// with no host-specific configuration, when it selects one. Without `ssh`
/// on the PATH, or without an `IdentityAgent` directive that applies, the
/// inherited environment stands. A configuration `ssh -G` rejects is
/// reported and the inherited environment stands too, since a daemon that
/// cannot clone over ssh still serves everything else.
// This function and `install::ensure_systemd_user_env` are the two `unsafe`
// allowances in the crate: `env::set_var` has no safe equivalent in edition
// 2024, and libssh2 reads the socket path from the process environment,
// giving no hook to pass it explicitly. The mutation is sound here because
// it runs during synchronous, single-threaded CLI startup, before the daemon
// runtime or any other thread exists (see the SAFETY note on the call). The
// rest of the crate is `#![deny(unsafe_code)]`.
#[allow(unsafe_code)]
pub(super) fn bind_ssh_agent_from_config() {
    let selected = match resolve_host_config(&SshTarget::without_host_specific_configuration()) {
        Ok(Some(config)) => config.identity_agent,
        Ok(None) => return,
        Err(message) => {
            warn!("{message}; git over ssh uses the agent SSH_AUTH_SOCK names");
            return;
        }
    };
    let IdentityAgent::Socket(socket) = selected else {
        return;
    };
    let bound = env::var_os(SSH_AUTH_SOCK)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    if bound.as_ref() != Some(&socket) {
        // SAFETY: called during single-threaded CLI startup, before the
        // daemon runtime and before any other thread exists.
        unsafe {
            env::set_var(SSH_AUTH_SOCK, &socket);
        }
    }
    info!(
        "git over ssh uses the agent at {} (IdentityAgent in ~/.ssh/config)",
        socket.display()
    );
}
