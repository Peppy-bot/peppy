use crate::Result;
use crate::services::action_loop::{GoalHandler, accept_goal, reject_goal, run_action_loop};
use crate::services::node::clone_with_progress;
use crate::services::node::gate::{Admission, ConcurrencyGate};
use crate::services::repo::cache::{
    ContractCacheEntry, EntryOrigin, LauncherCacheEntry, NodeCacheEntry, PairingCacheEntry,
    RepoCacheEntry, RepoItems, write_repo_cache,
};
use crate::services::repo::exclude::ExclusionSet;
use crate::services::repo::index::{PublishedItem, build_cache_entries, read_published_items};
use crate::services::repo::status::{self, RepoStatus, RepoStatusFailure};
use crate::services::repo::{normalize_repo_entries, source_identity};
use core_node_api::ActionId;
use core_node_api::encoding::{
    RepoRefreshFeedback, RepoRefreshGoal, RepoRefreshGoalResponse, RepoRefreshResult, RepoSource,
    RepoSourceKind,
};
use core_node_api::names;
use daemon_config::consts::PeppyDirs;
use daemon_config::repository::GitCommit;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::{ConcurrentAction, PendingGoal};
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
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
    RepoRefreshGoalResponse::accepted()
        .encode()
        .map_err(|e| PeppyError::InvalidServiceRequest {
            identifier: ActionId::RepoRefresh.name().to_string(),
            reason: e.to_string(),
        })
}

fn encode_refresh_rejected(reason: impl Into<String>) -> PeppyResult<Payload> {
    RepoRefreshGoalResponse::rejected(reason)
        .encode()
        .map_err(|e| PeppyError::InvalidServiceRequest {
            identifier: ActionId::RepoRefresh.name().to_string(),
            reason: e.to_string(),
        })
}

impl GoalHandler for RepoRefreshGoalHandler {
    async fn handle_goal(&self, pending: PendingGoal) {
        if let Err(e) = RepoRefreshGoal::decode(pending.request_bytes()) {
            reject_goal(
                pending,
                encode_refresh_rejected(format!("invalid goal payload: {e}")),
            )
            .await;
            return;
        }

        let generation = match self.gate.try_admit(300, false) {
            // `repo_refresh` never forces, so nothing is ever superseded here.
            Admission::Admitted { generation, .. } => generation,
            Admission::AlreadyRunning { .. } => {
                reject_goal(
                    pending,
                    encode_refresh_rejected("a repo refresh operation is already in progress"),
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
            // Frees the gate slot on every exit: explicitly before completion on
            // the normal path (via `release_then_complete` below), or on unwind
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

            let dirs = peppy_dirs;
            let scan = tokio::task::spawn_blocking(move || -> Result<RefreshCounts> {
                let _guard = crate::services::repo::refresh_lock().lock();
                let mut emit = |fb: RepoRefreshFeedback| {
                    let _ = tx.send(fb);
                };
                let refreshed = process_refresh(&dirs, SystemTime::now(), &mut emit)?;
                // Written whether or not a repository failed: the whole
                // point of containing a failure is that the repositories
                // that did update take effect, and that the ones that
                // did not keep the entries they last published. Caches
                // store every entry so that `repo list` can display every
                // source and users can pick a specific `sha256`.
                write_all_caches(&dirs, &refreshed)?;
                Ok(RefreshCounts {
                    nodes: count_unique(&refreshed.nodes),
                    launchers: count_unique(&refreshed.launchers),
                    contracts: count_unique(&refreshed.contracts),
                    pairings: count_unique(&refreshed.pairings),
                    failures: refreshed.failures,
                })
            })
            .await;

            let result = match scan {
                Ok(Ok(counts)) if counts.failures.is_empty() => RepoRefreshResult::success(
                    counts.nodes,
                    counts.launchers,
                    counts.contracts,
                    counts.pairings,
                ),
                Ok(Ok(counts)) => RepoRefreshResult::failure(failure_report(&counts.failures)),
                Ok(Err(e)) => {
                    warn!("Repo refresh failed: {}", e);
                    RepoRefreshResult::failure(e.to_string())
                }
                Err(e) => RepoRefreshResult::failure(format!("task panicked: {}", e)),
            };

            // Flush all pending feedbacks before completing: the end-of-stream
            // sentinel that `complete` emits must not race ahead of the final
            // feedback lines.
            let _ = drain.await;

            if let Ok(payload) = result.encode() {
                slot.release_then_complete(&goal_ctx, payload).await;
            }
        });
    }
}

/// Unique-identity counts for the four caches, plus the repositories
/// that failed. What one refresh has to report back. Named fields rather
/// than a tuple: four `u32`s in a row are indistinguishable at the call
/// site, and swapping two of them would go unnoticed.
struct RefreshCounts {
    nodes: u32,
    launchers: u32,
    contracts: u32,
    pairings: u32,
    failures: Vec<RepoFailure>,
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
    let dirs = peppy_dirs.clone();
    let outcome = tokio::task::spawn_blocking(move || -> Result<Vec<RepoFailure>> {
        let _guard = crate::services::repo::refresh_lock().lock();
        let refreshed = process_refresh(&dirs, SystemTime::now(), &mut |_| {})?;
        write_all_caches(&dirs, &refreshed)?;
        Ok(refreshed.failures)
    })
    .await;

