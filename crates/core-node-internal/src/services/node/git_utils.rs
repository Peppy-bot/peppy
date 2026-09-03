//! Git utilities shared by the node command handlers: repo-path
//! sanitization, ref checkout, reading the commit a working tree sits on,
//! and a clone that honors a deadline on the network transfer. Remote
//! repositories are read over HTTPS or SSH; the SSH ones authenticate the
//! way `ssh` itself would for the host, with the agent and identity files
//! `~/.ssh/config` selects, and have their host key checked against
//! `~/.ssh/known_hosts` (see [`install_credentials`]), so a private
//! repository is a `git@host:owner/repo.git` URL away rather than a local
//! checkout this machine must register instead.

use crate::ssh_config::{IdentityAgent, SshHostConfig, SshTarget, resolve_host_config};
use daemon_config::repository::GitCommit;
use git2::Repository;
use git2::build::{CheckoutBuilder, RepoBuilder};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::debug;

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
    /// The ssh agent's keys, authing as this user.
    AgentKey(String),
    /// An identity file, authing as this user.
    KeyFile { user: String, key: PathBuf },
    /// Just a username, for the username-only probe libgit2 makes on an
    /// `ssh://` URL that carries no user.
    Username(String),
    /// Nothing left to offer, and why.
    Refuse(String),
}

/// The identity files `ssh` itself tries when its configuration names none,
/// in the order a modern host realistically holds them. Offered only when
/// no `ssh` is installed to resolve the configured list; the `*-sk` kinds
/// are absent on purpose: they live on FIDO tokens, which no unattended
/// libssh2 client can tap.
const DEFAULT_SSH_IDENTITIES: [&str; 3] = ["id_ed25519", "id_ecdsa", "id_rsa"];

/// Resolves [`DEFAULT_SSH_IDENTITIES`] against `home`.
fn default_identity_files(home: Option<&Path>) -> Vec<PathBuf> {
    let Some(home) = home else {
        return Vec::new();
    };
    DEFAULT_SSH_IDENTITIES
        .iter()
        .map(|name| home.join(".ssh").join(name))
        .collect()
}

/// The agent whose keys open one ssh connection, or why none does.
#[derive(Debug, PartialEq, Eq)]
enum OfferedAgent {
    /// The agent at this socket, which `SSH_AUTH_SOCK` names in this process.
    Socket(PathBuf),
    /// `SSH_AUTH_SOCK` is unset and the host's configuration names no agent.
    Unset,
    /// The host's configuration disables the agent (`IdentityAgent none`).
    Disabled,
}

/// The credentials peppy offers on one ssh connection, in order: the
/// agent's keys, then each identity file. Built once per connection from
/// what `ssh` selects for the host.
#[derive(Debug, PartialEq, Eq)]
struct SshCredentialPlan {
    user: String,
    host: String,
    agent: OfferedAgent,
    /// The identity files that exist, offered after the agent.
    key_files: Vec<PathBuf>,
    /// The identity files the configuration names that are not files on
    /// disk, named in the refusal so a typo in `~/.ssh/config` is visible.
    absent_key_files: Vec<PathBuf>,
}

