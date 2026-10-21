//! Git utilities shared by the node command handlers: repo-path
//! sanitization, ref checkout, reading the commit a working tree sits on,
//! the shallow checkout of the one commit a read ref names, and a full
//! clone. Remote
//! repositories are read over HTTPS or SSH; the SSH ones authenticate the
//! way `ssh` itself would for the host, with the agent and identity files
//! `~/.ssh/config` selects, and have their host key checked against
//! `~/.ssh/known_hosts` (see [`install_credentials`]), so a private
//! repository is a `git@host:owner/repo.git` URL away rather than a local
//! checkout this machine must register instead.

use crate::source_url::SourceUrl;
use crate::ssh_config::{IdentityAgent, SshHostConfig, SshTarget, resolve_host_config};
use core_node_api::encoding::ReadRef;
use daemon_config::repository::GitCommit;
use git2::Repository;
use git2::build::{CheckoutBuilder, RepoBuilder};
use node_stack::build_io::format_bytes;
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

/// The name a remote advertises its HEAD under.
const REMOTE_HEAD: &str = "HEAD";

/// Why a shallow fetch of one commit failed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ShallowCheckoutError {
    /// The remote could not be read, or the checkout could not be written.
    Unreachable(String),
    /// The remote serves nothing under the read ref, or does not serve the
    /// commit.
    RefNotServed(String),
}

/// The one commit a shallow checkout fetches, as far as it is known before
/// the remote is asked.
#[derive(Debug, PartialEq, Eq)]
enum ShallowTarget {
    /// A commit named by its hash.
    Commit(GitCommit),
    /// The object of the first of these refs that the remote advertises.
    FirstAdvertised(Vec<String>),
}

impl ShallowTarget {
    /// What `read_ref` names, the remote's HEAD when `None`.
    ///
    /// A release tag is its full name alone, so that a branch of the same
    /// short name can never stand in for it. A named ref of 40 hexadecimal
    /// characters is a commit. Any other named ref is looked up as git looks
    /// up a short name: as written (`HEAD`, or a full name such as
    /// `refs/tags/v1`), then as a branch, then as a tag.
    fn of(read_ref: Option<&ReadRef>) -> Self {
        match read_ref {
            None => Self::FirstAdvertised(vec![REMOTE_HEAD.to_owned()]),
            Some(release @ ReadRef::ReleaseTag(_)) => {
                Self::FirstAdvertised(vec![release.git_name()])
            }
            Some(ReadRef::Named(name)) => match GitCommit::parse(name) {
                Ok(commit) => Self::Commit(commit),
                Err(_) => Self::FirstAdvertised(vec![
                    name.clone(),
                    format!("refs/heads/{name}"),
                    format!("refs/tags/{name}"),
                ]),
            },
        }
    }
}

/// The first of `names` that the remote advertises, with the object it
/// advertises under that name.
fn first_advertised<'n>(
    advertised: &[git2::RemoteHead<'_>],
    names: &'n [String],
) -> Option<(&'n str, git2::Oid)> {
    names.iter().find_map(|name| {
        advertised
            .iter()
            .find(|head| head.name() == name)
            .map(|head| (name.as_str(), head.oid()))
    })
}