    match outcome {
        Ok(Ok(failures)) if failures.is_empty() => None,
        Ok(Ok(failures)) => Some(failure_report(&failures)),
        Ok(Err(e)) => Some(format!("re-indexing failed: {e}")),
        Err(e) => Some(format!("re-indexing task panicked: {e}")),
    }
}

/// A repository that was skipped during refresh because it appears in the
/// `excluded_repositories.json5` configuration.
#[derive(Debug, Clone)]
pub(crate) struct ExcludedRepo {
    pub(crate) source_type: RepoSourceKind,
    pub(crate) identity: String,
}

/// Why one repository contributed nothing new to a refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepoFailureKind {
    /// Could not be read at all: the clone failed, the configured path is
    /// gone, the configuration entry is unrecognized. Kept distinct from
    /// [`RepoFailureKind::Conflict`] because an outage is not a content
    /// bug, and reporting one as the other sends the user to the wrong
    /// place entirely.
    Unreachable,
    /// Read fine; the contents are wrong. An identity is claimed by
    /// several manifests, so the repository states two different answers
    /// to the same question and there is no defensible winner.
    Conflict,
}

impl RepoFailureKind {
    /// Stable machine-readable value, recorded in `repo_status.json5` and
    /// put on the wire. Kept separate from [`Self::describe`] so the
    /// prose can be reworded without changing what tools match on.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            RepoFailureKind::Unreachable => "unreachable",
            RepoFailureKind::Conflict => "conflict",
        }
    }

    /// Predicate that reads as a sentence about the repository.
    fn describe(self) -> &'static str {
        match self {
            RepoFailureKind::Unreachable => "could not be read",
            RepoFailureKind::Conflict => "contradicts itself",
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
    pub(crate) kind: RepoFailureKind,
    pub(crate) detail: String,
    /// Previous entries kept in place for this repository, across all
    /// four kinds. Zero on a machine that has never read it successfully.
    pub(crate) retained: usize,
}

impl std::fmt::Display for RepoFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "repository {} ({}) {} [{}]: {}",
            self.id,
            self.label,
            self.kind.describe(),
            self.kind.as_str(),
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
/// the caches (freshly read, plus previous entries retained for
/// repositories that failed), the repositories skipped by the exclusion
/// list, and the repositories that failed.
pub(crate) struct RefreshedRepos {
    pub(crate) nodes: Vec<NodeCacheEntry>,
    pub(crate) launchers: Vec<LauncherCacheEntry>,
    pub(crate) contracts: Vec<ContractCacheEntry>,
    pub(crate) pairings: Vec<PairingCacheEntry>,
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

/// The four caches as they stood before this refresh. A repository that
/// fails its read keeps serving what it last published, so its
/// identities do not vanish out from under launchers that reference
/// them.
struct PreviousCaches {
    nodes: Vec<NodeCacheEntry>,
    launchers: Vec<LauncherCacheEntry>,
    contracts: Vec<ContractCacheEntry>,
    pairings: Vec<PairingCacheEntry>,
}

impl PreviousCaches {
    /// A cache that cannot be read is treated as empty rather than fatal:
    /// this runs on the recovery path, where refusing to start because
    /// the fallback is also broken helps nobody.
    fn load(peppy_dirs: &PeppyDirs) -> Self {
        fn read<E: RepoCacheEntry>(peppy_dirs: &PeppyDirs) -> Vec<E> {
            crate::services::repo::cache::load_repo_cache::<E>(peppy_dirs).unwrap_or_else(|e| {
                warn!("Could not read the previous {} cache: {e}", E::KIND);
                Vec::new()
            })
        }
        Self {
            nodes: read(peppy_dirs),
            launchers: read(peppy_dirs),
            contracts: read(peppy_dirs),
            pairings: read(peppy_dirs),
        }
    }
}

/// The previous entries of one kind that `repo_id` owns, using the same
/// attribution rule that tags entries at lookup time so a retained entry
/// keeps exactly the priority it had.
fn retained_entries<E: RepoCacheEntry>(previous: &[E], repos: &[Value], repo_id: u64) -> Vec<E> {
    previous
        .iter()
        .filter(|e| crate::services::repo::owning_repo_id(repos, e.origin()) == Some(repo_id))
        .cloned()
        .collect()
}

/// Publishes every cache file from one refresh result. The four entry
/// caches must always move together: rewriting only a subset leaves the
/// untouched files still listing items from repositories that are no
/// longer configured. The status file moves with them so it always
/// describes the entries that are actually on disk.
pub(crate) fn write_all_caches(peppy_dirs: &PeppyDirs, refreshed: &RefreshedRepos) -> Result<()> {
    write_repo_cache(peppy_dirs, &refreshed.nodes)?;
    write_repo_cache(peppy_dirs, &refreshed.launchers)?;
    write_repo_cache(peppy_dirs, &refreshed.contracts)?;
    write_repo_cache(peppy_dirs, &refreshed.pairings)?;
    status::write(peppy_dirs, &refreshed.statuses)
}

/// Parse a JSON entry from repositories.json5 into a `RepoSource`.
pub(crate) fn parse_repo_entry(entry: &Value) -> Option<RepoSource> {
    let typ = entry.get("type")?.as_str()?;
    match typ {
        "fs" => {
            let path = entry.get("path")?.as_str()?;
            Some(RepoSource::Fs(PathBuf::from(path)))
        }
        "git" => {
            let url = entry.get("url")?.as_str()?.to_owned();
            let repo_ref = entry
                .get("ref")
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned());
            Some(RepoSource::Git {
                repo_url: url,
                repo_ref,
            })
        }
        _ => None,
    }
}