impl SshCredentialPlan {
    /// The credentials in the order they are offered.
    fn offers(&self) -> impl Iterator<Item = CredentialAnswer> + '_ {
        let agent = matches!(self.agent, OfferedAgent::Socket(_))
            .then(|| CredentialAnswer::AgentKey(self.user.clone()));
        agent
            .into_iter()
            .chain(self.key_files.iter().map(|key| CredentialAnswer::KeyFile {
                user: self.user.clone(),
                key: key.clone(),
            }))
    }

    /// The answer to the nth credential request: each offer exactly once,
    /// then a refusal naming everything the connection had.
    fn answer(&self, attempt: usize) -> CredentialAnswer {
        self.offers()
            .nth(attempt)
            .unwrap_or_else(|| CredentialAnswer::Refuse(self.refusal()))
    }

    fn refusal(&self) -> String {
        let outcome = if self.offers().next().is_some() {
            "the remote refused every credential peppy holds for"
        } else {
            "peppy holds no credential for"
        };
        format!(
            "{outcome} {}@{}: {}, and {}; load a key the remote accepts into the agent, or keep \
             an unencrypted identity file at one of those paths",
            self.user,
            self.host,
            self.agent_description(),
            self.identity_files_description()
        )
    }

    fn agent_description(&self) -> String {
        match &self.agent {
            OfferedAgent::Socket(socket) if socket.exists() => {
                format!("the keys in the ssh agent at {}", socket.display())
            }
            OfferedAgent::Socket(socket) => format!(
                "the ssh agent at {}, whose socket does not exist",
                socket.display()
            ),
            OfferedAgent::Unset => "no ssh agent (SSH_AUTH_SOCK is unset)".to_owned(),
            OfferedAgent::Disabled => "no ssh agent (IdentityAgent none)".to_owned(),
        }
    }

    fn identity_files_description(&self) -> String {
        let absent = || {
            format!(
                "{} do not exist as files",
                list_paths(&self.absent_key_files)
            )
        };
        match (self.key_files.is_empty(), self.absent_key_files.is_empty()) {
            (false, true) => format!("the identity files {}", list_paths(&self.key_files)),
            (false, false) => format!(
                "the identity files {} ({})",
                list_paths(&self.key_files),
                absent()
            ),
            (true, false) => format!("no identity file ({})", absent()),
            (true, true) => "no identity file (the configuration names none)".to_owned(),
        }
    }
}

fn list_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Builds the plan for one connection from what `ssh` selects for the
/// host. `host_config` is `None` when no `ssh` is installed to ask, in
/// which case the agent `SSH_AUTH_SOCK` names and the default identity
/// files stand in. `process_agent` is the socket `SSH_AUTH_SOCK` names in
/// this process, the only agent libssh2 can reach; a host whose
/// configuration selects a different one is refused by name, since the
/// daemon binds its agent once at startup.
fn plan_ssh_credentials(
    user: String,
    target: &SshTarget,
    host_config: Option<SshHostConfig>,
    process_agent: Option<PathBuf>,
    default_identity_files: Vec<PathBuf>,
) -> Result<SshCredentialPlan, String> {
    let process_agent_offer = || match process_agent.clone() {
        Some(socket) => OfferedAgent::Socket(socket),
        None => OfferedAgent::Unset,
    };
    let (agent, identity_files) = match host_config {
        None => (process_agent_offer(), default_identity_files),
        Some(config) => {
            let agent = match config.identity_agent {
                IdentityAgent::FromEnvironment => process_agent_offer(),
                IdentityAgent::Disabled => OfferedAgent::Disabled,
                IdentityAgent::Socket(selected) if process_agent.as_ref() == Some(&selected) => {
                    OfferedAgent::Socket(selected)
                }
                IdentityAgent::Socket(selected) => {
                    return Err(agent_mismatch(target, &selected, process_agent.as_deref()));
                }
            };
            (agent, config.identity_files)
        }
    };
    let (key_files, absent_key_files) = identity_files.into_iter().partition(|path| path.is_file());
    Ok(SshCredentialPlan {
        user,
        host: target.host.clone(),
        agent,
        key_files,
        absent_key_files,
    })
}

fn agent_mismatch(target: &SshTarget, selected: &Path, bound: Option<&Path>) -> String {
    let bound = match bound {
        Some(bound) => format!("the agent at {}", bound.display()),
        None => "no agent (SSH_AUTH_SOCK is unset)".to_owned(),
    };
    format!(
        "~/.ssh/config selects the ssh agent at {} for {} (IdentityAgent), but this daemon is \
         bound to {bound} for its lifetime: when it starts it binds the agent ssh selects for a \
         host with no host-specific configuration. Give every host that IdentityAgent (a \
         `Host *` block) and restart the daemon",
        selected.display(),
        target.host
    )
}

/// The plan for the connection libgit2 opened to `url`, authing as `user`.
fn plan_for_url(url: &str, user: String) -> Result<SshCredentialPlan, String> {
    let target = SshTarget::from_git_url(url)
        .ok_or_else(|| format!("{url} is not an ssh URL peppy can read a host from"))?;
    let host_config = resolve_host_config(&target)?;
    let process_agent = std::env::var_os("SSH_AUTH_SOCK")
        .filter(|socket| !socket.is_empty())
        .map(PathBuf::from);
    let plan = plan_ssh_credentials(
        user,
        &target,
        host_config,
        process_agent,
        default_identity_files(dirs::home_dir().as_deref()),
    )?;
    debug!(
        "git over ssh to {}@{}: offering {}, then {}",
        plan.user,
        plan.host,
        plan.agent_description(),
        plan.identity_files_description()
    );
    Ok(plan)
}