/// The name of a ref the remote advertises at `commit`, if any. The peeled
/// line of an annotated tag (`refs/tags/v1^{}`) names the tag itself.
fn advertised_name_of(advertised: &[git2::RemoteHead<'_>], commit: &GitCommit) -> Option<String> {
    advertised
        .iter()
        .find(|head| head.oid().to_string() == commit.as_str())
        .map(|head| {
            let name = head.name();
            name.strip_suffix("^{}").unwrap_or(name).to_owned()
        })
}

/// A new, empty repository at `dst`, which a fetch fills.
///
/// It names no remote: each fetch reads its URL through an anonymous
/// remote, so the URL, and any credentials it embeds, is never written to
/// the repository.
pub(crate) fn init_empty_repository(dst: &Path) -> std::result::Result<Repository, String> {
    Repository::init(dst)
        .map_err(|e| format!("Failed to create a repository at {}: {e}", dst.display()))
}

/// Makes `dst` a working tree of `repo_url` at the one commit `read_ref`
/// names, the remote's HEAD when `None`, and returns that commit.
///
/// The way `repo refresh` reads a remote repository: [`fetch_shallow`]
/// brings that commit alone, at depth 1.
pub(crate) fn shallow_checkout_of_read_ref(
    repo_url: &str,
    read_ref: Option<&ReadRef>,
    dst: &Path,
    on_progress: &mut dyn FnMut(&str),
) -> std::result::Result<GitCommit, ShallowCheckoutError> {
    use ShallowCheckoutError::Unreachable;

    let repo = init_empty_repository(dst).map_err(Unreachable)?;
    let object = fetch_shallow(&repo, repo_url, &ShallowTarget::of(read_ref), on_progress)?;
    checkout_repo_ref(&repo, &object)
        .map_err(|e| Unreachable(format!("Failed to check out {object}: {e}")))?;
    head_commit(&repo).map_err(Unreachable)
}

/// Fetches `commit` alone into `repo` at depth 1 (see [`fetch_shallow`]).
pub(crate) fn fetch_commit_shallow(
    repo: &Repository,
    repo_url: &str,
    commit: &GitCommit,
    on_progress: &mut dyn FnMut(&str),
) -> std::result::Result<(), ShallowCheckoutError> {
    let target = ShallowTarget::Commit(commit.clone());
    fetch_shallow(repo, repo_url, &target, on_progress).map(drop)
}

/// Fetches the one commit `target` names from `repo_url` into `repo` at
/// depth 1, and returns the object to check out for it: the commit, or the
/// annotated tag of it.
///
/// The fetch brings that commit's tree, without its history and without any
/// other branch or tag of the remote, and writes no ref. The refs the remote
/// advertises are listed first, on the connection the fetch then uses, so
/// the two cannot disagree. A ref is looked up among them (see
/// [`ShallowTarget::of`]) and fetched by its name. A commit is fetched by
/// the name of a ref the remote advertises at it, which every host answers,
/// and else by its hash, which a host answers when it serves commits by
/// hash, as GitHub does.
///
/// The depth is dropped when `repo_url` targets the local transport (see
/// [`is_local_url`]), which then brings the whole history.
fn fetch_shallow(
    repo: &Repository,
    repo_url: &str,
    target: &ShallowTarget,
    on_progress: &mut dyn FnMut(&str),
) -> std::result::Result<String, ShallowCheckoutError> {
    use ShallowCheckoutError::{RefNotServed, Unreachable};

    let shown_url = SourceUrl::new(repo_url);
    let redacted = |e: git2::Error| shown_url.redact_in(&e.to_string());
    let mut remote = repo
        .remote_anonymous(repo_url)
        .map_err(|e| Unreachable(redacted(e)))?;
    let mut connect_callbacks = git2::RemoteCallbacks::new();
    install_credentials(&mut connect_callbacks);
    let mut connection = remote
        .connect_auth(git2::Direction::Fetch, Some(connect_callbacks), None)
        .map_err(|e| Unreachable(format!("Failed to connect to {shown_url}: {}", redacted(e))))?;
    let advertised = connection.list().map_err(|e| {
        Unreachable(format!(
            "Failed to list the refs of {shown_url}: {}",
            redacted(e)
        ))
    })?;

    match target {
        ShallowTarget::Commit(commit) => {
            let by_name = advertised_name_of(advertised, commit);
            let refspec = by_name.as_deref().unwrap_or(commit.as_str());
            fetch_with_progress(connection.remote(), repo_url, refspec, true, on_progress)
                .map_err(|e| {
                    RefNotServed(format!("fetching the commit failed: {}", redacted(e)))
                })?;
            Ok(commit.as_str().to_owned())
        }
        ShallowTarget::FirstAdvertised(names) => {
            let Some((name, object)) = first_advertised(advertised, names) else {
                let quoted: Vec<String> = names.iter().map(|name| format!("`{name}`")).collect();
                return Err(RefNotServed(format!(
                    "it advertises no ref named {}",
                    quoted.join(" or ")
                )));
            };
            fetch_with_progress(connection.remote(), repo_url, name, true, on_progress).map_err(
                |e| {
                    Unreachable(format!(
                        "Failed to fetch {name} from {shown_url}: {}",
                        redacted(e)
                    ))
                },
            )?;
            Ok(object.to_string())
        }
    }
}

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

/// Clones `repo_url` with its whole history into `dst` and checks out
/// `repo_ref` if set, emitting throttled (~500ms) `transfer_progress` lines
/// via `on_progress` so callers can surface live byte/object counts instead
/// of sitting silent for the duration of the clone.
///
/// Each progress line is formatted as
/// `"Cloning {repo_url}: received {recv}/{total} objects ({bytes})"`.
pub(crate) fn clone_with_progress(
    repo_url: &str,
    repo_ref: Option<&str>,
    dst: &Path,
    on_progress: &mut dyn FnMut(&str),
) -> std::result::Result<Repository, String> {
    let mut builder = RepoBuilder::new();
    builder.fetch_options(fetch_options(repo_url, false, "Cloning", on_progress));

    let repo = builder.clone(repo_url, dst).map_err(|e| {
        let shown_url = SourceUrl::new(repo_url);
        let error = shown_url.redact_in(&e.to_string());
        format!("Failed to clone {shown_url}: {error}")
    })?;

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
/// `shallow` requests a `depth=1` fetch, which is dropped when `repo_url`
/// targets the local transport (see [`is_local_url`]). The fetch brings
/// the objects of what `refspec` names and no tag besides, and writes no
/// `FETCH_HEAD`.
pub(crate) fn fetch_with_progress(
    remote: &mut git2::Remote<'_>,
    repo_url: &str,
    refspec: &str,
    shallow: bool,
    on_progress: &mut dyn FnMut(&str),
) -> std::result::Result<(), git2::Error> {
    let mut fetch_opts = fetch_options(repo_url, shallow, "Fetching", on_progress);
    fetch_opts.download_tags(git2::AutotagOption::None);
    fetch_opts.update_fetchhead(false);
    remote.fetch(&[refspec], Some(&mut fetch_opts), None)
}

/// The fetch options of a clone or a fetch of `repo_url`, reporting progress
/// lines that start with `verb` through `on_progress`.
///
/// `shallow` requests a `depth=1` fetch, which is dropped when `repo_url`
/// targets the local transport (see [`is_local_url`]).
fn fetch_options<'cb>(
    repo_url: &str,
    shallow: bool,
    verb: &'static str,
    on_progress: &'cb mut dyn FnMut(&str),
) -> git2::FetchOptions<'cb> {
    let mut fetch_opts = git2::FetchOptions::new();
    if shallow && !is_local_url(repo_url) {
        fetch_opts.depth(1);
    }
    fetch_opts.remote_callbacks(progress_callbacks(repo_url, verb, on_progress));
    fetch_opts
}