const DEFAULT_REPOS_TEMPLATE: &str = include_str!("../../../assets/default_repositories.json5");

/// Reads the repositories.json5 config file, creating it with defaults if it
/// does not exist yet.  Ensures every entry has an integer `id` field
/// (auto-assigns missing ids) and returns entries sorted by `id`.
pub(crate) fn read_or_create_repos(peppy_dirs: &PeppyDirs) -> Result<Vec<Value>> {
    let conf_dir = peppy_dirs.conf_dir();
    std::fs::create_dir_all(&conf_dir)?;
    let repos_path = conf_dir.join("repositories.json5");

    let mut repos: Vec<Value> = if repos_path.exists() {
        let content = std::fs::read_to_string(&repos_path)?;
        serde_json5::from_str(&content).map_err(|e| {
            core_node_api::Error::Decoding(format!("failed to parse repositories.json5: {e}"))
        })?
    } else {
        std::fs::write(&repos_path, DEFAULT_REPOS_TEMPLATE)?;
        serde_json5::from_str(DEFAULT_REPOS_TEMPLATE).map_err(|e| {
            core_node_api::Error::Decoding(format!("failed to parse default repositories: {e}"))
        })?
    };

    normalize_repo_entries(&mut repos, &repos_path, "repositories")?;

    Ok(repos)
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
/// entries read at two different times. The caches never had a notion of
/// a synchronised snapshot across repositories, so this is not a new
/// class of skew, but anything that later needs one coherent set of
/// bytes must establish that itself.
/// `now` is passed in rather than read from the clock so that tests are
/// deterministic.
pub(crate) fn process_refresh(
    peppy_dirs: &PeppyDirs,
    now: SystemTime,
    on_feedback: &mut dyn FnMut(RepoRefreshFeedback),
) -> Result<RefreshedRepos> {
    let (repos, exclusions) = {
        let _guard = crate::services::repo::repos_file_lock().lock();
        let repos = read_or_create_repos(peppy_dirs)?;
        let exclusions = ExclusionSet::load(peppy_dirs);
        (repos, exclusions)
    };
    let previous = PreviousCaches::load(peppy_dirs);
    let previous_statuses = status::read(peppy_dirs);
    let stamp = status::unix_secs(now);
    let mut failures: Vec<RepoFailure> = Vec::new();
    let mut statuses: Vec<RepoStatus> = Vec::new();

    let mut global_seen_nodes: HashSet<(String, String)> = HashSet::new();
    let mut global_seen_launchers: HashSet<(String, String)> = HashSet::new();
    let mut global_seen_contracts: HashSet<(String, String)> = HashSet::new();
    let mut global_seen_pairings: HashSet<(String, String)> = HashSet::new();
    let mut all_nodes: Vec<NodeCacheEntry> = Vec::new();
    let mut all_launchers: Vec<LauncherCacheEntry> = Vec::new();
    let mut all_contracts: Vec<ContractCacheEntry> = Vec::new();
    let mut all_pairings: Vec<PairingCacheEntry> = Vec::new();
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

    for entry in &repos {
        let id = entry.get("id").and_then(|v| v.as_u64()).unwrap_or(0);

        // An entry peppy cannot parse has no source kind or identity to
        // record, so it is reported but gets no status line.
        let Some(source) = parse_repo_entry(entry) else {
            failures.push(RepoFailure {
                id,
                label: entry.to_string(),
                kind: RepoFailureKind::Unreachable,
                detail: "unrecognized repository entry".to_owned(),
                retained: 0,
            });
            continue;
        };

        let identity = source_identity(&source);

        if exclusions.is_excluded(&identity) {
            debug!("Excluding {} repository: {}", source.kind(), identity);
            continue;
        }

        let read = match &source {
            RepoSource::Fs(path) => read_fs_repo(path, &exclusions, on_feedback),
            RepoSource::Git { repo_url, repo_ref } => {
                let ref_suffix = repo_ref
                    .as_deref()
                    .map(|r| format!(" (ref {})", r))
                    .unwrap_or_default();
                on_feedback(RepoRefreshFeedback::Progress {
                    message: format!("Cloning {}{}", repo_url, ref_suffix),
                });
                read_git_repo(repo_url, repo_ref.as_deref(), peppy_dirs, on_feedback)
            }
        };

        // A repository that could not be read and one whose index does not
        // describe it both fall back to their previous entries; only the
        // wording differs, so an outage never reads as a content bug.
        let read = read.and_then(|items| {
            build_cache_entries(items).map_err(|detail| (RepoFailureKind::Conflict, detail))
        });
        let failure = read.as_ref().err().cloned();

        // Matched on identity as well as id: an id repointed at another
        // path or url is a different repository, and carrying the old
        // read timestamp forward would date entries this source never
        // published.
        let previous_read = previous_statuses
            .iter()
            .find(|s| s.id == id && s.identity == identity)
            .and_then(|s| s.last_read_unix_secs);

        let items = match failure {
            None => {
                statuses.push(RepoStatus {
                    id,
                    identity: identity.clone(),
                    source_type: source.kind(),
                    last_read_unix_secs: Some(stamp),
                    // Cleared on success, so a repository that recovered
                    // stops reporting an old failure.
                    last_failure: None,
                });
                read.expect("checked to be Ok above")
            }
            Some((kind, detail)) => {
                let retained = RepoItems {
                    nodes: retained_entries(&previous.nodes, &repos, id),
                    launchers: retained_entries(&previous.launchers, &repos, id),
                    contracts: retained_entries(&previous.contracts, &repos, id),
                    pairings: retained_entries(&previous.pairings, &repos, id),
                };
                let count = retained.nodes.len()
                    + retained.launchers.len()
                    + retained.contracts.len()
                    + retained.pairings.len();
                let failure = RepoFailure {
                    id,
                    label: source.display_label(),
                    kind,
                    detail,
                    retained: count,
                };
                statuses.push(RepoStatus {
                    id,
                    identity: identity.clone(),
                    source_type: source.kind(),
                    // Carried forward untouched: the retained entries are
                    // still the ones read at that time, and overwriting it
                    // with now would claim they are current.
                    last_read_unix_secs: previous_read,
                    last_failure: Some(RepoStatusFailure {
                        kind: failure.kind.as_str().to_owned(),
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

        // Merged in this repository's own slot in the id-ordered loop,
        // retained or not, so priority order and the first-seen discovery
        // feedback stay exactly as they would have been.
        merge_published(
            items.nodes,
            &mut global_seen_nodes,
            &mut all_nodes,
            on_feedback,
        );
        merge_published(
            items.launchers,
            &mut global_seen_launchers,
            &mut all_launchers,
            on_feedback,
        );
        merge_published(
            items.contracts,
            &mut global_seen_contracts,
            &mut all_contracts,
            on_feedback,
        );
        merge_published(
            items.pairings,
            &mut global_seen_pairings,
            &mut all_pairings,
            on_feedback,
        );
    }

    Ok(RefreshedRepos {
        nodes: all_nodes,
        launchers: all_launchers,
        contracts: all_contracts,
        pairings: all_pairings,
        excluded: excluded_repos,
        failures,
        statuses,
    })
}

/// Appends one repository's entries to the running cross-repo collection,
/// emitting a `Discovered` feedback the first time each `(name, tag)`
/// identity is seen. Every entry is kept (including same-identity
/// duplicates from lower-priority repositories); feedback fires only for
/// the highest-priority repository, which is read first.
fn merge_published<E: RepoCacheEntry>(
    published: Vec<E>,
    global_seen: &mut HashSet<(String, String)>,
    all: &mut Vec<E>,
    on_feedback: &mut dyn FnMut(RepoRefreshFeedback),
) {
    for entry in published {
        if global_seen.insert((entry.name().to_owned(), entry.tag().to_owned())) {
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

/// Shallow-clone a git repository into `dst` and check out `repo_ref` if set,
/// forwarding throttled transfer-progress lines into `on_feedback`.
pub(crate) fn clone_shallow(
    repo_url: &str,
    repo_ref: Option<&str>,
    dst: &Path,
    on_feedback: &mut dyn FnMut(RepoRefreshFeedback),
) -> std::result::Result<git2::Repository, String> {
    clone_with_progress(repo_url, repo_ref, dst, true, &mut |line| {
        on_feedback(RepoRefreshFeedback::Progress {
            message: line.to_owned(),
        });
    })
}

/// What one repository read yields, or why it failed.
type ReadResult = std::result::Result<Vec<PublishedItem>, (RepoFailureKind, String)>;

/// Reads a repository that lives on this machine.
///
/// Excluded subtrees are applied to the items the index declares rather
/// than to a traversal: a repository states one location per identity, so
/// excluding part of the tree is a question about which of those locations
/// this machine is willing to serve.
fn read_fs_repo(
    root: &Path,
    exclusions: &ExclusionSet,
    on_feedback: &mut dyn FnMut(RepoRefreshFeedback),
) -> ReadResult {
    if !root.exists() {
        return Err((
            RepoFailureKind::Unreachable,
            format!("path does not exist: {}", root.display()),
        ));
    }
    on_feedback(RepoRefreshFeedback::Progress {
        message: format!("Reading {}", root.display()),
    });

    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let items = read_published_items(root, &|path| EntryOrigin::Fs {
        path: canonical.join(path.as_path()),
    })
    .map_err(|detail| (RepoFailureKind::Conflict, detail))?;

    Ok(items
        .into_iter()
        .filter(|item| !is_excluded_path(&item.origin, &exclusions.fs_paths))
        .collect())
}

/// Whether an origin's path lies under one of the excluded subtrees.
fn is_excluded_path(origin: &EntryOrigin, excluded: &[PathBuf]) -> bool {
    let EntryOrigin::Fs { path } = origin else {
        return false;
    };
    excluded.iter().any(|excluded| path.starts_with(excluded))
}

/// Reads a repository held by a remote, at the commit its configured ref
/// currently points at.
///
/// The clone is thrown away; what survives is the index it published and
/// the commit it was read at, which is what lets another machine read the
/// same bytes later.
fn read_git_repo(
    repo_url: &str,
    repo_ref: Option<&str>,
    peppy_dirs: &PeppyDirs,
    on_feedback: &mut dyn FnMut(RepoRefreshFeedback),
) -> ReadResult {
    let unreachable = |detail: String| (RepoFailureKind::Unreachable, detail);

    let tmp_dir = peppy_dirs.tmp_dir();
    std::fs::create_dir_all(&tmp_dir)
        .map_err(|e| unreachable(format!("failed to create tmp dir: {e}")))?;
    let tmp = tempfile::tempdir_in(&tmp_dir)
        .map_err(|e| unreachable(format!("failed to create temp dir: {e}")))?;

    let repo = clone_shallow(repo_url, repo_ref, tmp.path(), on_feedback).map_err(unreachable)?;
    let commit = head_commit(&repo).map_err(unreachable)?;
    // Recorded as configured rather than as resolved: it is what
    // `entry_belongs_to_repo` matches an entry back to its repository with,
    // and what a later fetch of the pinned commit starts from.
    let configured_ref = repo_ref.map(str::trim).filter(|r| !r.is_empty());

    read_published_items(tmp.path(), &|path| EntryOrigin::Git {
        repo_url: repo_url.to_owned(),
        repo_ref: configured_ref.unwrap_or_default().to_owned(),
        commit: commit.clone(),
        path: path.clone(),
    })
    .map_err(|detail| (RepoFailureKind::Conflict, detail))
}

/// The commit a fresh clone is sitting on.
fn head_commit(repo: &git2::Repository) -> std::result::Result<GitCommit, String> {
    let head = repo
        .head()
        .map_err(|e| format!("the clone has no HEAD to read a commit from: {e}"))?;
    let commit = head
        .peel_to_commit()
        .map_err(|e| format!("the clone's HEAD does not name a commit: {e}"))?;
    GitCommit::parse(&commit.id().to_string())
        .map_err(|e| format!("the clone's HEAD is not a usable commit: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::repo::cache::repositories_list_path;
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
        assert_eq!(nodes_hub.get("ref").unwrap().as_str().unwrap(), "main");

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

    fn write_launcher_json5(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(
            path,
            r#"{
  peppy_schema: "launcher/v1",
  deployments: []
}"#,
        )
        .unwrap();
    }

    fn write_contract_json5(path: &Path, name: &str, tag: &str) -> Vec<u8> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let body = format!(
            r#"{{
  peppy_schema: "contract/v1",
  manifest: {{ name: "{name}", tag: "{tag}" }},
  interfaces: {{}}
}}"#
        );
        std::fs::write(path, &body).unwrap();
        body.into_bytes()
    }

    /// Writes the index a repository publishes, the way `peppy repo index`
    /// does.
    ///
    /// A repository states what it holds by committing this file, so a test
    /// that writes a tree has to publish it before a refresh can read it.
    fn publish_repo(root: &Path) {
        let index = crate::services::repo::index::generate_repository_index(root)
            .expect("a well-formed test repository can be indexed");
        crate::services::repo::index::write_repository_index(root, &index)
            .expect("write repository index");
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
    /// This is what a broken repository looks like now that an index cannot
    /// claim an identity twice: the statement and the tree disagree.
    fn stale_index(root: &Path, removed_rel: &str) {
        publish_repo(root);
        std::fs::remove_file(root.join(removed_rel))
            .unwrap_or_else(|e| panic!("remove {removed_rel}: {e}"));
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
        process_refresh(peppy_dirs, now, on_feedback)
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
            if let Some(RepoSource::Fs(path)) = parse_repo_entry(&entry)
                && path.exists()
            {
                publish_repo(&path);
            }
        }
        process_refresh(peppy_dirs, now, on_feedback)
    }

    /// Helper: write a minimal valid peppy.json5 into `dir`.
    fn write_peppy_json5(dir: &Path, name: &str, tag: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(NODE_CONFIG_FILE),
            format!(
                r#"{{
  peppy_schema: "node/v1",
  manifest: {{ name: "{name}", tag: "{tag}" }},
  interfaces: {{}},
  execution: {{ language: "rust", build_cmd: ["true"], run_cmd: ["true"] }},
}}"#
            ),
        )
        .unwrap();
    }

    /// Helper: write a repositories.json5 file.
    fn write_repos(peppy_dirs: &PeppyDirs, content: &str) {
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(conf_dir.join("repositories.json5"), content).unwrap();
    }

    /// Helper: write an excluded_repositories.json5 file.
    fn write_excluded_repos(peppy_dirs: &PeppyDirs, content: &str) {
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(conf_dir.join("excluded_repositories.json5"), content).unwrap();
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
        write_peppy_json5(&healthy.join("first"), "first", "v1");
        write_peppy_json5(&broken.join("kept"), "kept", "v1");
        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}, {{ "id": 2, "type": "fs", "path": "{}" }}]"#,
                healthy.display(),
                broken.display()
            ),
        );

        // A clean run first, so the broken repository has something to
        // fall back to.
        let clean = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {}).unwrap();
        assert!(clean.failures.is_empty());
        write_all_caches(&peppy_dirs, &clean).unwrap();

        // Now break the second repository and add a node to the first.
        write_peppy_json5(&healthy.join("second"), "second", "v1");
        write_peppy_json5(&broken.join("gone"), "gone", "v1");
        stale_index(&broken, "gone/peppy.json5");

        let refreshed =
            refresh_publishing(&peppy_dirs, &[&healthy], TEST_NOW, &mut |_| {}).unwrap();

        let names: HashSet<&str> = refreshed
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
        assert_eq!(failure.kind, RepoFailureKind::Conflict);
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
        write_peppy_json5(&repo.join("first"), "first", "v1");
        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
                repo.display()
            ),
        );
        publish_repo(&repo);

        assert_eq!(reindex_after_change(&peppy_dirs).await, None);

        let cached = crate::services::repo::cache::load_repo_cache::<NodeCacheEntry>(&peppy_dirs)
            .expect("the re-index published the node cache");
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].node_name, "first");
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
            kind: RepoFailureKind::Unreachable,
            detail: "failed to connect".to_owned(),
            retained: 8,
        };
        let conflict = RepoFailure {
            id: 1000,
            label: "/home/user/workspace".to_owned(),
            kind: RepoFailureKind::Conflict,
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
        write_peppy_json5(&repo.join("node_a"), "node_a", "v1");
        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
                repo.display()
            ),
        );

        let first_read = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let clean = refresh_indexed(&peppy_dirs, first_read, &mut |_| {}).unwrap();
        write_all_caches(&peppy_dirs, &clean).unwrap();
        assert_eq!(clean.statuses.len(), 1);
        assert_eq!(clean.statuses[0].last_read_unix_secs, Some(1_000));
        assert!(!clean.statuses[0].is_retained());

        // Break it, and refresh much later.
        write_peppy_json5(&repo.join("vanished"), "vanished", "v1");
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
        assert_eq!(failure.kind, "conflict");
        assert_eq!(failure.unix_secs, 99_000, "the failure itself is recent");
    }

    /// A repository that recovers stops reporting its old failure, so a
    /// fixed problem does not linger in the diagnostics.
    #[test]
    fn process_refresh_clears_the_failure_once_a_repository_recovers() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let repo = tmp.path().join("repo");
        write_peppy_json5(&repo.join("vanished"), "vanished", "v1");
        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
                repo.display()
            ),
        );
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
        write_peppy_json5(&healthy.join("node_a"), "node_a", "v1");
        let gone = tmp.path().join("not-mounted");
        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}, {{ "id": 2, "type": "fs", "path": "{}" }}]"#,
                healthy.display(),
                gone.display()
            ),
        );

        let refreshed = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {}).unwrap();

        assert_eq!(refreshed.nodes.len(), 1, "the healthy repository updated");
        assert_eq!(refreshed.failures.len(), 1);
        assert_eq!(refreshed.failures[0].kind, RepoFailureKind::Unreachable);
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
        assert_eq!(refreshed.failures[0].kind, RepoFailureKind::Unreachable);
    }

    /// Several failures across several repositories come back in one
    /// pass, ordered by repository id, so one run tells the user
    /// everything they have to fix.
    #[test]
    fn process_refresh_reports_every_failure_in_repository_id_order() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let conflicted = tmp.path().join("conflicted");
        write_peppy_json5(&conflicted.join("a"), "vanished", "v1");
        stale_index(&conflicted, "a/peppy.json5");
        let gone = tmp.path().join("not-mounted");
        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 5, "type": "fs", "path": "{}" }}, {{ "id": 9, "type": "fs", "path": "{}" }}]"#,
                gone.display(),
                conflicted.display()
            ),
        );

        let refreshed = refresh_publishing(&peppy_dirs, &[], TEST_NOW, &mut |_| {}).unwrap();

        let seen: Vec<(u64, RepoFailureKind)> =
            refreshed.failures.iter().map(|f| (f.id, f.kind)).collect();
        assert_eq!(
            seen,
            vec![
                (5, RepoFailureKind::Unreachable),
                (9, RepoFailureKind::Conflict)
            ]
        );

        let report = failure_report(&refreshed.failures);
        assert!(report.contains("unreachable"), "{report}");
        assert!(report.contains("conflict"), "{report}");
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
        write_peppy_json5(&one.join("from_one"), "from_one", "v1");
        write_peppy_json5(&two.join("from_two"), "from_two", "v1");
        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}, {{ "id": 2, "type": "fs", "path": "{}" }}]"#,
                one.display(),
                two.display()
            ),
        );
        let clean = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {}).unwrap();
        write_all_caches(&peppy_dirs, &clean).unwrap();

        // Break repository 2 only.
        write_peppy_json5(&two.join("vanished"), "vanished", "v1");
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
            .nodes
            .iter()
            .filter(|n| n.origin.path_str().starts_with(two_root.to_string_lossy().as_ref()))
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
        write_peppy_json5(&repo_a.join("node_a"), "node_a", "v1");
        write_peppy_json5(&repo_b.join("node_b"), "node_b", "v1");

        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}, {{ "id": 2, "type": "fs", "path": "{}" }}]"#,
                repo_a.display(),
                repo_b.display()
            ),
        );
        write_excluded_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
                repo_b.display()
            ),
        );

        let RefreshedRepos {
            nodes: discovered,
            excluded,
            ..
        } = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {}).unwrap();
        assert_eq!(discovered.len(), 1, "only non-excluded repo nodes returned");
        assert_eq!(discovered[0].node_name, "node_a");
        assert_eq!(excluded.len(), 1, "one repo should be excluded");
        assert_eq!(excluded[0].source_type, RepoSourceKind::Fs);
        // `identity` is canonicalized by `json_entry_identity`; the test
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
        write_peppy_json5(&repo.join("keep_node"), "keep_node", "v1");
        write_peppy_json5(&repo.join("secret_node"), "secret_node", "v1");

        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
                repo.display()
            ),
        );
        // Exclude the subdirectory, not the whole repo
        write_excluded_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
                repo.join("secret_node").display()
            ),
        );

        let RefreshedRepos {
            nodes: discovered,
            excluded,
            ..
        } = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {}).unwrap();
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

    #[test]
    fn process_refresh_skips_excluded_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo = tmp.path().join("repo");
        write_peppy_json5(&repo.join("node_a"), "node_a", "v1");

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
            nodes: discovered,
            excluded,
            ..
        } = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {}).unwrap();
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
        write_peppy_json5(&repo.join("node_a"), "node_a", "v1");

        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
                repo.display()
            ),
        );

        // No excluded_repositories.json5 file
        let RefreshedRepos {
            nodes: discovered,
            excluded,
            ..
        } = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {}).unwrap();
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

        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
                repo.display()
            ),
        );

        let RefreshedRepos { contracts, .. } =
            refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {}).unwrap();
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
    /// the repo root, and `resolved_ref` records the branch that was
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

        let RefreshedRepos { contracts, .. } =
            refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {}).unwrap();
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

        write_repos(
            &peppy_dirs,
            &format!(
                r#"[
                    {{ "id": 1, "type": "fs", "path": "{}" }},
                    {{ "id": 2, "type": "fs", "path": "{}" }}
                ]"#,
                repo_a.display(),
                repo_b.display()
            ),
        );

        let mut feedbacks = Vec::new();
        let RefreshedRepos { contracts, .. } =
            refresh_indexed(&peppy_dirs, TEST_NOW, &mut |fb| feedbacks.push(fb)).unwrap();

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
        // with a launcher named the same in repo_b. Note that none of
        // them use the legacy `peppy_launcher.json5` name.
        write_launcher_json5(&repo_a.join("openarm01_sim_teleop.json5"));
        write_launcher_json5(&repo_a.join("calibration.json5"));
        write_launcher_json5(&repo_a.join("nested").join("demo.json5"));
        write_launcher_json5(&repo_b.join("openarm01_sim_teleop.json5"));
        // Throw a node into repo_a too so we exercise the mixed walk.
        write_peppy_json5(&repo_a.join("node_a"), "node_a", "v1");

        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}, {{ "id": 2, "type": "fs", "path": "{}" }}]"#,
                repo_a.display(),
                repo_b.display()
            ),
        );

        let RefreshedRepos {
            nodes: discovered,
            launchers,
            ..
        } = refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {}).unwrap();
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

        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
                repo.display()
            ),
        );

        let RefreshedRepos { launchers, .. } =
            refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {}).unwrap();
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

        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
                repo.display()
            ),
        );

        let RefreshedRepos { launchers, .. } =
            refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {}).unwrap();
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

        let RefreshedRepos { launchers, .. } =
            refresh_indexed(&peppy_dirs, TEST_NOW, &mut |_| {}).unwrap();
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
        assert_eq!(launcher.origin.path_str(), "openarm01/openarm01_teleop.json5");
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
            origin["commit"].as_str().expect("a commit is recorded").len(),
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
        write_peppy_json5(&repo.join("node_a"), "node_a", "v1");

        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
                repo.display()
            ),
        );

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
    /// local transport". The clone helper has to detect those URLs and skip
    /// the shallow option, otherwise the clone fails outright. These two
    /// tests lock in that fallback behavior so a regression that re-enables
    /// `depth(1)` for local URLs would fail fast.
    #[test]
    fn clone_shallow_falls_back_to_full_clone_for_file_url() {
        let src_tmp = tempfile::tempdir().unwrap();
        init_repo_with_commits(src_tmp.path(), 3);

        let dst_tmp = tempfile::tempdir().unwrap();
        let dst = dst_tmp.path().join("clone");
        let repo_url = format!("file://{}", src_tmp.path().display());

        let repo = clone_shallow(&repo_url, None, &dst, &mut |_| {})
            .expect("file:// clone should succeed (depth must be skipped)");

        assert!(
            !dst.join(".git/shallow").exists(),
            "file:// goes through libgit2's local transport which rejects \
             depth(1); .git/shallow should not exist"
        );
        assert_eq!(count_commits(&repo), 3);
    }

    #[test]
    fn clone_shallow_falls_back_to_full_clone_for_raw_path() {
        let src_tmp = tempfile::tempdir().unwrap();
        init_repo_with_commits(src_tmp.path(), 3);

        let dst_tmp = tempfile::tempdir().unwrap();
        let dst = dst_tmp.path().join("clone");
        let repo_url = src_tmp.path().display().to_string();

        let repo = clone_shallow(&repo_url, None, &dst, &mut |_| {})
            .expect("raw-path clone should succeed (depth must be skipped)");

        assert!(!dst.join(".git/shallow").exists());
        assert_eq!(count_commits(&repo), 3);
    }

    #[test]
    fn clone_shallow_ref_checkout_works_on_local_clone() {
        let src_tmp = tempfile::tempdir().unwrap();
        let commits = init_repo_with_commits(src_tmp.path(), 3);
        let target_sha = commits[1].to_string();

        let dst_tmp = tempfile::tempdir().unwrap();
        let dst = dst_tmp.path().join("clone");
        let repo_url = format!("file://{}", src_tmp.path().display());

        let repo = clone_shallow(&repo_url, Some(&target_sha), &dst, &mut |_| {})
            .expect("clone_shallow with ref should succeed");

        let head = repo.head().expect("head");
        let head_oid = head.target().expect("head oid");
        assert_eq!(head_oid.to_string(), target_sha);
    }

}
