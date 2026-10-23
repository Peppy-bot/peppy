use crate::Result;
use crate::services::action_loop::{
    GoalEnding, GoalHandler, accept_goal, invalid_payload, refuse_goal, run_action_loop,
};
use crate::services::node::cache as node_cache;
use crate::services::node::common::encode_response_or_err;
use crate::services::node::gate::{Admission, ConcurrencyGate};
use crate::services::node::{ShallowCheckoutError, shallow_checkout_of_read_ref};
use crate::services::repo::cache::{RepoCacheEntry, RepoItems, write_repo_cache};
use crate::services::repo::exclude::ExclusionSet;
use crate::services::repo::index::{
    PublishedItem, ReadSource, build_cache_entries, read_published_items,
};
use crate::services::repo::init::write_default_repos;
use crate::services::repo::parallel::map_in_parallel;
use crate::services::repo::status::{self, RepoStatus, RepoStatusFailure};
use crate::services::repo::{
    REPOS_FILE, RepoOwners, normalize_repo_entries, parse_repo_entry, repo_entry_id,
    source_identity,
};
use crate::source_url::SourceUrl;
use config::failure_kind::PeppyFailureKind;
use core_node_api::ActionId;
use core_node_api::encoding::{
    GitRepoRef, PEPPY_RELEASE_REF, PeppyBuild, ReadRef, RepoItemKind, RepoRefreshFeedback,
    RepoRefreshGoal, RepoRefreshGoalResponse, RepoRefreshResult, RepoSource, RepoSourceKind,
};
use core_node_api::names;
use daemon_config::consts::{PeppyDirs, peppy_build};
use daemon_config::repository::GitCommit;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::{CancelCause, ConcurrentAction, Failure, PendingGoal};
use peppylib::runtime::CancellationToken;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyResult};
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};
use std::num::NonZeroUsize;
use std::path::Path;
use std::time::SystemTime;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

pub async fn listen_for_repo_refresh(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    peppy_dirs: PeppyDirs,
) -> Result<JoinHandle<Result<()>>> {
    let action = ConcurrentAction::expose(
        messenger,
        core_node_name,
        instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        ActionId::RepoRefresh.name(),
        true,
    )
    .await?;

    let handler = RepoRefreshGoalHandler {
        peppy_dirs: peppy_dirs.clone(),
        gate: ConcurrencyGate::new(),
    };

    let handle = tokio::spawn(async move { run_action_loop(action, handler).await });

    Ok(handle)
}

#[derive(Clone)]
struct RepoRefreshGoalHandler {
    peppy_dirs: PeppyDirs,
    gate: ConcurrencyGate,
}

fn encode_refresh_accepted() -> PeppyResult<Payload> {
    encode_response_or_err(
        ActionId::RepoRefresh.name(),
        RepoRefreshGoalResponse::new().encode(),
    )
}

impl GoalHandler for RepoRefreshGoalHandler {
    async fn handle_goal(&self, pending: PendingGoal) {
        if let Err(e) = RepoRefreshGoal::decode(pending.request_bytes()) {
            refuse_goal(pending, invalid_payload(e)).await;
            return;
        }

        let generation = match self.gate.try_admit(300, false) {
            // `repo_refresh` never forces, so nothing is ever superseded here.
            Admission::Admitted { generation, .. } => generation,
            Admission::AlreadyRunning { .. } => {
                refuse_goal(
                    pending,
                    Failure::new(
                        PeppyFailureKind::PreconditionFailed,
                        "a repo refresh operation is already in progress",
                    ),
                )
                .await;
                return;
            }
        };

        let Some(goal_ctx) = accept_goal(pending, encode_refresh_accepted()).await else {
            self.gate.clear_running();
            return;
        };

        let peppy_dirs = self.peppy_dirs.clone();
        let feedback_publisher = goal_ctx
            .feedback_publisher()
            .expect("repo_refresh declares a feedback topic");
        let gate_for_task = self.gate.clone();

        tokio::spawn(async move {
            // Frees the gate slot on every exit: explicitly before the end on
            // the normal path (via `release_then_end` below), or on unwind
            // for a panic. A no-op if a later goal already took over.
            let slot = gate_for_task.into_slot_guard(generation);
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RepoRefreshFeedback>();

            let drain = tokio::spawn(async move {
                while let Some(feedback) = rx.recv().await {
                    if let Ok(payload) = feedback.encode() {
                        let _ = feedback_publisher.publish(payload).await;
                    }
                }
            });

            // A cancel of the goal's caller, or of its gone sender, stops the
            // refresh between two of its steps: it reads no other repository
            // and publishes nothing, as a refresh that fails. The refresh runs
            // on a blocking thread, which reads the cancel of the goal
            // itself.
            let refreshed = refresh_and_write_caches(
                &peppy_dirs,
                peppy_build(),
                RefreshScope::Every,
                SystemTime::now(),
                move |feedback| {
                    let _ = tx.send(feedback);
                },
                &goal_ctx.cancellation(),
            )
            .await;

            let ending = match refreshed {
                Ok(Some(refreshed)) => {
                    GoalEnding::of(Ok(refresh_result(&refreshed).encode()), None)
                }
                Ok(None) => GoalEnding::Cancelled(
                    goal_ctx
                        .cancel_cause()
                        .unwrap_or(CancelCause::Caller)
                        .reason()
                        .to_owned(),
                ),
                Err(e) => {
                    warn!("Repo refresh failed: {}", e);
                    GoalEnding::Failed(e.into_failure())
                }
            };

            // Flush all pending feedbacks before ending: the end-of-stream
            // sentinel that the end emits must not race ahead of the final
            // feedback lines.
            let _ = drain.await;

            slot.release_then_end(&goal_ctx, ending).await;
        });
    }
}

/// What one refresh reports back: the unique identities in each of the
/// five caches, and the repositories that failed.
///
/// A repository that could not be read does not fail the run. The refresh
/// did everything it could: it published the caches, every other
/// repository is current, and this one still serves what it last
/// published. Reporting that as a failure takes down whatever ran the
/// refresh, which on a fresh install is the installer itself, over a hub
/// that was briefly unreachable. The report names them so the caller can
/// say so and offer the retry.
fn refresh_result(refreshed: &RefreshedRepos) -> RepoRefreshResult {
    let items = &refreshed.items;
    let nodes = count_unique(&items.nodes);
    let launchers = count_unique(&items.launchers);
    let contracts = count_unique(&items.contracts);
    let pairings = count_unique(&items.pairings);
    let mcp_exposures = count_unique(&items.mcp_exposures);
    let result = RepoRefreshResult::new(nodes, launchers, contracts, pairings, mcp_exposures);
    if refreshed.failures.is_empty() {
        return result;
    }
    result.with_read_failure_report(failure_report(&refreshed.failures))
}

/// One message naming every repository that failed and why, so a user
/// with four problems fixes four things after one run rather than
/// running four times. `failures` arrives in repository id order, which
/// is what makes the report reproducible between machines.
pub(crate) fn failure_report(failures: &[RepoFailure]) -> String {
    let lines: Vec<String> = failures.iter().map(|f| f.to_string()).collect();
    format!(
        "{} of the configured repositories could not be updated. \
         Every other repository was updated normally.{}",
        failures.len(),
        daemon_config::format_bulleted(&lines)
    )
}

/// Re-indexes after a change to the repository list, returning a report
/// of everything that went wrong, or `None` when it all worked.
///
/// The caller's own edit has already been applied; this is the re-read
/// that makes it take effect. Its problems belong in the caller's
/// response rather than in a log nobody reads: this is the recovery
/// path, where a user who just added, removed or excluded a repository
/// to unblock themselves needs to know whether it worked.
pub(crate) async fn reindex_after_change(peppy_dirs: &PeppyDirs) -> Option<String> {
    let refreshed = refresh_and_write_caches(
        peppy_dirs,
        peppy_build(),
        RefreshScope::Every,
        SystemTime::now(),
        |_| {},
        &CancellationToken::new(),
    )
    .await;
    match refreshed {
        Ok(Some(refreshed)) if refreshed.failures.is_empty() => None,
        Ok(Some(refreshed)) => Some(failure_report(&refreshed.failures)),
        Ok(None) => Some("re-indexing was cancelled".to_owned()),
        Err(e) => Some(format!("re-indexing failed: {e}")),
    }
}

/// Refreshes the repositories `scope` names, with `@{peppy-release}`
/// resolved for `build`, and publishes the caches of the result, under
/// [`refresh_lock`](crate::services::repo::refresh_lock). Runs on tokio's
/// blocking pool: a refresh clones repositories over the network. A panic
/// of the refresh is returned as an error.
///
/// The caches are written whether or not a repository failed: the whole
/// point of containing a failure is that the repositories that did update
/// take effect, and that the ones that did not keep the entries they last
/// published. Caches store every entry so that `repo list` can display
/// every source and users can pick a specific `sha256`.
///
/// Returns `None` once `cancel` is cancelled before the caches are written:
/// the refresh reads no other repository and publishes nothing, as a
/// refresh that fails.
pub(crate) async fn refresh_and_write_caches(
    peppy_dirs: &PeppyDirs,
    build: PeppyBuild,
    scope: RefreshScope,
    now: SystemTime,
    mut on_feedback: impl FnMut(RepoRefreshFeedback) + Send + 'static,
    cancel: &CancellationToken,
) -> Result<Option<RefreshedRepos>> {
    let peppy_dirs = peppy_dirs.clone();
    let cancel = cancel.clone();
    tokio::task::spawn_blocking(move || {
        let _guard = crate::services::repo::refresh_lock().lock();
        let Some(refreshed) =
            process_refresh(&peppy_dirs, &build, &scope, now, &mut on_feedback, &cancel)?
        else {
            return Ok(None);
        };
        if cancel.is_cancelled() {
            return Ok(None);
        }
        write_all_caches(&peppy_dirs, &refreshed)?;
        Ok(Some(refreshed))
    })
    .await?
}

/// The repositories on `@{peppy-release}` that this machine last read at
/// another ref than `build` reads for them: after an upgrade or a downgrade,
/// the release tags of the version that read them, or the `main` of a build
/// that is not a release.
///
/// A repository is judged by its own status line (see [`status_line_of`]).
/// A repository with no status line of its own has never been read here as
/// it is configured, so there is nothing of another version to replace, and
/// the next `repo refresh` reads it.
pub(crate) fn release_repositories_read_for_another_version(
    repos: &[Value],
    exclusions: &ExclusionSet,
    statuses: &[RepoStatus],
    build: &PeppyBuild,
) -> BTreeSet<u64> {
    let expected_read_ref = GitRepoRef::PeppyRelease.read_ref(build);
    repos
        .iter()
        .filter_map(|entry| {
            let id = repo_entry_id(entry)?;
            let source = parse_repo_entry(entry).ok()?;
            let RepoSource::Git {
                repo_ref: GitRepoRef::PeppyRelease,
                ..
            } = &source
            else {
                return None;
            };
            let identity = source_identity(&source);
            if exclusions.is_excluded(&identity) {
                return None;
            }
            let status = status_line_of(statuses, id, &identity)?;
            (status.read_ref != expected_read_ref).then_some(id)
        })
        .collect()
}

/// The status line of the repository with `id` and `identity`. Matched on
/// identity as well as id: an id repointed at another path, url or ref is a
/// different repository, and the line of the old one describes reads this
/// source never made.
fn status_line_of<'a>(
    statuses: &'a [RepoStatus],
    id: u64,
    identity: &str,
) -> Option<&'a RepoStatus> {
    statuses
        .iter()
        .find(|status| status.id == id && status.identity == identity)
}

/// What [`refresh_release_repositories_for_this_version`] did.
#[derive(Debug)]
pub(crate) enum VersionRefresh {
    /// Every repository on `@{peppy-release}` was read for this version, or
    /// never read on this machine as it is configured: nothing to read
    /// again.
    NotNeeded,
    /// The repositories with these ids were read again, and these of them
    /// failed.
    Refreshed {
        ids: BTreeSet<u64>,
        failures: Vec<RepoFailure>,
    },
}

/// Reads again every repository on `@{peppy-release}` that was read for
/// another peppy version (see
/// [`release_repositories_read_for_another_version`]), and only those.
///
/// The daemon runs this at start, before any listener answers, so it never
/// serves the hub content of another version and a `repo refresh` that
/// arrives meanwhile is not refused as a refresh already in progress. A
/// repository whose read fails keeps its previous entries, as in any
/// refresh, and is read again at the next start. A version change never
/// writes `repositories.json5`: the entries name `@{peppy-release}`, not a
/// version.
pub(crate) async fn refresh_release_repositories_for_this_version(
    peppy_dirs: &PeppyDirs,
    build: &PeppyBuild,
    now: SystemTime,
) -> Result<VersionRefresh> {
    let ids = {
        let _guard = crate::services::repo::repos_file_lock().lock();
        let repos = read_or_create_repos(peppy_dirs)?;
        release_repositories_read_for_another_version(
            &repos,
            &ExclusionSet::load(peppy_dirs),
            &status::read(peppy_dirs),
            build,
        )
    };
    if ids.is_empty() {
        return Ok(VersionRefresh::NotNeeded);
    }
    let refreshed = refresh_and_write_caches(
        peppy_dirs,
        build.clone(),
        RefreshScope::Only(ids.clone()),
        now,
        |_| {},
        &CancellationToken::new(),
    )
    .await?;
    // Nothing cancels this refresh, so it always reads.
    let failures = refreshed
        .map(|refreshed| refreshed.failures)
        .unwrap_or_default();
    Ok(VersionRefresh::Refreshed { ids, failures })
}

