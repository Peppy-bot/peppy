//! Git utilities shared by the node command handlers: repo-path
//! sanitization, ref checkout, reading the commit a working tree sits on,
//! and a clone that honors a deadline on the network transfer. Remote
//! repositories are read over HTTPS or SSH; the SSH ones authenticate the
//! way `ssh` itself would — the ssh-agent's key, then the default identity
//! files — and have their host key checked against `~/.ssh/known_hosts`
//! (see [`install_credentials`]), so a private repository is a
//! `git@host:owner/repo.git` URL away rather than a local checkout this
//! machine must register instead.

use daemon_config::repository::GitCommit;
use git2::Repository;
use git2::build::{CheckoutBuilder, RepoBuilder};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

/// Throttle window between consecutive `transfer_progress` reports surfaced
/// to the user: fast enough to feel live, slow enough to avoid flooding
/// the feedback channel.
const PROGRESS_REPORT_INTERVAL: Duration = Duration::from_millis(500);

/// Returns `true` for URLs that dispatch through libgit2's local transport,
/// which rejects `depth(1)` shallow fetches with
/// "shallow fetch is not supported by the local transport".
fn is_local_url(repo_url: &str) -> bool {
    repo_url.starts_with('/') || repo_url.starts_with("file://")
}

/// Renders byte counts in the compact `KB`/`MB`/`GB` form shared by all of
/// peppy's progress lines (delegates to the one formatter in `node-stack`).
pub(crate) fn format_bytes(bytes: usize) -> String {
    node_stack::build_io::format_bytes(bytes as u64)
}

pub(crate) fn sanitize_repo_path(repo_path: &str) -> std::result::Result<PathBuf, String> {
    let trimmed = repo_path.trim().trim_start_matches(['/', '\\']);
    if trimmed.is_empty() {
        return Err("repo_path must not be empty".to_string());
    }
    let path = PathBuf::from(trimmed);
    for component in path.components() {
        match component {
            Component::ParentDir => {
                return Err("repo_path must not contain '..'".to_string());
            }
            Component::RootDir => {
                return Err("repo_path must be relative".to_string());
            }
            Component::Prefix(_) => {
                return Err("repo_path must not contain a drive or UNC prefix".to_string());
            }
            _ => {}
        }
    }
    Ok(path)
}

pub(crate) fn checkout_repo_ref(
    repo: &Repository,
    repo_ref: &str,
) -> std::result::Result<(), git2::Error> {
    let repo_ref = repo_ref.trim();
    if repo_ref.is_empty() {
        return Ok(());
    }
    let object = repo
        .revparse_single(repo_ref)
        .or_else(|_| repo.revparse_single(&format!("refs/tags/{repo_ref}")))
        .or_else(|_| repo.revparse_single(&format!("refs/heads/{repo_ref}")))
        .or_else(|_| repo.revparse_single(&format!("refs/remotes/origin/{repo_ref}")))?;
    let commit = object.peel_to_commit()?;
    repo.set_head_detached(commit.id())?;
    let mut checkout = CheckoutBuilder::new();
    checkout.force();
    repo.checkout_head(Some(&mut checkout))?;
    Ok(())
}

/// Clone `repo_url` into `dst` and check out `repo_ref` if set, emitting
/// throttled (~500ms) `transfer_progress` lines via `on_progress` so callers
/// can surface live byte/object counts instead of sitting silent for the
/// duration of the clone.
///
/// Each progress line is formatted as
/// `"Cloning {repo_url}: received {recv}/{total} objects ({bytes})"`.
///
/// `shallow` requests a `depth=1` fetch, but is silently downgraded to a
/// full clone when `repo_url` targets the local transport.
/// The commit `repo`'s working tree is sitting on.
///
/// The one place a `git2::Oid` becomes a [`GitCommit`], so a caller that
/// wants to know which bytes a checkout holds compares validated commits
/// rather than rendered strings.
pub(crate) fn head_commit(repo: &Repository) -> std::result::Result<GitCommit, String> {
    let head = repo
        .head()
        .map_err(|e| format!("the repository has no HEAD to read a commit from: {e}"))?;
    let commit = head
        .peel_to_commit()
        .map_err(|e| format!("the repository's HEAD does not name a commit: {e}"))?;
    GitCommit::parse(&commit.id().to_string())
        .map_err(|e| format!("the repository's HEAD is not a usable commit: {e}"))
}