fn progress_callbacks<'cb>(
    repo_url: &str,
    verb: &'static str,
    on_progress: &'cb mut dyn FnMut(&str),
) -> git2::RemoteCallbacks<'cb> {
    let shown_url = SourceUrl::new(repo_url).to_string();
    let mut last_report = Instant::now()
        .checked_sub(PROGRESS_REPORT_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut callbacks = git2::RemoteCallbacks::new();
    install_credentials(&mut callbacks);
    callbacks.transfer_progress(move |progress| {
        if last_report.elapsed() >= PROGRESS_REPORT_INTERVAL {
            last_report = Instant::now();
            on_progress(&format!(
                "{verb} {shown_url}: received {recv}/{total} objects ({bytes})",
                recv = progress.received_objects(),
                total = progress.total_objects(),
                bytes = format_bytes(progress.received_bytes() as u64),
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
    let target = SshTarget::from_git_url(url).ok_or_else(|| {
        format!(
            "{} is not an ssh URL peppy can read a host from",
            SourceUrl::new(url)
        )
    })?;
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
    fn a_read_ref_targets_a_commit_or_the_refs_its_name_is_looked_up_as() {
        let named = |name: &str| ShallowTarget::of(Some(&ReadRef::Named(name.to_owned())));
        let first_advertised = |names: &[&str]| {
            ShallowTarget::FirstAdvertised(names.iter().map(|name| name.to_string()).collect())
        };
        assert_eq!(
            named("0123456789ABCDEF0123456789abcdef01234567"),
            ShallowTarget::Commit(
                GitCommit::parse("0123456789abcdef0123456789abcdef01234567").unwrap()
            ),
            "40 hexadecimal characters name a commit, fetched by its hash"
        );
        assert_eq!(
            named("v1.2.0"),
            first_advertised(&["v1.2.0", "refs/heads/v1.2.0", "refs/tags/v1.2.0"]),
            "a short name is a branch before it is a tag"
        );
        assert_eq!(
            named("refs/tags/release/v1"),
            first_advertised(&[
                "refs/tags/release/v1",
                "refs/heads/refs/tags/release/v1",
                "refs/tags/refs/tags/release/v1",
            ]),
            "a full name is looked up as written first"
        );
        assert_eq!(
            named("0123abc"),
            first_advertised(&["0123abc", "refs/heads/0123abc", "refs/tags/0123abc"]),
            "an abbreviated hash is no commit peppy can fetch"
        );
        assert_eq!(
            ShallowTarget::of(Some(&ReadRef::ReleaseTag("v0.31.2".to_owned()))),
            first_advertised(&["refs/tags/peppy-release/v0.31.2"]),
            "a release tag is its full name alone"
        );
        assert_eq!(
            ShallowTarget::of(None),
            first_advertised(&["HEAD"]),
            "no read ref reads the remote's HEAD"
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