/// A repository that was skipped during refresh because it appears in the
/// `excluded_repositories.json5` configuration.
#[derive(Debug, Clone)]
pub(crate) struct ExcludedRepo {
    pub(crate) source_type: RepoSourceKind,
    pub(crate) identity: String,
}

/// How the read of one repository failed, so that it contributed nothing
/// new to a refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepoReadFailure {
    /// Could not be read at all: the clone failed, the configured path is
    /// gone, the configuration entry is unrecognized. Kept distinct from
    /// [`RepoReadFailure::Conflict`] because an outage is not a content
    /// bug, and reporting one as the other sends the user to the wrong
    /// place entirely.
    Unreachable,
    /// Read fine; the contents are wrong. An identity is claimed by
    /// several manifests, so the repository states two different answers
    /// to the same question and there is no defensible winner.
    Conflict,
}

impl RepoReadFailure {
    /// Stable machine-readable value, recorded in `repo_status.json5` and
    /// put on the wire. Kept separate from [`Self::describe`] so the
    /// prose can be reworded without changing what tools match on.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            RepoReadFailure::Unreachable => "unreachable",
            RepoReadFailure::Conflict => "conflict",
        }
    }

    /// Predicate that reads as a sentence about the repository.
    fn describe(self) -> &'static str {
        match self {
            RepoReadFailure::Unreachable => "could not be read",
            RepoReadFailure::Conflict => "contradicts itself",
        }
    }
}

/// One repository whose read failed, and what the machine fell back to.
/// A failure is scoped to its own repository: the others still update.
#[derive(Debug, Clone)]
pub(crate) struct RepoFailure {
    pub(crate) id: u64,
    /// Human-facing label (path for fs, `url (ref: r)` for git).
    pub(crate) label: String,
    pub(crate) read_failure: RepoReadFailure,
    pub(crate) detail: String,
    /// Previous entries kept in place for this repository, across all
    /// five kinds. Zero on a machine that has never read it successfully.
    pub(crate) retained: usize,
}

impl std::fmt::Display for RepoFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "repository {} ({}) {} [{}]: {}",
            self.id,
            self.label,
            self.read_failure.describe(),
            self.read_failure.as_str(),
            self.detail
        )?;
        match self.retained {
            0 => f.write_str(
                ". Nothing had been read from it before, so it contributes nothing this time",
            ),
            1 => f.write_str(". Kept 1 entry from its last successful read"),
            n => write!(f, ". Kept {n} entries from its last successful read"),
        }
    }
}

/// Aggregated output of [`process_refresh`]: all entries that belong in
/// the caches, the repositories skipped by the exclusion list, and the
/// repositories that failed.
pub(crate) struct RefreshedRepos {
    /// Freshly read, plus previous entries retained for repositories that
    /// failed and kept for repositories outside the refresh's scope.
    pub(crate) items: RepoItems,
    /// Reported to the client through [`RepoRefreshFeedback::Excluded`]
    /// as the scan runs, so the handler has nothing left to do with it;
    /// kept on the result because the tests assert against it.
    #[allow(dead_code)]
    pub(crate) excluded: Vec<ExcludedRepo>,
    pub(crate) failures: Vec<RepoFailure>,
    /// One entry per repository that was actually read (excluded ones are
    /// absent), carrying when it last read cleanly and how it last failed.
    pub(crate) statuses: Vec<RepoStatus>,
}

/// The five caches as they stood before this refresh. A repository that
/// fails its read keeps serving what it last published, so its
/// identities do not vanish out from under launchers that reference
/// them.
///
/// A cache that cannot be read is treated as empty rather than fatal:
/// this runs on the recovery path, where refusing to start because the
/// fallback is also broken helps nobody.
fn load_previous_caches(peppy_dirs: &PeppyDirs) -> RepoItems {
    fn read<E: RepoCacheEntry>(peppy_dirs: &PeppyDirs) -> Vec<E> {
        crate::services::repo::cache::load_repo_cache::<E>(peppy_dirs).unwrap_or_else(|e| {
            warn!("Could not read the previous {} cache: {e}", E::KIND);
            Vec::new()
        })
    }
    RepoItems {
        nodes: read(peppy_dirs),
        launchers: read(peppy_dirs),
        contracts: read(peppy_dirs),
        pairings: read(peppy_dirs),
        mcp_exposures: read(peppy_dirs),
    }
}

/// The previous entries of one kind that `repo_id` owns, using the same
/// attribution rule that tags entries at lookup time so a retained entry
/// keeps exactly the priority it had.
fn retained_entries<E: RepoCacheEntry>(
    previous: &[E],
    owners: &RepoOwners,
    repo_id: u64,
) -> Vec<E> {
    previous
        .iter()
        .filter(|e| owners.owner_of(e.origin()) == Some(repo_id))
        .cloned()
        .collect()
}

/// The previous entries of every kind that `repo_id` owns (see
/// [`retained_entries`]).
fn retained_items(previous: &RepoItems, owners: &RepoOwners, repo_id: u64) -> RepoItems {
    RepoItems {
        nodes: retained_entries(&previous.nodes, owners, repo_id),
        launchers: retained_entries(&previous.launchers, owners, repo_id),
        contracts: retained_entries(&previous.contracts, owners, repo_id),
        pairings: retained_entries(&previous.pairings, owners, repo_id),
        mcp_exposures: retained_entries(&previous.mcp_exposures, owners, repo_id),
    }
}

/// Publishes every cache file from one refresh result. The five entry
/// caches must always move together: rewriting only a subset leaves the
/// untouched files still listing items from repositories that are no
/// longer configured. The status file moves with them so it always
/// describes the entries that are actually on disk.
pub(crate) fn write_all_caches(peppy_dirs: &PeppyDirs, refreshed: &RefreshedRepos) -> Result<()> {
    let items = &refreshed.items;
    write_repo_cache(peppy_dirs, &items.nodes)?;
    write_repo_cache(peppy_dirs, &items.launchers)?;
    write_repo_cache(peppy_dirs, &items.contracts)?;
    write_repo_cache(peppy_dirs, &items.pairings)?;
    write_repo_cache(peppy_dirs, &items.mcp_exposures)?;
    status::write(peppy_dirs, &refreshed.statuses)?;

    // Pruned only once the caches that could still name a checkout have
    // been replaced, so nothing is dropped while it is still reachable. A
    // repository whose read failed keeps its previous entries, so its
    // checkouts stay live along with them.
    let removed = node_cache::git::prune_checkouts(peppy_dirs, live_checkouts(items));
    if removed > 0 {
        debug!("Removed {removed} cached git checkout(s) no repository cache points at");
    }
    Ok(())
}

/// Every `(repo_url, commit)` the five caches still resolve through the
/// checkout cache.
fn live_checkouts(items: &RepoItems) -> impl Iterator<Item = (&str, &GitCommit)> {
    fn checkouts<E: RepoCacheEntry>(entries: &[E]) -> impl Iterator<Item = (&str, &GitCommit)> {
        entries.iter().filter_map(|entry| entry.origin().checkout())
    }
    checkouts(&items.nodes)
        .chain(checkouts(&items.launchers))
        .chain(checkouts(&items.contracts))
        .chain(checkouts(&items.pairings))
        .chain(checkouts(&items.mcp_exposures))
}

/// Reads the repositories.json5 config file, creating it with defaults if it
/// does not exist yet (see [`write_default_repos`]). Ensures every entry has
/// an integer `id` field (auto-assigns missing ids) and returns entries
/// sorted by `id`.
pub(crate) fn read_or_create_repos(peppy_dirs: &PeppyDirs) -> Result<Vec<Value>> {
    let conf_dir = peppy_dirs.conf_dir();
    let repos_path = conf_dir.join(REPOS_FILE);
    if !repos_path.exists() {
        write_default_repos(&conf_dir)?;
    }

    let content = std::fs::read_to_string(&repos_path)?;
    let mut repos: Vec<Value> = serde_json5::from_str(&content).map_err(|e| {
        core_node_api::Error::Decoding(format!("failed to parse repositories.json5: {e}"))
    })?;

    normalize_repo_entries(&mut repos, &repos_path, "repositories")?;

    Ok(repos)
}

/// Which configured repositories one refresh reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RefreshScope {
    /// Every configured repository: what `repo refresh` does.
    Every,
    /// Only the repositories with these ids. Every other repository keeps
    /// the entries it holds and its status, as they stand.
    Only(BTreeSet<u64>),
}

impl RefreshScope {
    fn includes(&self, id: u64) -> bool {
        match self {
            RefreshScope::Every => true,
            RefreshScope::Only(ids) => ids.contains(&id),
        }
    }
}

/// Main synchronous processing: reads repos, walks each source, returns
/// the entries that belong in the caches plus the repositories that were
/// excluded or failed.
///
/// Every discovered entry is kept in the result so the cache (and
/// `repo list`) can display every source. When several repositories
/// declare the same `(name, tag)`, lookup picks the lowest-id
/// repository at query time; the `sha256` on each entry lets users
/// distinguish entries with the same identity but different content.
/// Discovery feedback is emitted once per unique identity from the
/// highest-priority repository; extra entries are silently cached.
///
/// A failure is contained to the repository that caused it: that
/// repository keeps its previous entries and is recorded in `failures`,
/// while every other repository updates normally. The caller decides
/// what a non-empty `failures` means for the run as a whole. One
/// consequence is deliberate and worth knowing: a cache can then hold
/// entries read at two different times. The caches hold no synchronised
/// snapshot across repositories, so anything that needs one coherent set
/// of bytes must establish that itself.
/// `now` is passed in rather than read from the clock so that tests are
/// deterministic.
///
/// Reads the repositories `scope` names, with `@{peppy-release}` resolved
/// for `build`. A repository outside `scope` is not read: it keeps the
/// entries the caches hold for it and its status line, in its own slot of
/// the id order, so the caches written from the result still hold every
/// repository.
///
/// The repositories are read in parallel, up to [`MAX_PARALLEL_READS`] at
/// once, so a refresh takes about as long as its slowest read. The progress
/// lines of the reads interleave, and each names its repository. What the
/// reads yield is merged in id order when every read has ended, so the
/// priority of an entry, the discovery feedback, `failures` and `statuses`
/// never depend on which read ends first.
///
/// Returns `None` once `cancel` is cancelled, checked before the reads,
/// before each read starts and after the reads end: a read that runs ends
/// first, and no other one starts.
pub(crate) fn process_refresh(
    peppy_dirs: &PeppyDirs,
    build: &PeppyBuild,
    scope: &RefreshScope,
    now: SystemTime,
    on_feedback: &mut dyn FnMut(RepoRefreshFeedback),
    cancel: &CancellationToken,
) -> Result<Option<RefreshedRepos>> {
    let (repos, exclusions) = {
        let _guard = crate::services::repo::repos_file_lock().lock();
        let repos = read_or_create_repos(peppy_dirs)?;
        let exclusions = ExclusionSet::load(peppy_dirs);
        (repos, exclusions)
    };
    let previous = load_previous_caches(peppy_dirs);
    // Resolved once for the whole refresh: retention asks which repository
    // owns an entry for every previous entry of all five kinds, of every
    // repository that failed.
    let owners = RepoOwners::new(&repos);
    let previous_statuses = status::read(peppy_dirs);
    let stamp = status::unix_secs(now);
    let mut failures: Vec<RepoFailure> = Vec::new();
    let mut statuses: Vec<RepoStatus> = Vec::new();

    let mut merged = MergedItems::default();
    let excluded_repos: Vec<ExcludedRepo> = exclusions
        .entries
        .iter()
        .map(|e| ExcludedRepo {
            source_type: e.source_type,
            identity: e.identity.clone(),
        })
        .collect();

    for repo in &excluded_repos {
        on_feedback(RepoRefreshFeedback::Excluded {
            source_type: repo.source_type,
            identity: repo.identity.clone(),
        });
    }

    let planned: Vec<PlannedRepo> = repos
        .iter()
        .filter_map(|entry| plan_repo(entry, &exclusions, scope))
        .collect();
    let sources_to_read: Vec<&RepoSource> = planned
        .iter()
        .filter_map(PlannedRepo::source_to_read)
        .collect();
    if cancel.is_cancelled() {
        return Ok(None);
    }
    let mut reads = map_in_parallel(
        sources_to_read,
        MAX_PARALLEL_READS,
        on_feedback,
        |source, on_feedback| {
            if cancel.is_cancelled() {
                return RepoRead::cancelled();
            }
            read_repo(source, &exclusions, build, peppy_dirs, on_feedback)
        },
    )
    .into_iter();
    if cancel.is_cancelled() {
        return Ok(None);
    }

    for repo in planned {
        let (id, identity, source) = match repo {
            PlannedRepo::Unparseable { id, label, detail } => {
                failures.push(RepoFailure {
                    id,
                    label,
                    read_failure: RepoReadFailure::Unreachable,
                    detail,
                    retained: 0,
                });
                continue;
            }
            PlannedRepo::Kept { id, identity } => {
                statuses.extend(status_line_of(&previous_statuses, id, &identity).cloned());
                merged.merge(retained_items(&previous, &owners, id), on_feedback);
                continue;
            }
            PlannedRepo::Read {
                id,
                identity,
                source,
            } => (id, identity, source),
        };
        let RepoRead { items, read_ref } = reads
            .next()
            .expect("`map_in_parallel` returns one read per source to read, in their order");
        let previous_status = status_line_of(&previous_statuses, id, &identity);

        let items = match items {
            Ok(items) => {
                statuses.push(RepoStatus {
                    id,
                    identity,
                    source_type: source.kind(),
                    last_read_unix_secs: Some(stamp),
                    read_ref,
                    // Cleared on success, so a repository that recovered
                    // stops reporting an old failure.
                    last_failure: None,
                });
                items
            }
            Err((read_failure, detail)) => {
                let retained = retained_items(&previous, &owners, id);
                let failure = RepoFailure {
                    id,
                    label: shown_label(&source, build),
                    read_failure,
                    detail,
                    retained: retained.len(),
                };
                statuses.push(RepoStatus {
                    id,
                    identity,
                    source_type: source.kind(),
                    // Carried forward untouched: the retained entries are
                    // still the ones read at that time and at that ref, and
                    // overwriting them would claim they are current.
                    last_read_unix_secs: previous_status.and_then(|s| s.last_read_unix_secs),
                    read_ref: previous_status.and_then(|s| s.read_ref.clone()),
                    last_failure: Some(RepoStatusFailure {
                        read_failure: failure.read_failure.as_str().to_owned(),
                        message: failure.detail.clone(),
                        unix_secs: stamp,
                    }),
                });
                warn!("{failure}");
                on_feedback(RepoRefreshFeedback::Progress {
                    message: failure.to_string(),
                });
                failures.push(failure);
                retained
            }
        };

        merged.merge(items, on_feedback);
    }

    Ok(Some(RefreshedRepos {
        items: merged.items,
        excluded: excluded_repos,
        failures,
        statuses,
    }))
}

