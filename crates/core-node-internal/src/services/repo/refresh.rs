use crate::Result;
use crate::names;
use crate::services::action_loop::{GoalHandler, accept_goal, reject_goal, run_action_loop};
use crate::services::node::clone_with_progress;
use crate::services::node::gate::{Admission, ConcurrencyGate};
use crate::services::repo::cache::{
    DiscoveredEntry, InterfaceCacheEntry, LauncherCacheEntry, NodeCacheEntry, PairingCacheEntry,
    RepoCacheEntry, write_repo_cache,
};
use crate::services::repo::exclude::ExclusionSet;
use crate::services::repo::{normalize_repo_entries, source_identity};
use config::consts::NODE_CONFIG_FILE;
use config::fingerprint::fingerprint_for_bytes;
use config::node::NodeConfigParser;
use config::schema::PeppySchema;
use core_node_api::encoding::{
    RepoRefreshFeedback, RepoRefreshGoal, RepoRefreshGoalResponse, RepoRefreshResult, RepoSource,
    RepoSourceKind,
};
use daemon_config::consts::PeppyDirs;
use daemon_config::interface::PeppyInterfaceParser;
use daemon_config::launcher::PeppyLauncherParser;
use daemon_config::pairing::PeppyPairingParser;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::{ConcurrentAction, PendingGoal};
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

/// Directory names that should never be descended into while searching for
/// `peppy.json5` files.
pub(crate) const PRUNED_DIR_NAMES: &[&str] = &[
    ".git",
    ".peppy",
    "target",
    "node_modules",
    ".venv",
    "__pycache__",
];

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
        names::REPO_REFRESH_ACTION,
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
            identifier: "repo_refresh".to_string(),
            reason: e.to_string(),
        })
}