/// Shallow-clones `repo_url` into `dst`, leaving the working tree wherever
/// the remote's HEAD points.
///
/// The one way peppy fetches a repository it reads, shared by `repo refresh`
/// and the commit-keyed checkout cache so the two can never disagree about
/// what cloning a repository brings. It brings every head the remote
/// publishes at depth 1: libgit2 clones with the standard
/// `refs/heads/*:refs/remotes/origin/*` refspec whatever branch is asked
/// for, so which ref a caller cares about changes nothing about the fetch.
///
/// Callers therefore position the working tree themselves, and the
/// positioning is the only thing that differs between them: refresh onto
/// the ref a repository is configured to follow, so the commit it pins is
/// that ref's tip; the checkout cache onto the commit an entry pins, which
/// is already here whenever it is a head's tip.
pub(crate) fn clone_repo_shallow(
    repo_url: &str,
    dst: &Path,
    on_progress: &mut dyn FnMut(&str),
) -> std::result::Result<Repository, String> {
    clone_with_progress(repo_url, None, dst, true, on_progress)
}

pub(crate) fn clone_with_progress(
    repo_url: &str,
    repo_ref: Option<&str>,
    dst: &Path,
    shallow: bool,
    on_progress: &mut dyn FnMut(&str),
) -> std::result::Result<Repository, String> {
    let mut builder = RepoBuilder::new();
    let mut fetch_opts = git2::FetchOptions::new();
    if shallow && !is_local_url(repo_url) {
        fetch_opts.depth(1);
    }
    fetch_opts.remote_callbacks(progress_callbacks(repo_url, "Cloning", on_progress));
    builder.fetch_options(fetch_opts);

    let repo = builder
        .clone(repo_url, dst)
        .map_err(|e| format!("Failed to clone {}: {}", repo_url, e))?;

    if let Some(r) = repo_ref {
        checkout_repo_ref(&repo, r)
            .map_err(|e| format!("Failed to checkout ref '{}': {}", r, e))?;
    }
    Ok(repo)
}

/// Fetch `refspec` on `remote`, emitting throttled (~500ms) progress lines
/// via `on_progress` formatted as
/// `"Fetching {repo_url}: received {recv}/{total} objects ({bytes})"`.
///
/// `shallow` is downgraded to non-shallow for local transports, mirroring
/// [`clone_with_progress`].
pub(crate) fn fetch_with_progress(
    remote: &mut git2::Remote<'_>,
    repo_url: &str,
    refspec: &str,
    shallow: bool,
    on_progress: &mut dyn FnMut(&str),
) -> std::result::Result<(), git2::Error> {
    let mut fetch_opts = git2::FetchOptions::new();
    if shallow && !is_local_url(repo_url) {
        fetch_opts.depth(1);
    }
    fetch_opts.remote_callbacks(progress_callbacks(repo_url, "Fetching", on_progress));
    remote.fetch(&[refspec], Some(&mut fetch_opts), None)
}

fn progress_callbacks<'cb>(
    repo_url: &str,
    verb: &'static str,
    on_progress: &'cb mut dyn FnMut(&str),
) -> git2::RemoteCallbacks<'cb> {
    let repo_url_owned = repo_url.to_string();
    let mut last_report = Instant::now()
        .checked_sub(PROGRESS_REPORT_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut callbacks = git2::RemoteCallbacks::new();
    install_credentials(&mut callbacks);
    callbacks.transfer_progress(move |progress| {
        if last_report.elapsed() >= PROGRESS_REPORT_INTERVAL {
            last_report = Instant::now();
            on_progress(&format!(
                "{verb} {repo_url}: received {recv}/{total} objects ({bytes})",
                repo_url = repo_url_owned,
                recv = progress.received_objects(),
                total = progress.total_objects(),
                bytes = format_bytes(progress.received_bytes()),
            ));
        }
        true
    });
    callbacks
}