/// What one refresh does with one configured repository, decided from its
/// entry before any repository is read.
enum PlannedRepo {
    /// An entry peppy cannot parse, in the scope of the refresh. It has no
    /// source kind or identity to record, so it is reported but gets no
    /// status line.
    Unparseable {
        id: u64,
        label: String,
        detail: String,
    },
    /// Outside the scope of the refresh: it keeps the entries the caches hold
    /// for it and its status line, as they stand.
    Kept { id: u64, identity: String },
    /// Read by this refresh.
    Read {
        id: u64,
        identity: String,
        source: RepoSource,
    },
}

impl PlannedRepo {
    fn source_to_read(&self) -> Option<&RepoSource> {
        match self {
            PlannedRepo::Read { source, .. } => Some(source),
            PlannedRepo::Unparseable { .. } | PlannedRepo::Kept { .. } => None,
        }
    }
}

/// What a refresh in `scope` does with `entry`, or `None` when it leaves the
/// entry out: an excluded repository, or an entry peppy cannot parse outside
/// the scope.
fn plan_repo(
    entry: &Value,
    exclusions: &ExclusionSet,
    scope: &RefreshScope,
) -> Option<PlannedRepo> {
    let id = repo_entry_id(entry).unwrap_or(0);
    let source = match parse_repo_entry(entry) {
        Ok(source) => source,
        Err(error) => {
            return scope.includes(id).then(|| PlannedRepo::Unparseable {
                id,
                label: entry.to_string(),
                detail: error.to_string(),
            });
        }
    };

    let identity = source_identity(&source);
    if exclusions.is_excluded(&identity) {
        debug!("Excluding {} repository: {}", source.kind(), identity);
        return None;
    }
    if !scope.includes(id) {
        return Some(PlannedRepo::Kept { id, identity });
    }
    Some(PlannedRepo::Read {
        id,
        identity,
        source,
    })
}

/// How many repositories one refresh reads at once. A read waits mostly on
/// the network, so the reads of the default hubs and of a few more
/// repositories all run together, and a long repository list opens no more
/// than this many connections at a time.
const MAX_PARALLEL_READS: NonZeroUsize = NonZeroUsize::new(8).expect("8 is not zero");

/// What reading one repository yields: the entries it publishes or why it
/// gives none, and for a git repository the ref it was read at.
struct RepoRead {
    items: std::result::Result<RepoItems, (RepoReadFailure, String)>,
    read_ref: Option<ReadRef>,
}

impl RepoRead {
    /// The read a cancel stopped before it started; the refresh it belongs
    /// to publishes nothing.
    fn cancelled() -> Self {
        Self {
            items: Err((
                RepoReadFailure::Unreachable,
                "the refresh was cancelled".to_owned(),
            )),
            read_ref: None,
        }
    }
}

/// Reads one repository, with `@{peppy-release}` resolved for `build`.
fn read_repo(
    source: &RepoSource,
    exclusions: &ExclusionSet,
    build: &PeppyBuild,
    peppy_dirs: &PeppyDirs,
    on_feedback: &mut dyn FnMut(RepoRefreshFeedback),
) -> RepoRead {
    let (read, read_ref) = match source {
        RepoSource::Fs(path) => (read_fs_repo(path, exclusions, on_feedback), None),
        RepoSource::Git { repo_url, repo_ref } => {
            on_feedback(RepoRefreshFeedback::Progress {
                message: format!("Cloning {}", shown_label(source, build)),
            });
            let read_ref = repo_ref.read_ref(build);
            let read = read_git_repo(
                repo_url,
                repo_ref,
                read_ref.as_ref(),
                build,
                peppy_dirs,
                on_feedback,
            );
            (read, read_ref)
        }
    };

    // A repository that could not be read and one whose index does not
    // describe it both fall back to their previous entries; only the
    // wording differs, so an outage never reads as a content bug.
    let items = read.and_then(|items| {
        build_cache_entries(items).map_err(|detail| (RepoReadFailure::Conflict, detail))
    });
    RepoRead { items, read_ref }
}

/// Every repository's entries, merged in id order, with the
/// `(kind, name, tag)` identities seen so far.
#[derive(Default)]
struct MergedItems {
    items: RepoItems,
    seen: HashSet<(RepoItemKind, String, String)>,
}

impl MergedItems {
    /// Merges one repository's entries. Called in the repository's own slot
    /// in the id-ordered loop, whether they were read, retained or kept, so
    /// priority order and the first-seen discovery feedback stay exactly as
    /// they would have been.
    fn merge(&mut self, items: RepoItems, on_feedback: &mut dyn FnMut(RepoRefreshFeedback)) {
        merge_published(
            items.nodes,
            &mut self.seen,
            &mut self.items.nodes,
            on_feedback,
        );
        merge_published(
            items.launchers,
            &mut self.seen,
            &mut self.items.launchers,
            on_feedback,
        );
        merge_published(
            items.contracts,
            &mut self.seen,
            &mut self.items.contracts,
            on_feedback,
        );
        merge_published(
            items.pairings,
            &mut self.seen,
            &mut self.items.pairings,
            on_feedback,
        );
        merge_published(
            items.mcp_exposures,
            &mut self.seen,
            &mut self.items.mcp_exposures,
            on_feedback,
        );
    }
}

/// Appends one repository's entries to the running cross-repo collection,
/// emitting a `Discovered` feedback the first time each `(name, tag)`
/// identity of `E`'s kind is seen. Every entry is kept (including
/// same-identity duplicates from lower-priority repositories); feedback
/// fires only for the highest-priority repository, which is read first.
fn merge_published<E: RepoCacheEntry>(
    published: Vec<E>,
    global_seen: &mut HashSet<(RepoItemKind, String, String)>,
    all: &mut Vec<E>,
    on_feedback: &mut dyn FnMut(RepoRefreshFeedback),
) {
    for entry in published {
        let identity = (
            E::ITEM_KIND,
            entry.name().to_owned(),
            entry.tag().to_owned(),
        );
        if global_seen.insert(identity) {
            on_feedback(RepoRefreshFeedback::Discovered {
                kind: E::ITEM_KIND,
                item_name: entry.name().to_owned(),
                item_tag: entry.tag().to_owned(),
                source_type: entry.origin().kind(),
                path: entry.origin().path_str().to_owned(),
                sha256: entry.sha256().to_string(),
            });
        }
        all.push(entry);
    }
}

/// Count the number of distinct `(name, tag)` identities. Used to
/// compute `total_*_found` after process_refresh returns every entry
/// (including same-identity duplicates from lower-priority repos).
fn count_unique<E: RepoCacheEntry>(entries: &[E]) -> u32 {
    let set: HashSet<(&str, &str)> = entries.iter().map(|e| (e.name(), e.tag())).collect();
    set.len() as u32
}

/// What one repository read yields, or why it failed.
type ReadResult = std::result::Result<Vec<PublishedItem>, (RepoReadFailure, String)>;

/// Reads a repository that lives on this machine, minus the subtrees this
/// machine is configured to stay out of.
fn read_fs_repo(
    root: &Path,
    exclusions: &ExclusionSet,
    on_feedback: &mut dyn FnMut(RepoRefreshFeedback),
) -> ReadResult {
    if !root.exists() {
        return Err((
            RepoReadFailure::Unreachable,
            format!("path does not exist: {}", root.display()),
        ));
    }
    on_feedback(RepoRefreshFeedback::Progress {
        message: format!("Reading {}", root.display()),
    });

    read_published_items(
        root,
        &ReadSource::Fs {
            excluded: exclusions.fs_paths.clone(),
        },
    )
}

/// Reads a repository held by a remote, at the commit `read_ref`, the ref
/// `build` reads for `repo_ref`, currently points at.
///
/// What survives the read is the index the repository published and the
/// commit it was read at, which is what lets another machine read the same
/// bytes later, and the clone itself, which goes to the checkout cache
/// because it is already the tree every item read here resolves to.
fn read_git_repo(
    repo_url: &str,
    repo_ref: &GitRepoRef,
    read_ref: Option<&ReadRef>,
    build: &PeppyBuild,
    peppy_dirs: &PeppyDirs,
    on_feedback: &mut dyn FnMut(RepoRefreshFeedback),
) -> ReadResult {
    let unreachable = |detail: String| (RepoReadFailure::Unreachable, detail);

    let tmp_dir = peppy_dirs.tmp_dir();
    std::fs::create_dir_all(&tmp_dir)
        .map_err(|e| unreachable(format!("failed to create tmp dir: {e}")))?;
    let tmp = tempfile::tempdir_in(&tmp_dir)
        .map_err(|e| unreachable(format!("failed to create temp dir: {e}")))?;

    // The checkout is the commit of the ref this build reads, so the pin
    // follows the line of development the repository was added for rather
    // than whatever the remote's default branch happens to be. A ref the
    // remote does not serve is a refusal: pinning the default branch instead
    // would publish a different tree under the same repository id without
    // saying so.
    let commit = shallow_checkout_of_read_ref(repo_url, read_ref, tmp.path(), &mut |line| {
        on_feedback(RepoRefreshFeedback::Progress {
            message: line.to_owned(),
        });
    })
    .map_err(|e| match (e, read_ref) {
        (ShallowCheckoutError::Unreachable(detail), _) => unreachable(detail),
        (ShallowCheckoutError::RefNotServed(detail), Some(read)) => {
            unreachable(ref_not_served(repo_url, repo_ref, read, build, &detail))
        }
        (ShallowCheckoutError::RefNotServed(detail), None) => unreachable(format!(
            "{} has no commit to read: {detail}",
            SourceUrl::new(repo_url)
        )),
    })?;

    // The configured ref is what attributes an entry back to its repository,
    // and the ref it was read at is what a later fetch of the pinned commit
    // starts from, so the origin records both.
    let items = read_published_items(
        tmp.path(),
        &ReadSource::Git {
            repo_url: repo_url.to_owned(),
            repo_ref: repo_ref.configured().map(str::to_owned),
            read_ref: read_ref.cloned(),
            commit: commit.clone(),
        },
    )?;

    // Handed over rather than deleted: this clone is the tree every item
    // just read resolves to, so donating it is the difference between the
    // daemon fetching one commit of one repository once and fetching it
    // again the first time anything from it is materialized.
    node_cache::git::adopt_checkout(peppy_dirs, repo_url, &commit, tmp.keep());
    Ok(items)
}

/// Why a git repository was not read: the remote does not serve `read`,
/// the ref `build` reads for `repo_ref`.
fn ref_not_served(
    repo_url: &str,
    repo_ref: &GitRepoRef,
    read: &ReadRef,
    build: &PeppyBuild,
    error: &str,
) -> String {
    let shown_url = SourceUrl::new(repo_url);
    match (repo_ref, build) {
        (GitRepoRef::PeppyRelease, PeppyBuild::Release(version)) => format!(
            "{shown_url} does not carry the tag `{}`, which `{PEPPY_RELEASE_REF}` reads in peppy \
             {version}: {error}",
            read.short_name()
        ),
        (GitRepoRef::PeppyRelease, PeppyBuild::Unreleased) => format!(
            "{shown_url} does not serve `{}`, which `{PEPPY_RELEASE_REF}` reads in a peppy build \
             that is not a release: {error}",
            read.short_name()
        ),
        _ => format!(
            "{shown_url} does not serve the configured ref `{}`: {error}",
            read.short_name()
        ),
    }
}