fn encode_refresh_rejected(reason: impl Into<String>) -> PeppyResult<Payload> {
    RepoRefreshGoalResponse::rejected(reason)
        .encode()
        .map_err(|e| PeppyError::InvalidServiceRequest {
            identifier: "repo_refresh".to_string(),
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
            let scan = tokio::task::spawn_blocking(move || {
                let _guard = crate::services::repo::refresh_lock().lock();
                let mut emit = |fb: RepoRefreshFeedback| {
                    let _ = tx.send(fb);
                };
                match process_refresh(&dirs, &mut emit) {
                    Ok(refreshed) => {
                        // Caches store every discovered entry so that
                        // `repo list` can display every source and users
                        // can pick a specific `sha256` when they need to.
                        write_all_caches(&dirs, &refreshed)?;
                        Ok((
                            count_unique(&refreshed.nodes),
                            count_unique(&refreshed.launchers),
                            count_unique(&refreshed.interfaces),
                            count_unique(&refreshed.pairings),
                            refreshed.excluded,
                        ))
                    }
                    Err(e) => Err(e),
                }
            })
            .await;

            let result = match scan {
                Ok(Ok((
                    unique_nodes,
                    unique_launchers,
                    unique_interfaces,
                    unique_pairings,
                    _excluded,
                ))) => RepoRefreshResult::success(
                    unique_nodes,
                    unique_launchers,
                    unique_interfaces,
                    unique_pairings,
                ),
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

/// A repository that was skipped during refresh because it appears in the
/// `excluded_repositories.json5` configuration.
#[derive(Debug, Clone)]
pub(crate) struct ExcludedRepo {
    pub(crate) source_type: RepoSourceKind,
    pub(crate) identity: String,
}

/// Aggregated output of [`process_refresh`]: all discovered entries
/// (nodes, launchers, interfaces) plus the repositories that were
/// skipped because they appear in the exclusion list.
pub(crate) struct RefreshedRepos {
    pub(crate) nodes: Vec<NodeCacheEntry>,
    pub(crate) launchers: Vec<LauncherCacheEntry>,
    pub(crate) interfaces: Vec<InterfaceCacheEntry>,
    pub(crate) pairings: Vec<PairingCacheEntry>,
    pub(crate) excluded: Vec<ExcludedRepo>,
}

/// Publishes every cache file from one refresh result. The four caches
/// must always move together: rewriting only a subset leaves the
/// untouched files still listing items from repositories that are no
/// longer configured.
pub(crate) fn write_all_caches(peppy_dirs: &PeppyDirs, refreshed: &RefreshedRepos) -> Result<()> {
    write_repo_cache(peppy_dirs, &refreshed.nodes)?;
    write_repo_cache(peppy_dirs, &refreshed.launchers)?;
    write_repo_cache(peppy_dirs, &refreshed.interfaces)?;
    write_repo_cache(peppy_dirs, &refreshed.pairings)
}

/// Source-and-file context shared by every `.json5` collector. The
/// collectors only need read access; bundling these arguments keeps
/// their signatures focused on the entry-specific state (seen set +
/// output vector).
struct EntryContext<'a> {
    root: &'a Path,
    source_type: RepoSourceKind,
    source_uri: Option<&'a str>,
    resolved_ref: Option<&'a str>,
    config_path: &'a Path,
    bytes: &'a [u8],
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
        "url" => {
            let url = entry.get("url")?.as_str()?.to_owned();
            Some(RepoSource::Url(url))
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
/// discovered nodes, launchers, interfaces, and the list of
/// repositories that were excluded.
///
/// Every discovered entry is kept in the result so the cache (and
/// `repo list`) can display every source. When several repositories
/// declare the same `(name, tag)`, lookup picks the lowest-id
/// repository at query time; the `sha256` on each entry lets users
/// distinguish entries with the same identity but different content.
/// Discovery feedback is emitted once per unique identity from the
/// highest-priority repository; extra entries are silently cached.
pub(crate) fn process_refresh(
    peppy_dirs: &PeppyDirs,
    on_feedback: &mut dyn FnMut(RepoRefreshFeedback),
) -> Result<RefreshedRepos> {
    let (repos, exclusions) = {
        let _guard = crate::services::repo::repos_file_lock().lock();
        let repos = read_or_create_repos(peppy_dirs)?;
        let exclusions = ExclusionSet::load(peppy_dirs);
        (repos, exclusions)
    };

    let mut global_seen_nodes: HashSet<(String, String)> = HashSet::new();
    let mut global_seen_launchers: HashSet<(String, String)> = HashSet::new();
    let mut global_seen_interfaces: HashSet<(String, String)> = HashSet::new();
    let mut global_seen_pairings: HashSet<(String, String)> = HashSet::new();
    let mut all_nodes: Vec<NodeCacheEntry> = Vec::new();
    let mut all_launchers: Vec<LauncherCacheEntry> = Vec::new();
    let mut all_interfaces: Vec<InterfaceCacheEntry> = Vec::new();
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
        let Some(source) = parse_repo_entry(entry) else {
            warn!("Skipping unrecognized repository entry: {:?}", entry);
            continue;
        };

        let identity = source_identity(&source);

        if exclusions.is_excluded(&identity) {
            debug!("Excluding {} repository: {}", source.kind(), identity);
            continue;
        }

        let walked = match source {
            RepoSource::Url(url) => {
                debug!("Skipping URL repository (not yet implemented): {}", url);
                continue;
            }
            RepoSource::Fs(path) => {
                if !path.exists() {
                    debug!("Skipping non-existent FS repository: {}", path.display());
                    continue;
                }
                on_feedback(RepoRefreshFeedback::Progress {
                    message: format!("Scanning {}", path.display()),
                });
                walk_directory(&path, RepoSourceKind::Fs, None, None, &exclusions.fs_paths)
            }
            RepoSource::Git { repo_url, repo_ref } => {
                let ref_suffix = repo_ref
                    .as_deref()
                    .map(|r| format!(" (ref {})", r))
                    .unwrap_or_default();
                on_feedback(RepoRefreshFeedback::Progress {
                    message: format!("Cloning {}{}", repo_url, ref_suffix),
                });
                match clone_and_walk_git_repo(
                    &repo_url,
                    repo_ref.as_deref(),
                    peppy_dirs,
                    on_feedback,
                ) {
                    Ok(walked) => walked,
                    Err(e) => {
                        warn!("Failed to refresh git repository {}: {}", repo_url, e);
                        continue;
                    }
                }
            }
        };

        merge_walked(
            walked.nodes,
            &mut global_seen_nodes,
            &mut all_nodes,
            on_feedback,
        );
        merge_walked(
            walked.launchers,
            &mut global_seen_launchers,
            &mut all_launchers,
            on_feedback,
        );
        merge_walked(
            walked.interfaces,
            &mut global_seen_interfaces,
            &mut all_interfaces,
            on_feedback,
        );
        merge_walked(
            walked.pairings,
            &mut global_seen_pairings,
            &mut all_pairings,
            on_feedback,
        );
    }

    Ok(RefreshedRepos {
        nodes: all_nodes,
        launchers: all_launchers,
        interfaces: all_interfaces,
        pairings: all_pairings,
        excluded: excluded_repos,
    })
}

/// Items discovered by walking a single repository's working tree.
pub(crate) struct WalkResult {
    pub nodes: Vec<NodeCacheEntry>,
    pub launchers: Vec<LauncherCacheEntry>,
    pub interfaces: Vec<InterfaceCacheEntry>,
    pub pairings: Vec<PairingCacheEntry>,
}

/// Appends one repository's walked entries to the running cross-repo
/// collection, emitting a `Discovered` feedback the first time each
/// `(name, tag)` identity is seen. Every entry is kept (including
/// same-identity duplicates from lower-priority repos); feedback fires
/// only for the highest-priority repository, which is walked first.
fn merge_walked<E: RepoCacheEntry>(
    walked: Vec<E>,
    global_seen: &mut HashSet<(String, String)>,
    all: &mut Vec<E>,
    on_feedback: &mut dyn FnMut(RepoRefreshFeedback),
) {
    for entry in walked {
        if global_seen.insert((entry.name().to_owned(), entry.tag().to_owned())) {
            on_feedback(RepoRefreshFeedback::Discovered {
                kind: E::ITEM_KIND,
                item_name: entry.name().to_owned(),
                item_tag: entry.tag().to_owned(),
                source_type: entry.source_type(),
                path: entry.path().to_owned(),
                sha256: entry.sha256().to_owned(),
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

/// Walk a directory looking for `peppy.json5` (node) and any `.json5`
/// file whose body declares `peppy_schema: "launcher/v1"` (launcher),
/// collecting discovered nodes and launchers.
///
/// Any directory whose path matches one of the `excluded_paths` entries is
/// pruned from the walk (neither descended into nor scanned for config files).
///
/// Each `.json5` file is read once. Files named `peppy.json5` are tried
/// as nodes first (preserves filename-driven node ergonomics); any
/// `.json5` whose body declares a `peppy_schema` value is dispatched to
/// the matching collector. Within a single repository walk, a given
/// `(name, tag)` (or launcher name) is collected only once; the
/// global cross-repo dedup happens in `process_refresh`.
pub(crate) fn walk_directory(
    root: &Path,
    source_type: RepoSourceKind,
    source_uri: Option<&str>,
    resolved_ref: Option<&str>,
    excluded_paths: &[PathBuf],
) -> WalkResult {
    // Canonicalize the root so that paths emitted by the walker share a
    // common prefix representation with the excluded paths (which come from
    // `ExclusionSet::load` already canonicalized). Without this, macOS
    // `/var/...` symlinks break subdirectory exclusion: the walker emits
    // `/var/...` while excluded paths resolve to `/private/var/...`.
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let excluded = excluded_paths.to_vec();
    let walker = ignore::WalkBuilder::new(&root)
        .hidden(true)
        .filter_entry(move |entry| {
            if !entry.file_type().is_some_and(|ft| ft.is_dir()) {
                return true;
            }
            if entry.depth() == 0 {
                return true;
            }
            let name = entry.file_name().to_string_lossy();
            if PRUNED_DIR_NAMES.iter().any(|pruned| name == *pruned) {
                return false;
            }
            let entry_path = entry.path();
            !excluded.iter().any(|exc| entry_path.starts_with(exc))
        })
        .build();

    let mut nodes_seen: HashSet<(String, String)> = HashSet::new();
    let mut launchers_seen: HashSet<(String, String)> = HashSet::new();
    let mut interfaces_seen: HashSet<(String, String)> = HashSet::new();
    let mut pairings_seen: HashSet<(String, String)> = HashSet::new();
    let mut nodes: Vec<NodeCacheEntry> = Vec::new();
    let mut launchers: Vec<LauncherCacheEntry> = Vec::new();
    let mut interfaces: Vec<InterfaceCacheEntry> = Vec::new();
    let mut pairings: Vec<PairingCacheEntry> = Vec::new();

    for entry in walker.flatten() {
        let file_name = entry.file_name().to_string_lossy();
        let config_path = entry.path();
        if !has_json5_extension(config_path) {
            continue;
        }
        let bytes = match std::fs::read(config_path) {
            Ok(bytes) if !bytes.is_empty() => bytes,
            Ok(_) => continue,
            Err(e) => {
                debug!(
                    "Skipping unreadable .json5 at {}: {}",
                    config_path.display(),
                    e
                );
                continue;
            }
        };
        let ctx = EntryContext {
            root: &root,
            source_type,
            source_uri,
            resolved_ref,
            config_path,
            bytes: &bytes,
        };
        if file_name == NODE_CONFIG_FILE {
            // Try node parse first to preserve the documented filename
            // convention for nodes. If the file's schema doesn't match,
            // fall through to the launcher/interface dispatch; that
            // way a non-node `peppy.json5` is still discoverable.
            if try_collect_node_entry(&ctx, &mut nodes_seen, &mut nodes) {
                continue;
            }
        }
        let Some(schema) = peek_peppy_schema(&bytes) else {
            continue;
        };
        match schema {
            PeppySchema::NodeV1 => {
                // A non-`peppy.json5` file declaring `node/v1` is unusual
                // but we still parse it strictly: matches the documented
                // "schema dispatch" rule for any `.json5`.
                try_collect_node_entry(&ctx, &mut nodes_seen, &mut nodes);
            }
            PeppySchema::LauncherV1 => {
                collect_launcher_entry(&ctx, &mut launchers_seen, &mut launchers);
            }
            PeppySchema::InterfaceV1 => {
                collect_interface_entry(&ctx, &mut interfaces_seen, &mut interfaces);
            }
            PeppySchema::PairingV1 => {
                collect_pairing_entry(&ctx, &mut pairings_seen, &mut pairings);
            }
        }
    }

    WalkResult {
        nodes,
        launchers,
        interfaces,
        pairings,
    }
}

fn has_json5_extension(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "json5")
}

/// Cheap schema sniff over the raw bytes. Returns `None` when the file
/// either doesn't declare a `peppy_schema` field or declares one we
/// don't know about; the caller treats both as "skip silently".
fn peek_peppy_schema(bytes: &[u8]) -> Option<PeppySchema> {
    #[derive(Deserialize)]
    struct SchemaPeek {
        peppy_schema: PeppySchema,
    }
    let content = std::str::from_utf8(bytes).ok()?;
    serde_json5::from_str::<SchemaPeek>(content)
        .ok()
        .map(|p| p.peppy_schema)
}

/// Shared body of the four collectors: UTF-8 check, strict parse via
/// `identity`, intra-repo dedup, then entry construction through
/// [`RepoCacheEntry::from_discovered`]. The strict parse catches
/// structural problems (unknown fields, malformed sections) that the
/// cheap schema peek can't.
///
/// `identity` returns the document's `(name, tag)` — `Ok(None)` to skip
/// the file silently (wrong schema variant, unusable file stem), or
/// `Err` when the content does not parse as `E`'s document kind at all.
/// `parse_failure_label` words that last case: a `peppy.json5` failing
/// the node parse is usually a different document kind rather than a
/// malformed node, so the node collector logs "non-node".
///
/// Returns `false` only on parse failure, so the node collector can
/// fall back to schema dispatch; intra-repo duplicates return `true`
/// because the file is a valid document of the kind.
fn collect_repo_entry<E: RepoCacheEntry>(
    ctx: &EntryContext<'_>,
    parse_failure_label: &str,
    seen: &mut HashSet<(String, String)>,
    out: &mut Vec<E>,
    identity: impl FnOnce(&str) -> std::result::Result<Option<(String, String)>, String>,
) -> bool {
    let content = match std::str::from_utf8(ctx.bytes) {
        Ok(s) => s,
        Err(e) => {
            debug!(
                "Skipping non-utf8 {} .json5 at {}: {}",
                E::KIND,
                ctx.config_path.display(),
                e
            );
            return false;
        }
    };
    let (name, tag) = match identity(content) {
        Ok(Some(identity)) => identity,
        Ok(None) => return true,
        Err(e) => {
            debug!(
                "Skipping {} .json5 at {}: {}",
                parse_failure_label,
                ctx.config_path.display(),
                e
            );
            return false;
        }
    };

    if !seen.insert((name.clone(), tag.clone())) {
        return true;
    }

    out.push(E::from_discovered(DiscoveredEntry {
        name,
        tag,
        sha256: fingerprint_for_bytes(ctx.bytes),
        path: relative_or_absolute_file_path(ctx.root, ctx.config_path, ctx.source_type),
        source_type: ctx.source_type,
        source_uri: ctx.source_uri.map(str::to_owned),
        resolved_ref: ctx.resolved_ref.map(str::to_owned),
    }));
    true
}

/// Returns `true` when the file parsed cleanly as a node and was
/// collected (or skipped because of an intra-repo duplicate). `false`
/// means parsing failed; the caller can fall back to a different
/// schema dispatch.
fn try_collect_node_entry(
    ctx: &EntryContext<'_>,
    seen: &mut HashSet<(String, String)>,
    nodes: &mut Vec<NodeCacheEntry>,
) -> bool {
    collect_repo_entry(ctx, "non-node", seen, nodes, |content| {
        let parsed = NodeConfigParser::from_content(content).map_err(|e| e.to_string())?;
        Ok(Some((
            parsed.manifest.name.as_str().to_string(),
            parsed.manifest.tag.clone(),
        )))
    })
}

fn collect_launcher_entry(
    ctx: &EntryContext<'_>,
    seen: &mut HashSet<(String, String)>,
    launchers: &mut Vec<LauncherCacheEntry>,
) {
    collect_repo_entry(ctx, "malformed launcher", seen, launchers, |content| {
        let parsed = PeppyLauncherParser::from_content(content).map_err(|e| e.to_string())?;
        if parsed.peppy_schema != PeppySchema::LauncherV1 {
            return Ok(None);
        }
        // Launcher name = basename without `.json5` (launcher documents
        // carry no manifest name). This matches `resolve_launcher_path`,
        // which appends `.json5` to a bare name when looking up a
        // launcher file.
        Ok(ctx
            .config_path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|stem| !stem.is_empty())
            .map(|stem| (stem.to_string(), String::new())))
    });
}

fn collect_interface_entry(
    ctx: &EntryContext<'_>,
    seen: &mut HashSet<(String, String)>,
    interfaces: &mut Vec<InterfaceCacheEntry>,
) {
    collect_repo_entry(ctx, "malformed interface", seen, interfaces, |content| {
        let parsed = PeppyInterfaceParser::from_content(content).map_err(|e| e.to_string())?;
        Ok(Some((
            parsed.manifest.name.as_str().to_string(),
            parsed.manifest.tag.clone(),
        )))
    });
}

fn collect_pairing_entry(
    ctx: &EntryContext<'_>,
    seen: &mut HashSet<(String, String)>,
    pairings: &mut Vec<PairingCacheEntry>,
) {
    collect_repo_entry(ctx, "malformed pairing", seen, pairings, |content| {
        let parsed = PeppyPairingParser::from_content(content).map_err(|e| e.to_string())?;
        Ok(Some((
            parsed.manifest.name.as_str().to_string(),
            parsed.manifest.tag.clone(),
        )))
    });
}

/// Returns the path of the manifest file itself (not its parent
/// directory). For git repos the result is relative to the repo root;
/// for fs repos it is the absolute path.
fn relative_or_absolute_file_path(
    root: &Path,
    config_path: &Path,
    source_type: RepoSourceKind,
) -> String {
    if source_type == RepoSourceKind::Git {
        config_path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        config_path.to_string_lossy().into_owned()
    }
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

/// Pick the ref string to persist in the cache so later batch installs can
/// re-fetch and re-check-out the same state.
///
/// `checkout_repo_ref` always detaches HEAD, which leaves `head().shorthand()`
/// equal to `"HEAD"` whenever the repo config pinned a ref; storing that
/// makes `add_batch` install the remote's default branch tip instead of the
/// pinned ref. Prefer the explicit config ref, then the cloned repo's
/// symbolic HEAD (for repos without a pin), and finally the commit OID.
fn resolve_ref_for_cache(repo: &git2::Repository, repo_ref: Option<&str>) -> String {
    if let Some(r) = repo_ref {
        let trimmed = r.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }

    if let Ok(head) = repo.head() {
        if let Ok(short) = head.shorthand()
            && short != "HEAD"
        {
            return short.to_owned();
        }
        if let Some(oid) = head.target() {
            return oid.to_string();
        }
    }

    "HEAD".to_owned()
}

fn clone_and_walk_git_repo(
    repo_url: &str,
    repo_ref: Option<&str>,
    peppy_dirs: &PeppyDirs,
    on_feedback: &mut dyn FnMut(RepoRefreshFeedback),
) -> std::result::Result<WalkResult, String> {
    let tmp_dir = peppy_dirs.tmp_dir();
    std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("failed to create tmp dir: {}", e))?;
    let tmp =
        tempfile::tempdir_in(&tmp_dir).map_err(|e| format!("failed to create temp dir: {}", e))?;

    let repo = clone_shallow(repo_url, repo_ref, tmp.path(), on_feedback)?;
    let resolved_ref = resolve_ref_for_cache(&repo, repo_ref);

    Ok(walk_directory(
        tmp.path(),
        RepoSourceKind::Git,
        Some(repo_url),
        Some(&resolved_ref),
        &[],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::repo::cache::repositories_list_path;

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
        } = process_refresh(&peppy_dirs, &mut |_| {}).unwrap();
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
        } = process_refresh(&peppy_dirs, &mut |_| {}).unwrap();
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
        } = process_refresh(&peppy_dirs, &mut |_| {}).unwrap();
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
        } = process_refresh(&peppy_dirs, &mut |_| {}).unwrap();
        assert_eq!(discovered.len(), 1, "node should be found normally");
        assert!(excluded.is_empty(), "no repos should be excluded");
    }

    #[test]
    fn process_refresh_skips_excluded_url_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo = tmp.path().join("repo");
        write_peppy_json5(&repo.join("node_a"), "node_a", "v1");

        write_repos(
            &peppy_dirs,
            &format!(
                r#"[
                    {{ "id": 1, "type": "fs", "path": "{}" }},
                    {{ "id": 2, "type": "url", "url": "https://example.com/packages" }}
                ]"#,
                repo.display()
            ),
        );
        write_excluded_repos(
            &peppy_dirs,
            r#"[{ "id": 1, "type": "url", "url": "https://example.com/packages" }]"#,
        );

        let RefreshedRepos {
            nodes: discovered,
            excluded,
            ..
        } = process_refresh(&peppy_dirs, &mut |_| {}).unwrap();
        assert_eq!(discovered.len(), 1, "FS node should still be found");
        assert_eq!(excluded.len(), 1, "url repo should be excluded");
        assert_eq!(excluded[0].source_type, RepoSourceKind::Url);
    }

    /// Helper: write a `.json5` launcher file at `path` (any name accepted).
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

    /// Helper: write a minimal valid interface manifest at `path`.
    fn write_interface_json5(path: &Path, name: &str, tag: &str) -> Vec<u8> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let body = format!(
            r#"{{
  peppy_schema: "interface/v1",
  manifest: {{ name: "{name}", tag: "{tag}" }},
  interfaces: {{}}
}}"#
        );
        std::fs::write(path, &body).unwrap();
        body.into_bytes()
    }

    /// FS-side interface discovery: an `interface/v1` document is
    /// recognized regardless of its filename, the cached `path` points
    /// at the manifest file itself, and `sha256` matches the raw bytes.
    #[test]
    fn process_refresh_discovers_interfaces_from_fs_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let repo = tmp.path().join("repo");
        let iface_path = repo.join("uvc_camera/peppy.json5");
        let bytes = write_interface_json5(&iface_path, "uvc_camera", "v1");

        write_repos(
            &peppy_dirs,
            &format!(
                r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
                repo.display()
            ),
        );

        let RefreshedRepos { interfaces, .. } = process_refresh(&peppy_dirs, &mut |_| {}).unwrap();
        assert_eq!(interfaces.len(), 1, "exactly one interface expected");
        let iface = &interfaces[0];
        assert_eq!(iface.interface_name, "uvc_camera");
        assert_eq!(iface.tag, "v1");
        assert_eq!(iface.source_type, RepoSourceKind::Fs);
        assert!(
            iface.path.ends_with("uvc_camera/peppy.json5"),
            "fs path should be absolute to the manifest file: {}",
            iface.path
        );
        assert_eq!(
            iface.sha256,
            fingerprint_for_bytes(&bytes),
            "cached sha256 must equal fingerprint_for_bytes of raw manifest bytes"
        );
    }

    /// Git-side interface discovery: the cached `path` is relative to
    /// the repo root, and `resolved_ref` records the branch that was
    /// cloned.
    #[test]
    fn process_refresh_discovers_interfaces_from_git_repo() {
        let src_tmp = tempfile::tempdir().unwrap();
        let src = src_tmp.path();
        let repo = git2::Repository::init(src).expect("init repo");
        let iface_rel = Path::new("uvc_camera/peppy.json5");
        write_interface_json5(&src.join(iface_rel), "uvc_camera", "v1");

        let signature =
            git2::Signature::now("Peppy", "peppy@example.com").expect("create signature");
        let mut index = repo.index().expect("open index");
        index.add_path(iface_rel).expect("stage interface");
        index.write().expect("write index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "add uvc_camera interface",
            &tree,
            &[],
        )
        .expect("commit");
        let branch = repo
            .head()
            .expect("head")
            .shorthand()
            .expect("shorthand")
            .to_owned();

        let peppy_tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(peppy_tmp.path());
        let repo_url = format!("file://{}", src.display());
        write_repos(
            &peppy_dirs,
            &format!(r#"[{{ "id": 1, "type": "git", "url": "{repo_url}", "ref": "{branch}" }}]"#,),
        );

        let RefreshedRepos { interfaces, .. } = process_refresh(&peppy_dirs, &mut |_| {}).unwrap();
        assert_eq!(interfaces.len(), 1, "exactly one interface expected");
        let iface = &interfaces[0];
        assert_eq!(iface.interface_name, "uvc_camera");
        assert_eq!(iface.tag, "v1");
        assert_eq!(iface.source_type, RepoSourceKind::Git);
        assert_eq!(iface.path, "uvc_camera/peppy.json5");
        assert_eq!(iface.resolved_ref.as_deref(), Some(branch.as_str()));
        assert!(
            !iface.sha256.is_empty(),
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
  peppy_schema: "interface/v1",
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
  peppy_schema: "interface/v1",
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
        let RefreshedRepos { interfaces, .. } =
            process_refresh(&peppy_dirs, &mut |fb| feedbacks.push(fb)).unwrap();

        assert_eq!(
            interfaces.len(),
            2,
            "both entries should be kept (sha256 disambiguates)"
        );
        let shas: HashSet<&str> = interfaces.iter().map(|i| i.sha256.as_str()).collect();
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

    /// `walk_directory` dispatches `.json5` files by `peppy_schema`:
    /// a node manifest, a launcher, and an interface coexisting in the
    /// same repository each land in the matching collector.
    #[test]
    fn walk_directory_dispatches_by_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("mixed");
        write_peppy_json5(&repo.join("nodes/my_sensor"), "my_sensor", "v1");
        write_launcher_json5(&repo.join("teleop.json5"));
        write_interface_json5(
            &repo.join("interfaces/uvc_camera.json5"),
            "uvc_camera",
            "v1",
        );

        let walked = walk_directory(&repo, RepoSourceKind::Fs, None, None, &[]);
        assert_eq!(walked.nodes.len(), 1, "one node");
        assert_eq!(walked.nodes[0].node_name, "my_sensor");
        assert_eq!(walked.launchers.len(), 1, "one launcher");
        assert_eq!(walked.launchers[0].launcher_name, "teleop");
        assert_eq!(walked.interfaces.len(), 1, "one interface");
        assert_eq!(walked.interfaces[0].interface_name, "uvc_camera");
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
        } = process_refresh(&peppy_dirs, &mut |_| {}).unwrap();
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
            demo.path.ends_with("demo.json5"),
            "launcher path should be the .json5 file itself: {}",
            demo.path
        );

        // Both `openarm01_sim_teleop` entries are present; the
        // repo_b one is the second occurrence.
        let dup: Vec<&LauncherCacheEntry> = launchers
            .iter()
            .filter(|l| l.launcher_name == "openarm01_sim_teleop")
            .collect();
        assert_eq!(dup.len(), 2);
        assert!(
            dup.iter().any(|l| l.path.contains("repo_a")),
            "primary entry should be from repo_a"
        );
        assert!(
            dup.iter().any(|l| l.path.contains("repo_b")),
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

        let RefreshedRepos { launchers, .. } = process_refresh(&peppy_dirs, &mut |_| {}).unwrap();
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

        let RefreshedRepos { launchers, .. } = process_refresh(&peppy_dirs, &mut |_| {}).unwrap();
        write_repo_cache(&peppy_dirs, &launchers).unwrap();

        let cache_path = launchers_repo_cache_path(&peppy_dirs);
        assert!(cache_path.exists(), "launcher cache should be written");

        let raw = std::fs::read_to_string(&cache_path).expect("read launcher cache");
        let parsed: serde_json::Value =
            serde_json5::from_str(&raw).expect("launcher cache should be valid JSON5");
        let arr = parsed.as_array().expect("expected JSON array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["launcher_name"], "openarm01_sim_teleop");
        assert_eq!(arr[0]["source_type"], "fs");
        let path_str = arr[0]["path"].as_str().expect("path should be a string");
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
        let signature =
            git2::Signature::now("Peppy", "peppy@example.com").expect("create signature");
        let mut index = repo.index().expect("open index");
        index
            .add_path(Path::new("openarm01/openarm01_teleop.json5"))
            .expect("stage launcher");
        index.write().expect("write index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "add openarm01_teleop launcher",
            &tree,
            &[],
        )
        .expect("commit");
        let branch = repo
            .head()
            .expect("head")
            .shorthand()
            .expect("shorthand")
            .to_owned();

        // Configure peppy with a single git repo entry pointing at the
        // local source via `file://`.
        let peppy_tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(peppy_tmp.path());
        let repo_url = format!("file://{}", src.display());
        write_repos(
            &peppy_dirs,
            &format!(r#"[{{ "id": 1, "type": "git", "url": "{repo_url}", "ref": "{branch}" }}]"#,),
        );

        let RefreshedRepos { launchers, .. } = process_refresh(&peppy_dirs, &mut |_| {}).unwrap();
        assert_eq!(launchers.len(), 1, "exactly one launcher expected");
        let launcher = &launchers[0];
        assert_eq!(launcher.launcher_name, "openarm01_teleop");
        assert_eq!(launcher.source_type, RepoSourceKind::Git);
        assert_eq!(launcher.source_uri.as_deref(), Some(repo_url.as_str()));
        assert_eq!(
            launcher.resolved_ref.as_deref(),
            Some(branch.as_str()),
            "resolved_ref should record the branch we cloned, not literal `HEAD`"
        );
        assert_eq!(launcher.path, "openarm01/openarm01_teleop.json5");
        assert!(
            !launcher.sha256.is_empty(),
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
        assert_eq!(entry["source_type"], "git");
        assert_eq!(entry["source_uri"], repo_url);
        assert_eq!(entry["resolved_ref"], branch);
        assert_eq!(entry["path"], "openarm01/openarm01_teleop.json5");
        assert!(
            entry.get("entry_type").is_none(),
            "entry_type should not be present in the on-disk schema"
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
        let _ = process_refresh(&peppy_dirs, &mut |fb| feedbacks.push(fb)).unwrap();

        let progress_messages: Vec<&str> = feedbacks
            .iter()
            .filter_map(|f| match f {
                RepoRefreshFeedback::Progress { message } => Some(message.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            progress_messages.iter().any(|m| m.starts_with("Scanning ")),
            "expected a 'Scanning …' progress feedback, got: {:?}",
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

    /// `checkout_repo_ref` detaches HEAD for every pinned ref, so
    /// `head().shorthand()` of the cloned repo is always `"HEAD"`. Guard
    /// against falling back to that literal; batch installs reuse
    /// `resolved_ref` as the fetch/checkout ref, so storing `"HEAD"`
    /// silently resolves to the remote's default branch instead of the
    /// pinned ref.
    #[test]
    fn resolve_ref_prefers_config_ref_over_detached_head() {
        let src_tmp = tempfile::tempdir().unwrap();
        let commits = init_repo_with_commits(src_tmp.path(), 2);
        let target_sha = commits[0].to_string();

        let dst_tmp = tempfile::tempdir().unwrap();
        let dst = dst_tmp.path().join("clone");
        let repo_url = format!("file://{}", src_tmp.path().display());

        let repo = clone_shallow(&repo_url, Some(&target_sha), &dst, &mut |_| {})
            .expect("clone_shallow with pinned commit should succeed");

        assert_eq!(
            repo.head().unwrap().shorthand(),
            Ok("HEAD"),
            "precondition: checkout_repo_ref always detaches HEAD"
        );

        let resolved = resolve_ref_for_cache(&repo, Some(&target_sha));
        assert_eq!(
            resolved, target_sha,
            "pinned commit ref must be preserved so batch installs fetch it back"
        );
    }

    #[test]
    fn resolve_ref_trims_and_rejects_empty_config_ref() {
        let src_tmp = tempfile::tempdir().unwrap();
        init_repo_with_commits(src_tmp.path(), 1);

        let dst_tmp = tempfile::tempdir().unwrap();
        let dst = dst_tmp.path().join("clone");
        let repo_url = format!("file://{}", src_tmp.path().display());

        let repo = clone_shallow(&repo_url, None, &dst, &mut |_| {})
            .expect("clone_shallow without ref should succeed");

        let short = repo
            .head()
            .unwrap()
            .shorthand()
            .expect("default branch shorthand")
            .to_owned();
        assert_ne!(short, "HEAD", "fresh clone without ref must stay attached");

        assert_eq!(
            resolve_ref_for_cache(&repo, Some("  v1.0  ")),
            "v1.0",
            "config ref should be trimmed"
        );
        assert_eq!(
            resolve_ref_for_cache(&repo, Some("")),
            short,
            "empty config ref should fall through to the attached branch name"
        );
        assert_eq!(
            resolve_ref_for_cache(&repo, None),
            short,
            "absent config ref should fall through to the attached branch name"
        );
    }

    #[test]
    fn resolve_ref_falls_back_to_commit_oid_when_detached_without_config_ref() {
        let src_tmp = tempfile::tempdir().unwrap();
        let commits = init_repo_with_commits(src_tmp.path(), 1);

        let dst_tmp = tempfile::tempdir().unwrap();
        let dst = dst_tmp.path().join("clone");
        let repo_url = format!("file://{}", src_tmp.path().display());

        let repo = clone_shallow(&repo_url, None, &dst, &mut |_| {})
            .expect("clone_shallow should succeed");
        repo.set_head_detached(commits[0])
            .expect("detach head for test");
        assert_eq!(repo.head().unwrap().shorthand(), Ok("HEAD"));

        assert_eq!(
            resolve_ref_for_cache(&repo, None),
            commits[0].to_string(),
            "detached HEAD with no config ref should record the commit OID"
        );
    }
}