/// What peppy answers when a remote asks for something other than an ssh
/// key. A public HTTPS repository never asks, so libgit2 never calls back
/// and the answer here is irrelevant to it. A private one asks for a
/// username and password peppy does not hold; refusing keeps that a named
/// failure that names the fix (credentials embedded in the URL, which
/// libgit2 uses without asking).
fn non_ssh_answer(username: Option<&str>, allowed: git2::CredentialType) -> CredentialAnswer {
    if allowed.contains(git2::CredentialType::USERNAME)
        && !allowed.intersects(git2::CredentialType::USER_PASS_PLAINTEXT)
    {
        return CredentialAnswer::Username(username.unwrap_or("git").to_owned());
    }
    CredentialAnswer::Refuse(
        "peppy authenticates git over ssh through the ssh agent and identity files, and offers \
         no other credentials; embed them in the repository URL instead"
            .to_owned(),
    )
}

/// Installs the credentials callback every peppy fetch of a remote goes
/// through, so no two call sites can drift apart on what authentication a
/// clone or fetch offers.
///
/// SSH always authenticates the client, and the answer mirrors what `ssh`
/// itself would try for the URL's host: the keys of the agent
/// `~/.ssh/config` selects for it (`IdentityAgent`, else the one
/// `SSH_AUTH_SOCK` names), then its `IdentityFile` list, each as the URL's
/// user (`git` when the URL names none). See [`plan_ssh_credentials`] for
/// how the plan is built and [`SshCredentialPlan::answer`] for the order.
/// Each candidate is offered at most once: a repeated request means the
/// remote refused it, and replaying would loop libgit2's auth retry until
/// the server hangs up. A candidate that fails to load (an encrypted
/// identity file) advances to the next within the same request rather than
/// failing the connection, exactly as `ssh` moves on from an identity it
/// cannot use.
///
/// The host side of SSH is not peppy's to answer: libgit2 checks the host
/// key against `~/.ssh/known_hosts` itself and refuses an unknown or
/// mismatched one before any credentials are requested.
pub(crate) fn install_credentials(callbacks: &mut git2::RemoteCallbacks<'_>) {
    let mut attempt = 0usize;
    let mut plan: Option<Result<SshCredentialPlan, String>> = None;
    callbacks.credentials(move |url, username, allowed| {
        loop {
            let answer = if allowed.contains(git2::CredentialType::SSH_KEY) {
                let user = username.unwrap_or("git").to_owned();
                match plan.get_or_insert_with(|| plan_for_url(url, user)) {
                    Ok(plan) => plan.answer(attempt),
                    Err(message) => CredentialAnswer::Refuse(message.clone()),
                }
            } else {
                non_ssh_answer(username, allowed)
            };
            match answer {
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
                CredentialAnswer::Refuse(message) => return Err(git2::Error::from_str(&message)),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn github() -> SshTarget {
        SshTarget {
            user: Some("git".to_owned()),
            host: "github.com".to_owned(),
            port: None,
        }
    }

    fn agent_key() -> CredentialAnswer {
        CredentialAnswer::AgentKey("git".to_owned())
    }

    fn key_file(key: &Path) -> CredentialAnswer {
        CredentialAnswer::KeyFile {
            user: "git".to_owned(),
            key: key.to_path_buf(),
        }
    }

    /// A `.ssh` directory holding `existing` as key files, plus a directory
    /// at `id_ed25519`, which is not a key.
    fn ssh_dir(existing: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let ssh_dir = tmp.path().join(".ssh");
        std::fs::create_dir_all(&ssh_dir).unwrap();
        for name in existing {
            std::fs::write(ssh_dir.join(name), b"key").unwrap();
        }
        std::fs::create_dir_all(ssh_dir.join("id_ed25519")).unwrap();
        (tmp, ssh_dir)
    }

    fn plan_without_ssh(
        process_agent: Option<PathBuf>,
        home: &Path,
    ) -> Result<SshCredentialPlan, String> {
        plan_ssh_credentials(
            "git".to_owned(),
            &github(),
            None,
            process_agent,
            default_identity_files(Some(home)),
        )
    }

    fn plan_with_config(
        identity_agent: IdentityAgent,
        identity_files: Vec<PathBuf>,
        process_agent: Option<PathBuf>,
    ) -> Result<SshCredentialPlan, String> {
        plan_ssh_credentials(
            "git".to_owned(),
            &github(),
            Some(SshHostConfig {
                identity_agent,
                identity_files,
            }),
            process_agent,
            vec![PathBuf::from("/never/offered")],
        )
    }

    #[test]
    fn without_ssh_the_environment_agent_leads_and_the_default_identities_follow() {
        let (tmp, ssh_dir) = ssh_dir(&["id_rsa"]);
        let socket = tmp.path().join("agent.sock");
        let plan = plan_without_ssh(Some(socket.clone()), tmp.path()).unwrap();

        assert_eq!(plan.agent, OfferedAgent::Socket(socket));
        assert_eq!(plan.key_files, vec![ssh_dir.join("id_rsa")]);
        assert_eq!(
            plan.absent_key_files,
            vec![ssh_dir.join("id_ed25519"), ssh_dir.join("id_ecdsa")]
        );
        assert_eq!(plan.answer(0), agent_key());
        assert_eq!(plan.answer(1), key_file(&ssh_dir.join("id_rsa")));
        assert!(matches!(plan.answer(2), CredentialAnswer::Refuse(_)));
    }

    #[test]
    fn the_configured_identity_files_replace_the_defaults_in_their_order() {
        let (_tmp, ssh_dir) = ssh_dir(&["work", "home"]);
        let files = vec![
            ssh_dir.join("work"),
            ssh_dir.join("typo"),
            ssh_dir.join("home"),
        ];
        let plan = plan_with_config(IdentityAgent::FromEnvironment, files, None).unwrap();

        assert_eq!(plan.agent, OfferedAgent::Unset);
        assert_eq!(
            plan.key_files,
            vec![ssh_dir.join("work"), ssh_dir.join("home")]
        );
        assert_eq!(plan.absent_key_files, vec![ssh_dir.join("typo")]);
        assert_eq!(plan.answer(0), key_file(&ssh_dir.join("work")));
        assert_eq!(plan.answer(1), key_file(&ssh_dir.join("home")));
    }

    #[test]
    fn the_agent_the_configuration_selects_is_offered_when_the_process_is_bound_to_it() {
        let socket = PathBuf::from("/run/agent.sock");
        let plan = plan_with_config(
            IdentityAgent::Socket(socket.clone()),
            Vec::new(),
            Some(socket.clone()),
        )
        .unwrap();
        assert_eq!(plan.agent, OfferedAgent::Socket(socket));
        assert_eq!(plan.answer(0), agent_key());
    }

    #[test]
    fn an_agent_the_process_is_not_bound_to_is_refused_by_name() {
        let selected = PathBuf::from("/run/1password.sock");
        let refusal = plan_with_config(
            IdentityAgent::Socket(selected.clone()),
            Vec::new(),
            Some(PathBuf::from("/run/launchd.sock")),
        )
        .unwrap_err();
        assert_eq!(
            refusal,
            "~/.ssh/config selects the ssh agent at /run/1password.sock for github.com \
             (IdentityAgent), but this daemon is bound to the agent at /run/launchd.sock for its \
             lifetime: when it starts it binds the agent ssh selects for a host with no \
             host-specific configuration. Give every host that IdentityAgent (a `Host *` block) \
             and restart the daemon"
        );

        let refusal =
            plan_with_config(IdentityAgent::Socket(selected), Vec::new(), None).unwrap_err();
        assert!(
            refusal.contains("bound to no agent (SSH_AUTH_SOCK is unset)"),
            "{refusal}"
        );
    }

    #[test]
    fn a_disabled_agent_is_never_offered() {
        let (tmp, ssh_dir) = ssh_dir(&["id_rsa"]);
        let plan = plan_with_config(
            IdentityAgent::Disabled,
            vec![ssh_dir.join("id_rsa")],
            Some(tmp.path().join("agent.sock")),
        )
        .unwrap();
        assert_eq!(plan.agent, OfferedAgent::Disabled);
        assert_eq!(plan.answer(0), key_file(&ssh_dir.join("id_rsa")));
    }

    #[test]
    fn the_refusal_names_what_was_offered_and_what_was_not() {
        let (tmp, ssh_dir) = ssh_dir(&["id_rsa"]);
        let socket = tmp.path().join("agent.sock");
        std::fs::write(&socket, b"").unwrap();
        let plan = plan_without_ssh(Some(socket.clone()), tmp.path()).unwrap();

        assert_eq!(
            plan.answer(2),
            CredentialAnswer::Refuse(format!(
                "the remote refused every credential peppy holds for git@github.com: the keys \
                 in the ssh agent at {}, and the identity files {} ({}, {} do not exist as \
                 files); load a key the remote accepts into the agent, or keep an unencrypted \
                 identity file at one of those paths",
                socket.display(),
                ssh_dir.join("id_rsa").display(),
                ssh_dir.join("id_ed25519").display(),
                ssh_dir.join("id_ecdsa").display(),
            ))
        );
    }

    #[test]
    fn a_missing_agent_socket_is_named_as_such() {
        let (tmp, _ssh_dir) = ssh_dir(&[]);
        let socket = tmp.path().join("gone.sock");
        let plan = plan_without_ssh(Some(socket.clone()), tmp.path()).unwrap();
        let CredentialAnswer::Refuse(refusal) = plan.answer(1) else {
            panic!("the second request has nothing left to offer");
        };
        assert!(
            refusal.starts_with(&format!(
                "the remote refused every credential peppy holds for git@github.com: the ssh \
                 agent at {}, whose socket does not exist, and no identity file (",
                socket.display()
            )),
            "{refusal}"
        );
    }

    #[test]
    fn a_connection_with_nothing_to_offer_refuses_the_first_request() {
        let plan = plan_with_config(IdentityAgent::Disabled, Vec::new(), None).unwrap();
        assert_eq!(
            plan.answer(0),
            CredentialAnswer::Refuse(
                "peppy holds no credential for git@github.com: no ssh agent (IdentityAgent \
                 none), and no identity file (the configuration names none); load a key the \
                 remote accepts into the agent, or keep an unencrypted identity file at one of \
                 those paths"
                    .to_owned()
            )
        );

        let plan = plan_without_ssh(None, Path::new("/nonexistent")).unwrap();
        let CredentialAnswer::Refuse(refusal) = plan.answer(0) else {
            panic!("nothing to offer");
        };
        assert!(
            refusal.starts_with(
                "peppy holds no credential for git@github.com: no ssh agent (SSH_AUTH_SOCK is \
                 unset), and no identity file (/nonexistent/.ssh/id_ed25519, "
            ),
            "{refusal}"
        );
    }

    #[test]
    fn username_only_requests_answer_the_user_git_would_auth_as() {
        let username_only = git2::CredentialType::USERNAME;
        assert_eq!(
            non_ssh_answer(Some("peppy"), username_only),
            CredentialAnswer::Username("peppy".to_owned())
        );
        assert_eq!(
            non_ssh_answer(None, username_only),
            CredentialAnswer::Username("git".to_owned())
        );
    }

    #[test]
    fn password_requests_refuse_with_the_embedded_url_hint() {
        assert_eq!(
            non_ssh_answer(Some("peppy"), git2::CredentialType::USER_PASS_PLAINTEXT),
            CredentialAnswer::Refuse(
                "peppy authenticates git over ssh through the ssh agent and identity files, \
                 and offers no other credentials; embed them in the repository URL instead"
                    .to_owned()
            )
        );
    }

    #[test]
    fn plans_are_built_for_ssh_urls_only() {
        let refusal = plan_for_url(
            "https://github.com/Peppy-bot/nodes-hub.git",
            "git".to_owned(),
        )
        .unwrap_err();
        assert_eq!(
            refusal,
            "https://github.com/Peppy-bot/nodes-hub.git is not an ssh URL peppy can read a host \
             from"
        );
    }
}