/// The label of `source` as the daemon writes it: a git URL appears without
/// its credentials and its query.
fn shown_label(source: &RepoSource, build: &PeppyBuild) -> String {
    let label = source.display_label(build);
    match source {
        RepoSource::Git { repo_url, .. } => SourceUrl::new(repo_url).redact_in(&label),
        RepoSource::Fs(_) => label,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_shows_a_git_url_without_its_credentials_and_keeps_its_ref() {
        let source = RepoSource::Git {
            repo_url: "https://oauth2:glpat-secret@gitlab.example/acme/nodes.git".to_owned(),
            repo_ref: GitRepoRef::parse("main").expect("a git name"),
        };
        assert_eq!(
            shown_label(&source, &PeppyBuild::Unreleased),
            "https://gitlab.example/acme/nodes.git (ref: main)"
        );
        let fs = RepoSource::Fs("/srv/nodes".into());
        assert_eq!(shown_label(&fs, &PeppyBuild::Unreleased), "/srv/nodes");
    }
    use crate::services::node::test_git_daemon::GitDaemon;
    use crate::services::repo::cache::test_support::{fs_repos, write_excluded_repos, write_repos};
    use crate::services::repo::cache::{
        EntryOrigin, LauncherCacheEntry, NodeCacheEntry, repositories_list_path,
    };
    use crate::services::repo::index::test_support::{
        write_contract_json5, write_launcher_json5, write_node_json5,
    };
    use config::consts::NODE_CONFIG_FILE;

    /// A fixed instant for every refresh under test, so nothing depends
    /// on the host clock or on how fast the test runs.
    const TEST_NOW: SystemTime = SystemTime::UNIX_EPOCH;

    #[test]
    fn read_or_create_repos_creates_file_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repos_path = repositories_list_path(&peppy_dirs);
        assert!(!repos_path.exists());

        let repos = read_or_create_repos(&peppy_dirs).unwrap();
        assert!(repos_path.exists(), "repositories.json5 should be created");

        // Don't pin the exact count; defaults grow over time. Just
        // assert that the well-known entries shipped in the template are
        // present with their expected shape.
        let by_id = |id: u64| -> &Value {
            repos
                .iter()
                .find(|r| r.get("id").and_then(|v| v.as_u64()) == Some(id))
                .unwrap_or_else(|| panic!("default repos should include id {id}"))
        };

        let nodes_hub = by_id(1000);
        assert_eq!(nodes_hub.get("type").unwrap().as_str().unwrap(), "git");
        assert_eq!(
            nodes_hub.get("url").unwrap().as_str().unwrap(),
            "https://github.com/Peppy-bot/nodes-hub.git"
        );
        assert_eq!(
            nodes_hub.get("ref").unwrap().as_str().unwrap(),
            "@{peppy-release}"
        );

        let launchers_hub = by_id(1001);
        assert_eq!(launchers_hub.get("type").unwrap().as_str().unwrap(), "git");
        assert_eq!(
            launchers_hub.get("url").unwrap().as_str().unwrap(),
            "https://github.com/Peppy-bot/launchers-hub.git"
        );
    }

    #[test]
    fn read_or_create_repos_reads_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join("repositories.json5"),
            r#"[{ "id": 1, "type": "fs", "path": "/custom/path" }]"#,
        )
        .unwrap();

        let repos = read_or_create_repos(&peppy_dirs).unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(
            repos[0].get("path").unwrap().as_str().unwrap(),
            "/custom/path"
        );
    }

    #[test]
    fn read_or_create_repos_subsequent_call_uses_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        // First call creates the file
        let _ = read_or_create_repos(&peppy_dirs).unwrap();

        // Overwrite with custom content
        let repos_path = repositories_list_path(&peppy_dirs);
        std::fs::write(
            &repos_path,
            r#"[{ "id": 1, "type": "fs", "path": "/other" }]"#,
        )
        .unwrap();

        // Second call should read the overwritten file, not re-create defaults
        let repos = read_or_create_repos(&peppy_dirs).unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].get("path").unwrap().as_str().unwrap(), "/other");
    }

    #[test]
    fn read_or_create_repos_rejects_duplicate_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join("repositories.json5"),
            r#"[
                { "id": 1, "type": "fs", "path": "/a" },
                { "id": 1, "type": "fs", "path": "/b" }
            ]"#,
        )
        .unwrap();

        let err = read_or_create_repos(&peppy_dirs).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("duplicate repository id 1"),
            "error should mention the duplicate id, got: {msg}"
        );
    }

    #[test]
    fn read_or_create_repos_auto_assigns_missing_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join("repositories.json5"),
            r#"[
                { "type": "fs", "path": "/no-id-a" },
                { "id": 5, "type": "fs", "path": "/has-id" },
                { "type": "fs", "path": "/no-id-b" }
            ]"#,
        )
        .unwrap();

        let repos = read_or_create_repos(&peppy_dirs).unwrap();
        assert_eq!(repos.len(), 3);

        // All entries should have ids and be sorted by id
        let ids: Vec<u64> = repos
            .iter()
            .map(|e| e.get("id").unwrap().as_u64().unwrap())
            .collect();
        assert_eq!(ids[0], 5, "explicit id 5 should be present");
        assert_eq!(ids[1], 6, "first missing id should be auto-assigned 6");
        assert_eq!(ids[2], 7, "second missing id should be auto-assigned 7");
    }

    /// Writes the index a repository publishes, the way `peppy repo index`
    /// does.
    ///
    /// A repository states what it holds by committing this file, so a test
    /// that writes a tree has to publish it before a refresh can read it.
    fn publish_repo(root: &Path) {
        crate::services::repo::index::publish_repository_index(root)
            .expect("a well-formed test repository can be published");
    }

    /// Publishes `root`'s index and commits it alongside `files`, returning
    /// the branch HEAD is on.
    ///
    /// A remote repository is read from a clone, so what it publishes has to
    /// be committed, not merely written.
    fn publish_and_commit(repo: &git2::Repository, root: &Path, files: &[&str]) -> String {
        publish_repo(root);

        let mut index = repo.index().expect("open index");
        for file in files {
            index
                .add_path(Path::new(file))
                .unwrap_or_else(|e| panic!("stage {file}: {e}"));
        }
        index
            .add_path(Path::new(daemon_config::consts::REPOSITORY_INDEX_FILE))
            .expect("stage the repository index");
        index.write().expect("write index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        let signature =
            git2::Signature::now("Peppy", "peppy@example.com").expect("create signature");
        let parents: Vec<git2::Commit> = repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .map(|oid| vec![repo.find_commit(oid).expect("find parent commit")])
            .unwrap_or_default();
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "publish",
            &tree,
            &parent_refs,
        )
        .expect("commit");

        repo.head()
            .expect("head")
            .shorthand()
            .expect("shorthand")
            .to_owned()
    }

    /// Publishes `root`, then removes the file its index names for
    /// `removed_rel`, leaving a repository that reads fine and states
    /// something untrue about itself.
    ///
    /// An index cannot claim an identity twice, so a broken repository in
    /// these tests is one whose statement and tree disagree.
    fn stale_index(root: &Path, removed_rel: &str) {
        publish_repo(root);
        std::fs::remove_file(root.join(removed_rel))
            .unwrap_or_else(|e| panic!("remove {removed_rel}: {e}"));
    }

    /// Refreshes every configured repository as this binary reads it.
    fn refresh_every(
        peppy_dirs: &PeppyDirs,
        now: SystemTime,
        on_feedback: &mut dyn FnMut(RepoRefreshFeedback),
    ) -> Result<RefreshedRepos> {
        process_refresh(
            peppy_dirs,
            &peppy_build(),
            &RefreshScope::Every,
            now,
            on_feedback,
            &CancellationToken::new(),
        )
        .map(|refreshed| refreshed.expect("nothing cancels the refresh"))
    }

    /// Publishes only `roots`, then refreshes. For tests that deliberately
    /// leave a repository broken and must not have it re-published.
    fn refresh_publishing(
        peppy_dirs: &PeppyDirs,
        roots: &[&Path],
        now: SystemTime,
        on_feedback: &mut dyn FnMut(RepoRefreshFeedback),
    ) -> Result<RefreshedRepos> {
        for root in roots {
            publish_repo(root);
        }
        refresh_every(peppy_dirs, now, on_feedback)
    }

    /// Publishes every configured fs repository, then refreshes.
    ///
    /// Keeps each test about the case it is testing rather than about
    /// re-stating the publish step.
    fn refresh_indexed(
        peppy_dirs: &PeppyDirs,
        now: SystemTime,
        on_feedback: &mut dyn FnMut(RepoRefreshFeedback),
    ) -> Result<RefreshedRepos> {
        for entry in read_or_create_repos(peppy_dirs)? {
            if let Ok(RepoSource::Fs(path)) = parse_repo_entry(&entry)
                && path.exists()
            {
                publish_repo(&path);
            }
        }
        refresh_every(peppy_dirs, now, on_feedback)
    }

    /// The load-bearing case: a failure is scoped to the repository that
    /// caused it. The healthy repository picks up its change, the broken
    /// one keeps the entries it last published, and only the broken one
    /// is named.
    #[test]
    fn process_refresh_contains_a_failure_to_its_own_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let healthy = tmp.path().join("healthy");
        let broken = tmp.path().join("broken");
        write_node_json5(&healthy.join("first"), "first", "v1");
        write_node_json5(&broken.join("kept"), "kept", "v1");
        write_repos(&peppy_dirs, &fs_repos(&[(1, &healthy), (2, &broken)]));

        // A clean run first, so the broken repository has something to
        // fall back to.
        let clean = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {}).unwrap();
        assert!(clean.failures.is_empty());
        write_all_caches(&peppy_dirs, &clean).unwrap();

        // Now break the second repository and add a node to the first.
        write_node_json5(&healthy.join("second"), "second", "v1");
        write_node_json5(&broken.join("gone"), "gone", "v1");
        stale_index(&broken, "gone/peppy.json5");

        let refreshed =
            refresh_publishing(&peppy_dirs, &[&healthy], TEST_NOW, &mut |_| {}).unwrap();

        let names: HashSet<&str> = refreshed
            .items
            .nodes
            .iter()
            .map(|n| n.node_name.as_str())
            .collect();
        assert!(
            names.contains("first") && names.contains("second"),
            "the healthy repository still picked up its change: {names:?}"
        );
        assert!(
            names.contains("kept"),
            "the broken repository kept its previous entries: {names:?}"
        );
        assert!(
            !names.contains("gone"),
            "an identity the repository states but cannot produce is not published: {names:?}"
        );

        assert_eq!(refreshed.failures.len(), 1, "only the broken repo failed");
        let failure = &refreshed.failures[0];
        assert_eq!(failure.id, 2);
        assert_eq!(failure.read_failure, RepoReadFailure::Conflict);
        assert_eq!(failure.retained, 1, "one node kept from the last read");
        assert!(failure.detail.contains("gone:v1"), "{}", failure.detail);
        assert!(
            failure.detail.contains("gone/peppy.json5"),
            "{}",
            failure.detail
        );
    }

    /// A re-index that reads every repository cleanly has nothing to add
    /// to the caller's response, and publishes the caches that make the
    /// caller's edit take effect.
    #[tokio::test]
    async fn reindex_after_change_reports_nothing_when_the_re_read_is_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo = tmp.path().join("repo");
        write_node_json5(&repo.join("first"), "first", "v1");
        write_repos(&peppy_dirs, &fs_repos(&[(1, &repo)]));
        publish_repo(&repo);

        assert_eq!(reindex_after_change(&peppy_dirs).await, None);

        let cached = crate::services::repo::cache::load_repo_cache::<NodeCacheEntry>(&peppy_dirs)
            .expect("the re-index published the node cache");
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].node_name, "first");
    }

    /// A refresh whose cancel came before its reads reads no repository and
    /// publishes nothing: the caches keep what they held, here none.
    #[tokio::test]
    async fn a_refresh_cancelled_before_its_reads_reads_nothing_and_publishes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let repo = tmp.path().join("repo");
        write_node_json5(&repo.join("first"), "first", "v1");
        write_repos(&peppy_dirs, &fs_repos(&[(1, &repo)]));
        publish_repo(&repo);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let mut feedback = Vec::new();
        let read = process_refresh(
            &peppy_dirs,
            &peppy_build(),
            &RefreshScope::Every,
            TEST_NOW,
            &mut |line| feedback.push(line),
            &cancel,
        )
        .expect("the refresh runs");
        assert!(read.is_none(), "the cancel stops the refresh");
        assert!(feedback.is_empty(), "no repository is read: {feedback:?}");

        let published = refresh_and_write_caches(
            &peppy_dirs,
            peppy_build(),
            RefreshScope::Every,
            TEST_NOW,
            |_| {},
            &cancel,
        )
        .await
        .expect("the refresh runs");
        assert!(published.is_none(), "the cancel stops the refresh");
        assert!(
            crate::services::repo::cache::load_repo_cache::<NodeCacheEntry>(&peppy_dirs)
                .unwrap_or_default()
                .is_empty(),
            "a cancelled refresh publishes no cache"
        );
    }

    /// A re-index that cannot read the configuration at all reports it:
    /// the caller just changed that configuration, so this belongs in
    /// their response rather than in a log nobody reads.
    #[tokio::test]
    async fn reindex_after_change_reports_a_re_read_that_failed_outright() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        write_repos(
            &peppy_dirs,
            r#"[
                { "id": 1, "type": "fs", "path": "/a" },
                { "id": 1, "type": "fs", "path": "/b" }
            ]"#,
        );

        let report = reindex_after_change(&peppy_dirs)
            .await
            .expect("a re-read that failed outright is reported");
        assert!(report.starts_with("re-indexing failed:"), "got: {report}");
        assert!(
            report.contains("duplicate repository id 1"),
            "the report names what to fix, got: {report}"
        );
    }

    /// The report reads as prose while still carrying the machine value,
    /// so an operator can act on it and a tool can match on it.
    #[test]
    fn failure_report_reads_as_a_sentence_and_names_the_kind() {
        let unreachable = RepoFailure {
            id: 1002,
            label: "https://example.com/hub.git (ref: main)".to_owned(),
            read_failure: RepoReadFailure::Unreachable,
            detail: "failed to connect".to_owned(),
            retained: 8,
        };
        let conflict = RepoFailure {
            id: 1000,
            label: "/home/user/workspace".to_owned(),
            read_failure: RepoReadFailure::Conflict,
            detail: "2 node manifests claim `a:v1`".to_owned(),
            retained: 0,
        };

        let report = failure_report(&[conflict, unreachable]);

        assert!(
            report.contains("could not be read [unreachable]"),
            "{report}"
        );
        assert!(report.contains("contradicts itself [conflict]"), "{report}");
        assert!(
            report.contains("Kept 8 entries from its last successful read"),
            "{report}"
        );
        assert!(
            report.contains("contributes nothing this time"),
            "a machine with nothing to fall back to says so: {report}"
        );
        assert!(
            report.contains("Every other repository was updated normally"),
            "{report}"
        );
    }

    /// A failing repository keeps the timestamp of its last successful
    /// read rather than being stamped with now: the retained entries are
    /// still the ones read at that time, and restamping them would claim
    /// they are current.
    #[test]
    fn process_refresh_carries_forward_the_last_successful_read_time() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let repo = tmp.path().join("repo");
        write_node_json5(&repo.join("node_a"), "node_a", "v1");
        write_repos(&peppy_dirs, &fs_repos(&[(1, &repo)]));

        let first_read = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let clean = refresh_indexed(&peppy_dirs, first_read, &mut |_| {}).unwrap();
        write_all_caches(&peppy_dirs, &clean).unwrap();
        assert_eq!(clean.statuses.len(), 1);
        assert_eq!(clean.statuses[0].last_read_unix_secs, Some(1_000));
        assert!(!clean.statuses[0].is_retained());

        // Break it, and refresh much later.
        write_node_json5(&repo.join("vanished"), "vanished", "v1");
        stale_index(&repo, "vanished/peppy.json5");
        let later = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(99_000);
        let failed = refresh_publishing(&peppy_dirs, &[], later, &mut |_| {}).unwrap();

        let status = &failed.statuses[0];
        assert_eq!(
            status.last_read_unix_secs,
            Some(1_000),
            "the entries still date from the last clean read"
        );
        assert!(status.is_retained());
        let failure = status.last_failure.as_ref().expect("failure recorded");
        assert_eq!(failure.read_failure, "conflict");
        assert_eq!(failure.unix_secs, 99_000, "the failure itself is recent");
    }

    /// A repository that recovers stops reporting its old failure, so a
    /// fixed problem does not linger in the diagnostics.
    #[test]
    fn process_refresh_clears_the_failure_once_a_repository_recovers() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let repo = tmp.path().join("repo");
        write_node_json5(&repo.join("vanished"), "vanished", "v1");
        write_repos(&peppy_dirs, &fs_repos(&[(1, &repo)]));
        stale_index(&repo, "vanished/peppy.json5");

        let broken = refresh_publishing(&peppy_dirs, &[], TEST_NOW, &mut |_| {}).unwrap();
        write_all_caches(&peppy_dirs, &broken).unwrap();
        assert!(broken.statuses[0].is_retained());

        // Re-publishing states what the repository actually holds again.
        let fixed = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {}).unwrap();

        assert!(fixed.failures.is_empty());
        assert!(!fixed.statuses[0].is_retained());
        assert!(fixed.statuses[0].last_failure.is_none());
    }

    /// A repository that cannot be reached at all is reported as
    /// unreachable, not as broken content: an outage and a content bug
    /// send the user to completely different places.
    #[test]
    fn process_refresh_reports_a_missing_fs_repo_as_unreachable() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let healthy = tmp.path().join("healthy");
        write_node_json5(&healthy.join("node_a"), "node_a", "v1");
        let gone = tmp.path().join("not-mounted");
        write_repos(&peppy_dirs, &fs_repos(&[(1, &healthy), (2, &gone)]));

        let refreshed = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {}).unwrap();

        assert_eq!(
            refreshed.items.nodes.len(),
            1,
            "the healthy repository updated"
        );
        assert_eq!(refreshed.failures.len(), 1);
        assert_eq!(
            refreshed.failures[0].read_failure,
            RepoReadFailure::Unreachable
        );
        assert_eq!(
            refreshed.failures[0].retained, 0,
            "nothing was ever read from it"
        );
    }

    /// An unrecognized `repositories.json5` entry is a failure rather
    /// than a silent skip: a typo in the configuration should not look
    /// like a repository that simply has no content.
    #[test]
    fn process_refresh_reports_an_unrecognized_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        write_repos(&peppy_dirs, r#"[{ "id": 1, "type": "nonsense" }]"#);

        let refreshed = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {}).unwrap();

        assert_eq!(refreshed.failures.len(), 1);
        assert_eq!(
            refreshed.failures[0].read_failure,
            RepoReadFailure::Unreachable
        );
    }

    /// Several failures across several repositories come back in one
    /// pass, ordered by repository id, so one run tells the user
    /// everything they have to fix.
    #[test]
    fn process_refresh_reports_every_failure_in_repository_id_order() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let conflicted = tmp.path().join("conflicted");
        write_node_json5(&conflicted.join("a"), "vanished", "v1");
        stale_index(&conflicted, "a/peppy.json5");
        let gone = tmp.path().join("not-mounted");
        write_repos(&peppy_dirs, &fs_repos(&[(5, &gone), (9, &conflicted)]));

        let refreshed = refresh_publishing(&peppy_dirs, &[], TEST_NOW, &mut |_| {}).unwrap();

        let seen: Vec<(u64, RepoReadFailure)> = refreshed
            .failures
            .iter()
            .map(|f| (f.id, f.read_failure))
            .collect();
        assert_eq!(
            seen,
            vec![
                (5, RepoReadFailure::Unreachable),
                (9, RepoReadFailure::Conflict)
            ]
        );

        let report = failure_report(&refreshed.failures);
        assert!(report.contains("unreachable"), "{report}");
        assert!(report.contains("conflict"), "{report}");
    }

    /// Repositories of every kind, read in parallel, come back in id order:
    /// the failures, the status lines, the merged entries and the discovery
    /// feedback, whichever read ends first.
    #[test]
    fn process_refresh_returns_repositories_of_every_kind_in_id_order() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let first = tmp.path().join("first");
        write_node_json5(&first.join("first"), "first", "v1");
        let git_src = tmp.path().join("git_src");
        let repo = git2::Repository::init(&git_src).expect("init repo");
        write_node_json5(&git_src.join("from_git"), "from_git", "v1");
        let branch = publish_and_commit(&repo, &git_src, &["from_git/peppy.json5"]);
        let gone = tmp.path().join("not-mounted");
        let last = tmp.path().join("last");
        write_node_json5(&last.join("last"), "last", "v1");
        let git_url = format!("file://{}", git_src.display());
        let repos = serde_json::json!([
            { "id": 1, "type": "fs", "path": first },
            { "id": 2, "type": "nonsense" },
            { "id": 3, "type": "git", "url": git_url, "ref": branch },
            { "id": 4, "type": "fs", "path": gone },
            { "id": 5, "type": "fs", "path": last },
        ]);
        write_repos(&peppy_dirs, &repos.to_string());

        let mut discovered = Vec::new();
        let refreshed = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |feedback| {
            if let RepoRefreshFeedback::Discovered { item_name, .. } = feedback {
                discovered.push(item_name);
            }
        })
        .unwrap();

        let failures: Vec<(u64, RepoReadFailure)> = refreshed
            .failures
            .iter()
            .map(|f| (f.id, f.read_failure))
            .collect();
        assert_eq!(
            failures,
            vec![
                (2, RepoReadFailure::Unreachable),
                (4, RepoReadFailure::Unreachable)
            ]
        );
        let status_ids: Vec<u64> = refreshed.statuses.iter().map(|s| s.id).collect();
        assert_eq!(
            status_ids,
            vec![1, 3, 4, 5],
            "an unparseable entry has none"
        );
        let nodes: Vec<&str> = refreshed
            .items
            .nodes
            .iter()
            .map(|n| n.node_name.as_str())
            .collect();
        assert_eq!(nodes, vec!["first", "from_git", "last"]);
        assert_eq!(discovered, vec!["first", "from_git", "last"]);
    }

    /// Retention follows the same attribution rule as lookup, so a
    /// retained entry keeps exactly the priority it had and a failing
    /// repository never inherits another repository's entries.
    #[test]
    fn process_refresh_retains_only_the_failed_repositorys_own_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let one = tmp.path().join("one");
        let two = tmp.path().join("two");
        write_node_json5(&one.join("from_one"), "from_one", "v1");
        write_node_json5(&two.join("from_two"), "from_two", "v1");
        write_repos(&peppy_dirs, &fs_repos(&[(1, &one), (2, &two)]));
        let clean = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {}).unwrap();
        write_all_caches(&peppy_dirs, &clean).unwrap();

        // Break repository 2 only.
        write_node_json5(&two.join("vanished"), "vanished", "v1");
        stale_index(&two, "vanished/peppy.json5");
        let refreshed = refresh_publishing(&peppy_dirs, &[&one], TEST_NOW, &mut |_| {}).unwrap();

        assert_eq!(refreshed.failures.len(), 1);
        assert_eq!(
            refreshed.failures[0].retained, 1,
            "only `from_two` is retained, not `from_one`"
        );
        // Node paths are stored canonicalized, so compare against the
        // canonical form of the repo root (on macOS the tempdir `two` is a
        // `/var` symlink to `/private/var`).
        let two_root = std::fs::canonicalize(&two).unwrap();
        let retained: Vec<&str> = refreshed
            .items
            .nodes
            .iter()
            .filter(|n| {
                n.origin
                    .path_str()
                    .starts_with(two_root.to_string_lossy().as_ref())
            })
            .map(|n| n.node_name.as_str())
            .collect();
        assert_eq!(retained, vec!["from_two"]);
    }

    #[test]
    fn process_refresh_skips_excluded_fs_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo_a = tmp.path().join("repo_a");
        let repo_b = tmp.path().join("repo_b");
        write_node_json5(&repo_a.join("node_a"), "node_a", "v1");
        write_node_json5(&repo_b.join("node_b"), "node_b", "v1");

        write_repos(&peppy_dirs, &fs_repos(&[(1, &repo_a), (2, &repo_b)]));
        write_excluded_repos(&peppy_dirs, &fs_repos(&[(1, &repo_b)]));

        let RefreshedRepos {
            items, excluded, ..
        } = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {}).unwrap();
        let discovered = items.nodes;
        assert_eq!(discovered.len(), 1, "only non-excluded repo nodes returned");
        assert_eq!(discovered[0].node_name, "node_a");
        assert_eq!(excluded.len(), 1, "one repo should be excluded");
        assert_eq!(excluded[0].source_type, RepoSourceKind::Fs);
        // `identity` is canonicalized by `entry_identity`; the test
        // path is not, so compare against the canonical form to stay
        // robust on platforms with symlinked tempdirs (e.g. macOS's
        // `/var` → `/private/var`).
        assert_eq!(
            excluded[0].identity,
            std::fs::canonicalize(&repo_b)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        );
    }

    #[test]
    fn process_refresh_excludes_fs_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo = tmp.path().join("repo");
        write_node_json5(&repo.join("keep_node"), "keep_node", "v1");
        write_node_json5(&repo.join("secret_node"), "secret_node", "v1");

        write_repos(&peppy_dirs, &fs_repos(&[(1, &repo)]));
        // Exclude the subdirectory, not the whole repo
        write_excluded_repos(&peppy_dirs, &fs_repos(&[(1, &repo.join("secret_node"))]));

        let RefreshedRepos {
            items, excluded, ..
        } = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {}).unwrap();
        let discovered = items.nodes;
        assert_eq!(
            discovered.len(),
            1,
            "only the non-excluded subdirectory node should be found"
        );
        assert_eq!(discovered[0].node_name, "keep_node");
        // The subdirectory exclusion should still appear in the excluded list
        // so that it is reported as feedback to the user.
        assert_eq!(
            excluded.len(),
            1,
            "subdirectory exclusion should be reported"
        );
        assert_eq!(excluded[0].source_type, RepoSourceKind::Fs);
        assert!(
            excluded[0].identity.contains("secret_node"),
            "excluded identity should reference the subdirectory"
        );
    }

    /// An excluded subtree is one this machine does not answer for, so its
    /// files are never opened. That is also what keeps a broken manifest
    /// inside it from failing the repository the user did ask for. The
    /// index still declares the item; only the read is skipped.
    #[test]
    fn process_refresh_does_not_read_an_excluded_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo = tmp.path().join("repo");
        write_node_json5(&repo.join("keep_node"), "keep_node", "v1");
        write_node_json5(&repo.join("secret_node"), "secret_node", "v1");
        // Published first, so the index declares both, then the excluded
        // one is made unreadable behind peppy's back.
        publish_repo(&repo);
        std::fs::remove_file(repo.join("secret_node").join(NODE_CONFIG_FILE))
            .expect("remove the excluded manifest");

        write_repos(&peppy_dirs, &fs_repos(&[(1, &repo)]));
        write_excluded_repos(&peppy_dirs, &fs_repos(&[(1, &repo.join("secret_node"))]));

        let RefreshedRepos {
            items, failures, ..
        } = refresh_every(&peppy_dirs, TEST_NOW, &mut |_| {}).unwrap();
        let discovered = items.nodes;
        assert!(
            failures.is_empty(),
            "an item the machine does not serve cannot break the read: {failures:?}"
        );
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].node_name, "keep_node");
    }

    /// The clone `repo refresh` pays for is the tree every item it read
    /// resolves to, so it becomes the cached checkout instead of being
    /// deleted and downloaded again on first use.
    #[test]
    fn process_refresh_hands_its_clone_to_the_checkout_cache() {
        let src_tmp = tempfile::tempdir().unwrap();
        let src = src_tmp.path();
        let repo = git2::Repository::init(src).expect("init repo");
        write_node_json5(&src.join("arm"), "arm", "v1");
        let branch = publish_and_commit(&repo, src, &["arm/peppy.json5"]);

        let peppy_tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(peppy_tmp.path());
        let repo_url = format!("file://{}", src.display());
        write_repos(
            &peppy_dirs,
            &format!(r#"[{{ "id": 1, "type": "git", "url": "{repo_url}", "ref": "{branch}" }}]"#),
        );

        let nodes = refresh_every(&peppy_dirs, TEST_NOW, &mut |_| {})
            .unwrap()
            .items
            .nodes;
        let (url, commit) = nodes[0].origin.checkout().expect("a git origin");

        let checkout =
            crate::services::node::cache::git::checkout_dir_for(&peppy_dirs, url, commit);
        assert!(
            checkout.join("arm").join(NODE_CONFIG_FILE).exists(),
            "the refresh clone is now the cached checkout at {}",
            checkout.display()
        );
    }

    #[test]
    fn process_refresh_skips_excluded_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo = tmp.path().join("repo");
        write_node_json5(&repo.join("node_a"), "node_a", "v1");

        write_repos(
            &peppy_dirs,
            &format!(
                r#"[
                    {{ "id": 1, "type": "fs", "path": "{}" }},
                    {{ "id": 2, "type": "git", "url": "https://example.com/repo.git" }}
                ]"#,
                repo.display()
            ),
        );
        write_excluded_repos(
            &peppy_dirs,
            r#"[{ "id": 1, "type": "git", "url": "https://example.com/repo.git" }]"#,
        );

        let RefreshedRepos {
            items, excluded, ..
        } = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {}).unwrap();
        let discovered = items.nodes;
        assert_eq!(discovered.len(), 1, "FS node should still be found");
        assert_eq!(discovered[0].node_name, "node_a");
        assert_eq!(excluded.len(), 1, "git repo should be excluded");
        assert_eq!(excluded[0].source_type, RepoSourceKind::Git);
        assert_eq!(excluded[0].identity, "https://example.com/repo.git");
    }

    #[test]
    fn process_refresh_no_exclusion_file() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo = tmp.path().join("repo");
        write_node_json5(&repo.join("node_a"), "node_a", "v1");

        write_repos(&peppy_dirs, &fs_repos(&[(1, &repo)]));

        // No excluded_repositories.json5 file
        let RefreshedRepos {
            items, excluded, ..
        } = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {}).unwrap();
        let discovered = items.nodes;
        assert_eq!(discovered.len(), 1, "node should be found normally");
        assert!(excluded.is_empty(), "no repos should be excluded");
    }

    #[test]
    fn process_refresh_discovers_contracts_from_fs_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo = tmp.path().join("repo");
        let iface_path = repo.join("uvc_camera/peppy.json5");
        let bytes = write_contract_json5(&iface_path, "uvc_camera", "v1");

        write_repos(&peppy_dirs, &fs_repos(&[(1, &repo)]));

        let contracts = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {})
            .unwrap()
            .items
            .contracts;
        assert_eq!(contracts.len(), 1, "exactly one contract expected");
        let iface = &contracts[0];
        assert_eq!(iface.contract_name, "uvc_camera");
        assert_eq!(iface.tag, "v1");
        assert_eq!(iface.origin.kind(), RepoSourceKind::Fs);
        assert!(
            iface.origin.path_str().ends_with("uvc_camera/peppy.json5"),
            "fs path should be absolute to the manifest file: {}",
            iface.origin.path_str()
        );
        assert_eq!(
            iface.sha256,
            daemon_config::repository::ManifestFingerprint::of_bytes(&bytes),
            "cached sha256 must equal fingerprint_for_bytes of raw manifest bytes"
        );
    }

    /// Git-side contract discovery: the cached `path` is relative to
    /// the repo root, and `repo_ref` records the branch that was
    /// cloned.
    #[test]
    fn process_refresh_discovers_contracts_from_git_repo() {
        let src_tmp = tempfile::tempdir().unwrap();
        let src = src_tmp.path();
        let repo = git2::Repository::init(src).expect("init repo");
        let iface_rel = Path::new("uvc_camera/peppy.json5");
        write_contract_json5(&src.join(iface_rel), "uvc_camera", "v1");
        let branch = publish_and_commit(&repo, src, &["uvc_camera/peppy.json5"]);

        let peppy_tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(peppy_tmp.path());
        let repo_url = format!("file://{}", src.display());
        write_repos(
            &peppy_dirs,
            &format!(r#"[{{ "id": 1, "type": "git", "url": "{repo_url}", "ref": "{branch}" }}]"#,),
        );

        let contracts = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {})
            .unwrap()
            .items
            .contracts;
        assert_eq!(contracts.len(), 1, "exactly one contract expected");
        let iface = &contracts[0];
        assert_eq!(iface.contract_name, "uvc_camera");
        assert_eq!(iface.tag, "v1");
        assert_eq!(iface.origin.kind(), RepoSourceKind::Git);
        assert_eq!(iface.origin.path_str(), "uvc_camera/peppy.json5");
        assert_eq!(iface.origin.repo_ref(), Some(branch.as_str()));
        assert!(
            iface.sha256.as_str().len() == 64,
            "sha256 should be populated from the manifest file bytes"
        );
    }

    /// Two repositories ship `uvc_camera@0.1.0` with different content;
    /// both entries are kept in the cache, with `sha256` letting the user
    /// pick one. Feedback fires only once for the higher-priority repo.
    #[test]
    fn process_refresh_keeps_same_name_tag_with_different_sha256() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo_a = tmp.path().join("repo_a");
        let repo_b = tmp.path().join("repo_b");
        // Same name/tag, different bodies (whitespace differs) produce
        // distinct sha256 fingerprints over the raw bytes.
        std::fs::create_dir_all(repo_a.join("uvc_camera")).unwrap();
        std::fs::write(
            repo_a.join("uvc_camera/peppy.json5"),
            r#"{
  peppy_schema: "contract/v1",
  manifest: { name: "uvc_camera", tag: "v1" },
  interfaces: {}
}"#,
        )
        .unwrap();
        std::fs::create_dir_all(repo_b.join("uvc_camera")).unwrap();
        std::fs::write(
            repo_b.join("uvc_camera/peppy.json5"),
            r#"{
  // Same identity, different content fingerprint via extra whitespace.
  peppy_schema: "contract/v1",
  manifest:    { name: "uvc_camera", tag: "v1" },
  interfaces:  {}
}"#,
        )
        .unwrap();

        write_repos(&peppy_dirs, &fs_repos(&[(1, &repo_a), (2, &repo_b)]));

        let mut feedbacks = Vec::new();
        let contracts = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |fb| feedbacks.push(fb))
            .unwrap()
            .items
            .contracts;

        assert_eq!(
            contracts.len(),
            2,
            "both entries should be kept (sha256 disambiguates)"
        );
        let shas: HashSet<&str> = contracts.iter().map(|i| i.sha256.as_str()).collect();
        assert_eq!(
            shas.len(),
            2,
            "the two manifest bodies must produce distinct sha256s"
        );

        // Only one discovery feedback for this (name, tag): the
        // higher-priority repo (lower id) wins.
        let discovered_paths: Vec<&str> = feedbacks
            .iter()
            .filter_map(|f| match f {
                RepoRefreshFeedback::Discovered {
                    item_name, path, ..
                } if item_name == "uvc_camera" => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            discovered_paths.len(),
            1,
            "feedback fires once per unique (name, tag)"
        );
        assert!(
            discovered_paths[0].contains("repo_a"),
            "first listed repo should win the feedback: {}",
            discovered_paths[0]
        );
    }

    /// Process_refresh discovers launcher files (any `.json5` filename
    /// with `peppy_schema: "launcher/v1"`) alongside node files in the
    /// same FS walk, names them by basename, and dedupes across repos.
    #[test]
    fn process_refresh_discovers_launchers_from_fs_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo_a = tmp.path().join("repo_a");
        let repo_b = tmp.path().join("repo_b");
        // Three launchers with arbitrary names in repo_a; one collides
        // with a launcher named the same in repo_b.
        write_launcher_json5(&repo_a.join("openarm01_sim_teleop.json5"));
        write_launcher_json5(&repo_a.join("calibration.json5"));
        write_launcher_json5(&repo_a.join("nested").join("demo.json5"));
        write_launcher_json5(&repo_b.join("openarm01_sim_teleop.json5"));
        // Throw a node into repo_a too so we exercise the mixed walk.
        write_node_json5(&repo_a.join("node_a"), "node_a", "v1");

        write_repos(&peppy_dirs, &fs_repos(&[(1, &repo_a), (2, &repo_b)]));

        let items = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {})
            .unwrap()
            .items;
        let discovered = items.nodes;
        let launchers = items.launchers;
        assert_eq!(discovered.len(), 1, "node_a should be the only node");
        assert_eq!(
            launchers.len(),
            4,
            "every launcher is kept (same-name entries from different repos coexist)"
        );

        // The unique set is computed by name; lookup picks the
        // highest-priority repo when several declare the same name.
        let unique_by_name: std::collections::HashMap<&str, &LauncherCacheEntry> = launchers
            .iter()
            .map(|l| (l.launcher_name.as_str(), l))
            .collect();
        let mut unique_names: Vec<&str> = unique_by_name.keys().copied().collect();
        unique_names.sort_unstable();
        assert_eq!(
            unique_names,
            vec!["calibration", "demo", "openarm01_sim_teleop"]
        );

        // Path points at the `.json5` file itself, not its parent
        // directory, so downstream code can read it directly.
        let demo = unique_by_name.get("demo").expect("demo launcher");
        assert!(
            demo.origin.path_str().ends_with("demo.json5"),
            "launcher path should be the .json5 file itself: {}",
            demo.origin.path_str()
        );

        // Both `openarm01_sim_teleop` entries are present; the
        // repo_b one is the second occurrence.
        let dup: Vec<&LauncherCacheEntry> = launchers
            .iter()
            .filter(|l| l.launcher_name == "openarm01_sim_teleop")
            .collect();
        assert_eq!(dup.len(), 2);
        assert!(
            dup.iter().any(|l| l.origin.path_str().contains("repo_a")),
            "primary entry should be from repo_a"
        );
        assert!(
            dup.iter().any(|l| l.origin.path_str().contains("repo_b")),
            "secondary entry should be from repo_b"
        );
    }

    /// `.json5` files that don't declare `peppy_schema: "launcher/v1"`
    /// must be skipped silently; they're unrelated configuration, not
    /// launchers.
    #[test]
    fn process_refresh_ignores_non_launcher_json5_files() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        // Real launcher.
        write_launcher_json5(&repo.join("real_launcher.json5"));
        // Random JSON5 file (not a launcher schema).
        std::fs::write(
            repo.join("settings.json5"),
            r#"{ theme: "dark", verbose: true }"#,
        )
        .unwrap();
        // Another node config in the wrong shape; this lives elsewhere
        // in the repo and must not be misclassified.
        std::fs::write(
            repo.join("manifest.json5"),
            r#"{ peppy_schema: "node/v1", manifest: { name: "x", tag: "v1" }, interfaces: {}, execution: { language: "rust", build_cmd: ["true"], run_cmd: ["true"] } }"#,
        )
        .unwrap();

        write_repos(&peppy_dirs, &fs_repos(&[(1, &repo)]));

        let launchers = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {})
            .unwrap()
            .items
            .launchers;
        assert_eq!(
            launchers.len(),
            1,
            "only the real launcher should be cached"
        );
        assert_eq!(launchers[0].launcher_name, "real_launcher");
    }

    /// The launcher cache is written to disk by the refresh handler so
    /// downstream lookups can resolve launchers by name.
    #[test]
    fn process_refresh_writes_launcher_cache() {
        use crate::services::repo::cache::launchers_repo_cache_path;

        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo = tmp.path().join("repo");
        write_launcher_json5(&repo.join("openarm01_sim_teleop.json5"));

        write_repos(&peppy_dirs, &fs_repos(&[(1, &repo)]));

        let launchers = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {})
            .unwrap()
            .items
            .launchers;
        write_repo_cache(&peppy_dirs, &launchers).unwrap();

        let cache_path = launchers_repo_cache_path(&peppy_dirs);
        assert!(cache_path.exists(), "launcher cache should be written");

        let raw = std::fs::read_to_string(&cache_path).expect("read launcher cache");
        let parsed: serde_json::Value =
            serde_json5::from_str(&raw).expect("launcher cache should be valid JSON5");
        let arr = parsed.as_array().expect("expected JSON array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["launcher_name"], "openarm01_sim_teleop");
        assert_eq!(arr[0]["origin"]["source_type"], "fs");
        let path_str = arr[0]["origin"]["path"]
            .as_str()
            .expect("path should be a string");
        assert!(
            path_str.ends_with("openarm01_sim_teleop.json5"),
            "cached path should point at the .json5 file: {path_str}"
        );
    }

    /// End-to-end coverage of the launchers-hub flow: a git repository
    /// (cloned via libgit2's local transport) that ships a launcher at
    /// `openarm01/openarm01_teleop.json5` should land in `launchers.json5`
    /// with `launcher_name = "openarm01_teleop"`, the `file://` source
    /// URI, a non-`HEAD` resolved ref, and the relative path to the
    /// `.json5` file.
    #[test]
    fn process_refresh_discovers_launchers_from_git_repo() {
        use crate::services::repo::cache::launchers_repo_cache_path;

        // Build a real git repo with one launcher committed at the same
        // path layout as launchers-hub's openarm01_teleop launcher.
        let src_tmp = tempfile::tempdir().unwrap();
        let src = src_tmp.path();
        let repo = git2::Repository::init(src).expect("init repo");
        std::fs::create_dir_all(src.join("openarm01")).unwrap();
        std::fs::write(
            src.join("openarm01").join("openarm01_teleop.json5"),
            r#"{
                peppy_schema: "launcher/v1",
                deployments: []
            }"#,
        )
        .unwrap();
        let branch = publish_and_commit(&repo, src, &["openarm01/openarm01_teleop.json5"]);

        // Configure peppy with a single git repo entry pointing at the
        // local source via `file://`.
        let peppy_tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(peppy_tmp.path());
        let repo_url = format!("file://{}", src.display());
        write_repos(
            &peppy_dirs,
            &format!(r#"[{{ "id": 1, "type": "git", "url": "{repo_url}", "ref": "{branch}" }}]"#,),
        );

        let launchers = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {})
            .unwrap()
            .items
            .launchers;
        assert_eq!(launchers.len(), 1, "exactly one launcher expected");
        let launcher = &launchers[0];
        assert_eq!(launcher.launcher_name, "openarm01_teleop");
        assert_eq!(launcher.origin.kind(), RepoSourceKind::Git);
        assert_eq!(launcher.origin.repo_url(), Some(repo_url.as_str()));
        assert_eq!(
            launcher.origin.repo_ref(),
            Some(branch.as_str()),
            "resolved_ref should record the branch we cloned, not literal `HEAD`"
        );
        assert_eq!(
            launcher.origin.path_str(),
            "openarm01/openarm01_teleop.json5"
        );
        assert!(
            launcher.sha256.as_str().len() == 64,
            "sha256 should be populated from the manifest file bytes"
        );

        // Round-trip through `write_repo_cache` so we also lock in
        // the on-disk shape the user specified: `launcher_name` field
        // present, `entry_type` and `node_name` absent.
        write_repo_cache(&peppy_dirs, &launchers).unwrap();
        let raw = std::fs::read_to_string(launchers_repo_cache_path(&peppy_dirs))
            .expect("read launcher cache");
        let parsed: serde_json::Value =
            serde_json5::from_str(&raw).expect("launcher cache should be valid JSON5");
        let entry = &parsed.as_array().expect("array")[0];
        assert_eq!(entry["launcher_name"], "openarm01_teleop");
        let origin = &entry["origin"];
        assert_eq!(origin["source_type"], "git");
        assert_eq!(origin["repo_url"], repo_url);
        assert_eq!(origin["repo_ref"], branch);
        assert_eq!(origin["path"], "openarm01/openarm01_teleop.json5");
        assert_eq!(
            origin["commit"]
                .as_str()
                .expect("a commit is recorded")
                .len(),
            40,
            "the entry records the commit it was read at, not just the branch"
        );
        assert!(
            entry.get("node_name").is_none(),
            "node_name should be replaced by launcher_name"
        );
    }

    #[test]
    fn process_refresh_emits_progress_feedback() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo = tmp.path().join("repo");
        write_node_json5(&repo.join("node_a"), "node_a", "v1");

        write_repos(&peppy_dirs, &fs_repos(&[(1, &repo)]));

        let mut feedbacks: Vec<RepoRefreshFeedback> = Vec::new();
        let _ = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |fb| feedbacks.push(fb)).unwrap();

        let progress_messages: Vec<&str> = feedbacks
            .iter()
            .filter_map(|f| match f {
                RepoRefreshFeedback::Progress { message } => Some(message.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            progress_messages.iter().any(|m| m.starts_with("Reading ")),
            "expected a 'Reading …' progress feedback, got: {:?}",
            feedbacks
        );

        let discovered_names: Vec<&str> = feedbacks
            .iter()
            .filter_map(|f| match f {
                RepoRefreshFeedback::Discovered { item_name, .. } => Some(item_name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(discovered_names, vec!["node_a"]);
    }

    #[test]
    fn read_or_create_repos_sorts_by_id() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join("repositories.json5"),
            r#"[
                { "id": 3, "type": "fs", "path": "/third" },
                { "id": 1, "type": "fs", "path": "/first" },
                { "id": 2, "type": "fs", "path": "/second" }
            ]"#,
        )
        .unwrap();

        let repos = read_or_create_repos(&peppy_dirs).unwrap();
        assert_eq!(repos[0].get("path").unwrap().as_str().unwrap(), "/first");
        assert_eq!(repos[1].get("path").unwrap().as_str().unwrap(), "/second");
        assert_eq!(repos[2].get("path").unwrap().as_str().unwrap(), "/third");
    }

    /// Create a local git repo at `path` with `commit_count` commits on the
    /// current branch. Each commit adds a file `commit_N.txt`. Returns the
    /// OIDs of the created commits in order (oldest first).
    fn init_repo_with_commits(path: &Path, commit_count: usize) -> Vec<git2::Oid> {
        let repo = git2::Repository::init(path).expect("init repo");
        let signature =
            git2::Signature::now("Peppy", "peppy@example.com").expect("create signature");
        let mut commits: Vec<git2::Oid> = Vec::new();

        for i in 0..commit_count {
            let file_name = format!("commit_{i}.txt");
            std::fs::write(path.join(&file_name), format!("content {i}")).expect("write file");
            let mut index = repo.index().expect("open index");
            index.add_path(Path::new(&file_name)).expect("add to index");
            index.write().expect("write index");
            let tree_id = index.write_tree().expect("write tree");
            let tree = repo.find_tree(tree_id).expect("find tree");

            let parent_commits: Vec<git2::Commit> = commits
                .last()
                .map(|oid| vec![repo.find_commit(*oid).expect("find parent")])
                .unwrap_or_default();
            let parent_refs: Vec<&git2::Commit> = parent_commits.iter().collect();

            let oid = repo
                .commit(
                    Some("HEAD"),
                    &signature,
                    &signature,
                    &format!("commit {i}"),
                    &tree,
                    &parent_refs,
                )
                .expect("commit");
            commits.push(oid);
        }

        commits
    }

    fn count_commits(repo: &git2::Repository) -> usize {
        let mut revwalk = repo.revwalk().expect("revwalk");
        revwalk.push_head().expect("push head");
        revwalk.count()
    }

    /// libgit2's local transport (used for both raw paths and `file://`
    /// URLs) rejects `depth(1)` with "shallow fetch is not supported by the
    /// local transport". The checkout has to detect those URLs and skip the
    /// depth, otherwise the fetch fails outright. These cases lock in that
    /// fallback so a regression that asks a local URL for depth 1 fails fast.
    #[rstest::rstest]
    #[case::file_url("file://")]
    #[case::raw_path("")]
    fn a_local_repository_is_read_with_its_whole_history(#[case] url_prefix: &str) {
        let src_tmp = tempfile::tempdir().unwrap();
        let commits = init_repo_with_commits(src_tmp.path(), 3);

        let dst_tmp = tempfile::tempdir().unwrap();
        let dst = dst_tmp.path().join("clone");
        let repo_url = format!("{url_prefix}{}", src_tmp.path().display());

        let commit = shallow_checkout_of_read_ref(&repo_url, None, &dst, &mut |_| {})
            .unwrap_or_else(|e| {
                panic!("reading {repo_url} should succeed (depth must be skipped): {e:?}")
            });

        assert_eq!(commit.as_str(), commits[2].to_string());
        assert!(
            !dst.join(".git/shallow").exists(),
            "{repo_url} goes through libgit2's local transport which rejects \
             depth(1); .git/shallow should not exist"
        );
        let repo = git2::Repository::open(&dst).expect("open the checkout");
        assert_eq!(count_commits(&repo), 3);
    }

    /// A commit named by its hash is fetched by that hash, through the local
    /// transport too.
    #[test]
    fn a_local_repository_is_read_at_a_commit_named_by_its_hash() {
        let src_tmp = tempfile::tempdir().unwrap();
        let commits = init_repo_with_commits(src_tmp.path(), 3);
        let target = commits[1].to_string();

        let dst_tmp = tempfile::tempdir().unwrap();
        let dst = dst_tmp.path().join("clone");
        let repo_url = format!("file://{}", src_tmp.path().display());

        let commit = shallow_checkout_of_read_ref(
            &repo_url,
            Some(&ReadRef::Named(target.clone())),
            &dst,
            &mut |_| {},
        )
        .expect("the commit is read");

        assert_eq!(commit.as_str(), target);
    }

    /// A repository at `base/hub` that publishes `arm:v1` at each of two
    /// commits on its only branch, with the annotated tag `v1` on the first.
    /// Returns the two commits, oldest first.
    ///
    /// Served over `git://` (see [`GitDaemon`]), it is read through the
    /// depth-1 fetch peppy makes of a real remote.
    fn hub_with_a_tag_behind_its_head(base: &Path) -> (String, String) {
        let src = base.join("hub");
        let repo = git2::Repository::init(&src).expect("init repo");
        // The daemon hands out a commit by its hash only when the repository
        // allows it, as GitHub does.
        repo.config()
            .expect("repository config")
            .set_bool("uploadpack.allowReachableSHA1InWant", true)
            .expect("allow fetches by hash");

        write_node_json5(&src.join("arm"), "arm", "v1");
        publish_and_commit(&repo, &src, &["arm/peppy.json5"]);
        let tagged = repo.head().expect("head").peel_to_commit().expect("commit");
        let signature =
            git2::Signature::now("Peppy", "peppy@example.com").expect("create signature");
        repo.tag("v1", tagged.as_object(), &signature, "v1", false)
            .expect("tag the first commit");

        std::fs::write(src.join("notes.txt"), "second").expect("write notes");
        publish_and_commit(&repo, &src, &["arm/peppy.json5", "notes.txt"]);
        let head = repo.head().expect("head").peel_to_commit().expect("commit");
        (tagged.id().to_string(), head.id().to_string())
    }

    /// Refreshes one git repository at `url`, configured on `git_ref`.
    fn refresh_git_repo_on(peppy_dirs: &PeppyDirs, url: &str, git_ref: &str) -> RefreshedRepos {
        write_repos(
            peppy_dirs,
            &format!(r#"[{{ "id": 1, "type": "git", "url": "{url}", "ref": "{git_ref}" }}]"#),
        );
        refresh_every(peppy_dirs, TEST_NOW, &mut |_| {}).expect("the refresh runs")
    }

    /// The commit `refreshed` read its one node at, after checking that the
    /// read went through a depth-1 fetch.
    fn commit_read_through_a_shallow_clone(
        peppy_dirs: &PeppyDirs,
        refreshed: &RefreshedRepos,
    ) -> String {
        assert!(
            refreshed.failures.is_empty(),
            "the repository is read: {:?}",
            refreshed.failures
        );
        let (url, commit) = refreshed.items.nodes[0]
            .origin
            .checkout()
            .expect("a git origin");
        let checkout = crate::services::node::cache::git::checkout_dir_for(peppy_dirs, url, commit);
        assert!(
            checkout.join(".git").join("shallow").exists(),
            "the clone at {} is shallow",
            checkout.display()
        );
        commit.as_str().to_owned()
    }

    #[test]
    fn process_refresh_fetches_a_configured_tag_behind_the_head() {
        let served = tempfile::tempdir().unwrap();
        let (tagged, _head) = hub_with_a_tag_behind_its_head(served.path());
        let daemon = GitDaemon::serve(served.path());
        let peppy_tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(peppy_tmp.path());

        let refreshed = refresh_git_repo_on(&peppy_dirs, &daemon.url("hub"), "v1");

        assert_eq!(
            commit_read_through_a_shallow_clone(&peppy_dirs, &refreshed),
            tagged
        );
    }

    #[test]
    fn process_refresh_fetches_a_configured_commit_that_is_no_branch_head() {
        let served = tempfile::tempdir().unwrap();
        let (older, _head) = hub_with_a_tag_behind_its_head(served.path());
        let daemon = GitDaemon::serve(served.path());
        let peppy_tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(peppy_tmp.path());

        let refreshed = refresh_git_repo_on(&peppy_dirs, &daemon.url("hub"), &older);

        assert_eq!(
            commit_read_through_a_shallow_clone(&peppy_dirs, &refreshed),
            older
        );
    }

    #[test]
    fn process_refresh_refuses_a_configured_ref_the_remote_does_not_serve() {
        let served = tempfile::tempdir().unwrap();
        hub_with_a_tag_behind_its_head(served.path());
        let daemon = GitDaemon::serve(served.path());
        let peppy_tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(peppy_tmp.path());
        let url = daemon.url("hub");

        let refreshed = refresh_git_repo_on(&peppy_dirs, &url, "v9");

        assert!(refreshed.items.nodes.is_empty());
        assert_eq!(refreshed.failures.len(), 1);
        let failure = &refreshed.failures[0];
        assert_eq!(failure.read_failure, RepoReadFailure::Unreachable);
        assert!(
            failure
                .detail
                .starts_with(&format!("{url} does not serve the configured ref `v9`: ")),
            "{}",
            failure.detail
        );
    }

    /// A refresh fetches the commit it reads and nothing else: not the
    /// commit behind it and not the tip of another branch. It writes no
    /// ref, not even for a tag on the commit it reads.
    #[test]
    fn process_refresh_fetches_the_commit_it_reads_alone() {
        let served = tempfile::tempdir().unwrap();
        let (behind, head) = hub_with_a_tag_behind_its_head(served.path());
        let hub = git2::Repository::open(served.path().join("hub")).expect("open the hub");
        let head_commit = hub
            .find_commit(git2::Oid::from_str(&head).unwrap())
            .expect("the head commit");
        let signature = git2::Signature::now("Peppy", "peppy@example.com").unwrap();
        hub.tag("v2", head_commit.as_object(), &signature, "v2", false)
            .expect("tag the head");
        let side = hub
            .commit(
                Some("refs/heads/side"),
                &signature,
                &signature,
                "side",
                &head_commit.tree().unwrap(),
                &[&head_commit],
            )
            .expect("commit on another branch")
            .to_string();
        let daemon = GitDaemon::serve(served.path());
        let peppy_tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(peppy_tmp.path());

        let refreshed = refresh_git_repo_on(&peppy_dirs, &daemon.url("hub"), "");

        assert_eq!(
            commit_read_through_a_shallow_clone(&peppy_dirs, &refreshed),
            head,
            "no ref reads the remote's HEAD"
        );
        let checkout = refreshed_checkout(&peppy_dirs, &refreshed);
        for (absent, what) in [
            (behind.as_str(), "the commit behind the head"),
            (side.as_str(), "the tip of another branch"),
        ] {
            assert!(
                checkout
                    .find_commit(git2::Oid::from_str(absent).unwrap())
                    .is_err(),
                "the checkout holds {what}"
            );
        }
        assert_eq!(ref_names(&checkout), Vec::<String>::new());
    }

    /// The checkout the one repository of `refreshed` was read from.
    fn refreshed_checkout(peppy_dirs: &PeppyDirs, refreshed: &RefreshedRepos) -> git2::Repository {
        let (url, commit) = refreshed.items.nodes[0]
            .origin
            .checkout()
            .expect("a git origin");
        let dir = crate::services::node::cache::git::checkout_dir_for(peppy_dirs, url, commit);
        git2::Repository::open(dir).expect("open the checkout")
    }

    /// The name of every ref `repo` holds.
    fn ref_names(repo: &git2::Repository) -> Vec<String> {
        repo.references()
            .expect("list the refs")
            .names()
            .map(|name| name.expect("a UTF-8 ref name").to_owned())
            .collect()
    }

    /// A name that is both a branch and a tag reads the branch, as
    /// `git clone --branch` does.
    #[test]
    fn process_refresh_reads_a_branch_before_a_tag_of_the_same_name() {
        let served = tempfile::tempdir().unwrap();
        let (tagged, head) = hub_with_a_tag_behind_its_head(served.path());
        let hub = git2::Repository::open(served.path().join("hub")).expect("open the hub");
        let head_commit = hub
            .find_commit(git2::Oid::from_str(&head).unwrap())
            .expect("the head commit");
        hub.branch("v1", &head_commit, false)
            .expect("the branch of the tag's name");
        let daemon = GitDaemon::serve(served.path());
        let peppy_tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(peppy_tmp.path());

        let refreshed = refresh_git_repo_on(&peppy_dirs, &daemon.url("hub"), "v1");

        assert_ne!(tagged, head);
        assert_eq!(
            commit_read_through_a_shallow_clone(&peppy_dirs, &refreshed),
            head
        );
        assert_eq!(
            ref_names(&refreshed_checkout(&peppy_dirs, &refreshed)),
            Vec::<String>::new(),
            "reading a branch writes no ref"
        );
    }

    #[test]
    fn process_refresh_refuses_a_repository_with_no_commit() {
        let served = tempfile::tempdir().unwrap();
        git2::Repository::init(served.path().join("hub")).expect("init an empty repo");
        let daemon = GitDaemon::serve(served.path());
        let peppy_tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(peppy_tmp.path());
        let url = daemon.url("hub");

        let refreshed = refresh_git_repo_on(&peppy_dirs, &url, "");

        assert!(refreshed.items.nodes.is_empty());
        assert_eq!(refreshed.failures.len(), 1);
        let failure = &refreshed.failures[0];
        assert_eq!(failure.read_failure, RepoReadFailure::Unreachable);
        assert_eq!(
            failure.detail,
            format!("{url} has no commit to read: it advertises no ref named `HEAD`")
        );
    }
    fn release(version: &str) -> PeppyBuild {
        PeppyBuild::Release(version.to_owned())
    }

    fn status_line(id: u64, identity: &str, read_ref: Option<&str>) -> RepoStatus {
        RepoStatus {
            id,
            identity: identity.to_owned(),
            source_type: RepoSourceKind::Git,
            last_read_unix_secs: Some(1),
            read_ref: read_ref.map(ReadRef::from_git_name),
            last_failure: None,
        }
    }

    /// Which repositories the daemon reads again at start: only those on
    /// `@{peppy-release}` that this machine read as they are configured, and
    /// read for another version than the one running.
    #[test]
    fn the_start_reads_again_only_release_repositories_read_for_another_version() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let hub = |id: u64, git_ref: &str| {
            serde_json::json!({
                "id": id,
                "type": "git",
                "url": format!("https://example.com/hub{id}.git"),
                "ref": git_ref,
            })
        };
        let repos = vec![
            hub(1, "@{peppy-release}"),
            hub(2, "@{peppy-release}"),
            hub(3, "@{peppy-release}"),
            hub(4, "@{peppy-release}"),
            hub(5, "@{peppy-release}"),
            hub(6, "peppy-release/v0.31.1"),
        ];
        let current = "refs/tags/peppy-release/v0.31.2";
        let statuses = vec![
            // Read for this version.
            status_line(
                1,
                "https://example.com/hub1.git@@{peppy-release}",
                Some(current),
            ),
            // Read for the previous version.
            status_line(
                2,
                "https://example.com/hub2.git@@{peppy-release}",
                Some("refs/tags/peppy-release/v0.31.1"),
            ),
            // The id was read as another repository: its line is not this
            // repository's.
            status_line(3, "https://example.com/hub3.git@main", Some("main")),
            // 4 was never read on this machine: no status line.
            // Excluded below.
            status_line(
                5,
                "https://example.com/hub5.git@@{peppy-release}",
                Some("main"),
            ),
            // Pinned to one tag: it reads that tag in every version.
            status_line(
                6,
                "https://example.com/hub6.git@peppy-release/v0.31.1",
                None,
            ),
        ];
        write_excluded_repos(
            &peppy_dirs,
            r#"[{ "id": 1, "type": "git", "url": "https://example.com/hub5.git", "ref": "@{peppy-release}" }]"#,
        );

        let ids = release_repositories_read_for_another_version(
            &repos,
            &ExclusionSet::load(&peppy_dirs),
            &statuses,
            &release("v0.31.2"),
        );

        assert_eq!(ids, BTreeSet::from([2]));
    }

    /// A hub with a branch and a tag both named `peppy-release/v0.31.2`, at
    /// different commits: a release build of v0.31.2 reads the tag.
    #[test]
    fn a_release_build_reads_its_tag_even_beside_a_branch_of_the_same_name() {
        let served = tempfile::tempdir().unwrap();
        let src = served.path().join("hub");
        let repo = git2::Repository::init(&src).expect("init repo");
        write_node_json5(&src.join("arm"), "arm", "v1");
        publish_and_commit(&repo, &src, &["arm/peppy.json5"]);
        let branched = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("peppy-release/v0.31.2", &branched, false)
            .expect("the branch of the tag's name");
        std::fs::write(src.join("notes.txt"), "tagged").unwrap();
        publish_and_commit(&repo, &src, &["arm/peppy.json5", "notes.txt"]);
        let tagged = repo.head().unwrap().peel_to_commit().unwrap();
        let signature = git2::Signature::now("Peppy", "peppy@example.com").unwrap();
        repo.tag(
            "peppy-release/v0.31.2",
            tagged.as_object(),
            &signature,
            "v0.31.2",
            false,
        )
        .expect("tag the release");
        std::fs::write(src.join("notes.txt"), "after the release").unwrap();
        publish_and_commit(&repo, &src, &["arm/peppy.json5", "notes.txt"]);

        let daemon = GitDaemon::serve(served.path());
        let peppy_tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(peppy_tmp.path());
        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "git", "url": "{}", "ref": "@{{peppy-release}}" }}]"#,
                daemon.url("hub")
            ),
        );

        let refreshed = process_refresh(
            &peppy_dirs,
            &release("v0.31.2"),
            &RefreshScope::Every,
            TEST_NOW,
            &mut |_| {},
            &CancellationToken::new(),
        )
        .unwrap()
        .expect("nothing cancels the refresh");

        assert!(refreshed.failures.is_empty(), "{:?}", refreshed.failures);
        let release_tag = Some(ReadRef::ReleaseTag("v0.31.2".to_owned()));
        let EntryOrigin::Git {
            repo_ref,
            read_ref,
            commit,
            ..
        } = &refreshed.items.nodes[0].origin
        else {
            panic!("the hub is a git repository");
        };
        assert_eq!(commit.as_str(), tagged.id().to_string());
        assert_ne!(tagged.id(), branched.id());
        assert_eq!(repo_ref.as_deref(), Some("@{peppy-release}"));
        assert_eq!(read_ref, &release_tag);
        assert_eq!(refreshed.statuses[0].read_ref, release_tag);
    }

    /// A hub tagged for two releases, served over `git://`, with the
    /// commits of the two tags.
    fn hub_tagged_for_two_releases(base: &Path) -> (String, String) {
        let src = base.join("hub");
        let repo = git2::Repository::init(&src).expect("init repo");
        let signature = git2::Signature::now("Peppy", "peppy@example.com").unwrap();
        let mut commits = Vec::new();
        for (version, note) in [("v0.31.1", "first"), ("v0.31.2", "second")] {
            write_node_json5(&src.join("arm"), "arm", "v1");
            std::fs::write(src.join("notes.txt"), note).unwrap();
            publish_and_commit(&repo, &src, &["arm/peppy.json5", "notes.txt"]);
            let commit = repo.head().unwrap().peel_to_commit().unwrap();
            repo.tag(
                &format!("peppy-release/{version}"),
                commit.as_object(),
                &signature,
                version,
                false,
            )
            .unwrap();
            commits.push(commit.id().to_string());
        }
        (commits[0].clone(), commits[1].clone())
    }

    fn cached_node_commit(peppy_dirs: &PeppyDirs) -> String {
        let nodes: Vec<NodeCacheEntry> =
            crate::services::repo::cache::load_repo_cache(peppy_dirs).unwrap();
        nodes[0].origin.commit().unwrap().as_str().to_owned()
    }

    /// After an upgrade, the start reads the hub again at the tag of the new
    /// version and leaves `repositories.json5` byte for byte as it was. Once
    /// read for the running version, the next start reads nothing. When the
    /// hub cannot be read, the repository keeps the entries it holds, and
    /// the start after that tries again.
    #[tokio::test]
    async fn a_version_change_reads_the_release_repositories_again() {
        let served = tempfile::tempdir().unwrap();
        let (first, second) = hub_tagged_for_two_releases(served.path());
        let daemon = GitDaemon::serve(served.path());
        let peppy_tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(peppy_tmp.path());
        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "git", "url": "{}", "ref": "@{{peppy-release}}" }}]"#,
                daemon.url("hub")
            ),
        );
        let repos_file = std::fs::read(repositories_list_path(&peppy_dirs)).unwrap();

        let read_for_the_old_version = process_refresh(
            &peppy_dirs,
            &release("v0.31.1"),
            &RefreshScope::Every,
            TEST_NOW,
            &mut |_| {},
            &CancellationToken::new(),
        )
        .unwrap()
        .expect("nothing cancels the refresh");
        write_all_caches(&peppy_dirs, &read_for_the_old_version).unwrap();
        assert_eq!(cached_node_commit(&peppy_dirs), first);

        let upgraded = refresh_release_repositories_for_this_version(
            &peppy_dirs,
            &release("v0.31.2"),
            TEST_NOW,
        )
        .await
        .unwrap();
        let VersionRefresh::Refreshed { ids, failures } = upgraded else {
            panic!("the hub was read for another version");
        };
        assert_eq!(ids, BTreeSet::from([1]));
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(cached_node_commit(&peppy_dirs), second);
        assert_eq!(
            std::fs::read(repositories_list_path(&peppy_dirs)).unwrap(),
            repos_file,
            "a version change never writes repositories.json5"
        );

        assert!(matches!(
            refresh_release_repositories_for_this_version(
                &peppy_dirs,
                &release("v0.31.2"),
                TEST_NOW
            )
            .await
            .unwrap(),
            VersionRefresh::NotNeeded
        ));

        drop(daemon);
        for _ in 0..2 {
            let unreachable = refresh_release_repositories_for_this_version(
                &peppy_dirs,
                &release("v0.31.3"),
                TEST_NOW,
            )
            .await
            .unwrap();
            let VersionRefresh::Refreshed { failures, .. } = unreachable else {
                panic!("a hub not read for v0.31.3 is read again at each start");
            };
            assert_eq!(failures.len(), 1);
            assert_eq!(cached_node_commit(&peppy_dirs), second);
        }
    }
}