/// The answer to libgit2's nth request for credentials on one connection.
#[derive(Debug, PartialEq, Eq)]
enum CredentialAnswer {
    /// The ssh-agent's key, authing as this user.
    AgentKey(String),
    /// A default identity file, authing as this user.
    KeyFile { user: String, key: PathBuf },
    /// Just a username, for the username-only probe libgit2 makes on an
    /// `ssh://` URL that carries no user.
    Username(String),
    /// Nothing left to offer, and why.
    Refuse(&'static str),
}

/// The default identity files OpenSSH itself tries, in the order a modern
/// host realistically holds them. The `*-sk` kinds are absent on purpose:
/// they live on FIDO tokens, which no unattended libssh2 client can tap.
const DEFAULT_SSH_IDENTITIES: [&str; 3] = ["id_ed25519", "id_ecdsa", "id_rsa"];

/// Resolves [`DEFAULT_SSH_IDENTITIES`] against `home`, skipping names that
/// exist as directories (an `~/.ssh/id_ed25519/` is not a key).
fn ssh_key_candidates(home: Option<&Path>) -> Vec<PathBuf> {
    let Some(home) = home else {
        return Vec::new();
    };
    DEFAULT_SSH_IDENTITIES
        .iter()
        .map(|name| home.join(".ssh").join(name))
        .filter(|path| path.is_file())
        .collect()
}

/// What peppy answers when a remote asks for credentials.
///
/// A public HTTPS repository never asks, so libgit2 never calls back and the
/// answer here is irrelevant to it. A private one asks for a username and
/// password peppy does not hold; refusing keeps that a named failure that
/// names the fix (credentials embedded in the URL, which libgit2 uses
/// without asking). SSH always authenticates the client, and the answer
/// mirrors what `ssh` itself would try: the ssh-agent's key for the URL's
/// user — `git` for the scp-style `git@host:owner/repo.git` URLs the hub
/// repositories use, and also for an `ssh://` URL that spells no user — and
/// then the default identity files under `~/.ssh`. Each candidate is offered
/// at most once: a repeated request means the remote refused it, and
/// replaying would loop libgit2's auth retry until the server hangs up.
///
/// The host side of SSH is not peppy's to answer: libgit2 checks the host
/// key against `~/.ssh/known_hosts` itself and refuses an unknown or
/// mismatched one before any credentials are requested.
fn credential_answer(
    username: Option<&str>,
    allowed: git2::CredentialType,
    attempt: usize,
    key_candidates: &[PathBuf],
) -> CredentialAnswer {
    let user = username.unwrap_or("git").to_owned();
    if allowed.contains(git2::CredentialType::SSH_KEY) {
        if attempt == 0 {
            return CredentialAnswer::AgentKey(user);
        }
        let key = key_candidates.get(attempt - 1).cloned();
        if let Some(key) = key {
            return CredentialAnswer::KeyFile { user, key };
        }
        return CredentialAnswer::Refuse(
            "the remote refused the ssh-agent's keys and the default identity files \
             (~/.ssh/id_ed25519, id_ecdsa, id_rsa); load a key the remote accepts into the \
             agent, or point the repository at an unencrypted default identity",
        );
    }
    if allowed.contains(git2::CredentialType::USERNAME)
        && !allowed
            .intersects(git2::CredentialType::SSH_KEY | git2::CredentialType::USER_PASS_PLAINTEXT)
    {
        return CredentialAnswer::Username(user);
    }
    CredentialAnswer::Refuse(
        "peppy authenticates git over ssh through the ssh-agent and the default identity \
         files, and offers no other credentials; embed them in the repository URL instead",
    )
}

/// Installs the credentials callback every peppy fetch of a remote goes
/// through, so no two call sites can drift apart on what authentication a
/// clone or fetch offers. See [`credential_answer`] for what is offered.
///
/// A candidate that fails to load — an agent with no keys, an encrypted
/// identity file — advances to the next within the same request rather than
/// failing the connection, exactly as `ssh` moves on from an identity it
/// cannot use.
pub(crate) fn install_credentials(callbacks: &mut git2::RemoteCallbacks<'_>) {
    let mut attempt = 0usize;
    let key_candidates = ssh_key_candidates(dirs::home_dir().as_deref());
    callbacks.credentials(move |_url, username, allowed| {
        loop {
            match credential_answer(username, allowed, attempt, &key_candidates) {
                CredentialAnswer::AgentKey(user) => {
                    attempt += 1;
                    if let Ok(cred) = git2::Cred::ssh_key_from_agent(&user) {
                        return Ok(cred);
                    }
                }
                CredentialAnswer::KeyFile { user, key } => {
                    attempt += 1;
                    if let Ok(cred) = git2::Cred::ssh_key(user.as_str(), None, &key, None) {
                        return Ok(cred);
                    }
                }
                CredentialAnswer::Username(user) => return git2::Cred::username(&user),
                CredentialAnswer::Refuse(message) => return Err(git2::Error::from_str(message)),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ssh_only() -> git2::CredentialType {
        git2::CredentialType::SSH_KEY
    }

    #[test]
    fn the_first_ssh_request_gets_the_agent_key_for_the_url_user() {
        assert_eq!(
            credential_answer(Some("git"), ssh_only(), 0, &[]),
            CredentialAnswer::AgentKey("git".to_owned())
        );
    }

    #[test]
    fn ssh_requests_default_the_user_to_git() {
        assert_eq!(
            credential_answer(None, ssh_only(), 0, &[]),
            CredentialAnswer::AgentKey("git".to_owned())
        );
    }

    #[test]
    fn later_ssh_requests_walk_the_default_identity_files_in_order() {
        let keys = vec![
            PathBuf::from("/home/peppy/.ssh/id_ed25519"),
            PathBuf::from("/home/peppy/.ssh/id_ecdsa"),
        ];
        assert_eq!(
            credential_answer(Some("git"), ssh_only(), 1, &keys),
            CredentialAnswer::KeyFile {
                user: "git".to_owned(),
                key: PathBuf::from("/home/peppy/.ssh/id_ed25519")
            }
        );
        assert_eq!(
            credential_answer(Some("git"), ssh_only(), 2, &keys),
            CredentialAnswer::KeyFile {
                user: "git".to_owned(),
                key: PathBuf::from("/home/peppy/.ssh/id_ecdsa")
            }
        );
    }

    #[test]
    fn exhausting_the_candidates_refuses_instead_of_replaying_one() {
        assert_eq!(
            credential_answer(Some("git"), ssh_only(), 1, &[]),
            CredentialAnswer::Refuse(
                "the remote refused the ssh-agent's keys and the default identity files \
                 (~/.ssh/id_ed25519, id_ecdsa, id_rsa); load a key the remote accepts into \
                 the agent, or point the repository at an unencrypted default identity"
            )
        );
    }

    #[test]
    fn username_only_requests_answer_the_user_git_would_auth_as() {
        let username_only = git2::CredentialType::USERNAME;
        assert_eq!(
            credential_answer(Some("peppy"), username_only, 0, &[]),
            CredentialAnswer::Username("peppy".to_owned())
        );
        assert_eq!(
            credential_answer(None, username_only, 0, &[]),
            CredentialAnswer::Username("git".to_owned())
        );
    }

    #[test]
    fn password_requests_refuse_with_the_embedded_url_hint() {
        assert_eq!(
            credential_answer(
                Some("peppy"),
                git2::CredentialType::USER_PASS_PLAINTEXT,
                0,
                &[]
            ),
            CredentialAnswer::Refuse(
                "peppy authenticates git over ssh through the ssh-agent and the default \
                 identity files, and offers no other credentials; embed them in the \
                 repository URL instead"
            )
        );
    }

    #[test]
    fn key_candidates_list_the_default_identities_that_exist_as_files() {
        let tmp = tempfile::tempdir().unwrap();
        let ssh_dir = tmp.path().join(".ssh");
        std::fs::create_dir_all(&ssh_dir).unwrap();
        std::fs::write(ssh_dir.join("id_rsa"), b"key").unwrap();
        std::fs::create_dir_all(ssh_dir.join("id_ed25519")).unwrap();

        assert_eq!(
            ssh_key_candidates(Some(tmp.path())),
            vec![ssh_dir.join("id_rsa")]
        );
        assert!(ssh_key_candidates(None).is_empty());
    }
}
