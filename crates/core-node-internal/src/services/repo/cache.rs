//! Typed loaders for the four repo caches written by `repo_refresh`:
//! `~/.peppy/cache/nodes.json5`, `launchers.json5`, `contracts.json5`,
//! and `pairings.json5`. Each file lists every item of its kind
//! discovered across every configured repository (FS, Git, or HTTP).
//! This module gives the rest of the daemon a typed view over those
//! entries so callers don't have to dig through `serde_json::Value`
//! every time.
//!
//! The four caches share one pipeline: every cache concern (atomic
//! write, load-time `repo_id` tagging, malformed-entry filtering,
//! repo-priority lookup, refresh-side collection) is implemented once,
//! generic over [`RepoCacheEntry`]. The entry structs stay distinct
//! types because each cache file has its own on-disk field names
//! (`node_name`, `launcher_name`, …), which must remain stable for
//! caches written by other peppy versions.
//!
//! Every entry carries a `sha256` of the raw manifest file bytes. Two
//! entries that share `(name, tag)` across repositories are kept side
//! by side in the cache (`sha256` differentiates their content); lookup
//! picks the entry from the lowest-id repository.
//!
//! Reads of the nodes cache are memoized by `(mtime-of-cache-file)` per
//! path so that a daemon hit by many `node add` / launch goals in a row
//! doesn't re-read and re-parse the cache file on every request.

use crate::Result;
use crate::services::repo::refresh::read_or_create_repos;
use core_node_api::encoding::{NodeSource, RepoItemKind, RepoSourceKind};
use daemon_config::consts::PeppyDirs;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;
use tracing::warn;

/// One entry as it appears in `nodes.json5`.
///
/// `path` points at the `peppy.json5` file itself (matching launcher
/// and contract semantics). Callers that need the containing directory
/// derive it via `Path::new(&entry.path).parent()`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NodeCacheEntry {
    pub node_name: String,
    pub node_tag: String,
    pub source_type: RepoSourceKind,
    /// Git repository URL or HTTP archive URL. `None` for FS entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_uri: Option<String>,
    /// Short ref name (branch/tag) actually checked out during the last
    /// refresh. `None` for FS and HTTP entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_ref: Option<String>,
    /// SHA-256 of the manifest file bytes. Acts as a content fingerprint
    /// so two entries that share `(name, tag)` across repositories can
    /// still be told apart by their content.
    #[serde(default)]
    pub sha256: String,
    /// Recorded SHA-256 for URL-kind entries, when the repository entry
    /// pinned one at registration time. `None` for FS and Git entries,
    /// and for URL entries whose repository did not declare a checksum.
    /// This is a different concept from `sha256`: `checksum` covers the
    /// HTTP archive integrity declared at repo registration; `sha256`
    /// fingerprints the manifest file content discovered at refresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    /// Absolute path (fs) or repo-relative (git) path to the
    /// `peppy.json5` manifest file.
    pub path: String,
    /// The id of the repository entry this node was discovered
    /// under (as read from `repositories.json5`). Derived at read time
    /// and never serialized back to disk.
    #[serde(skip)]
    pub repo_id: u32,
}

/// One entry as it appears in `launchers.json5`. Launchers live in the
/// same kind of repositories as nodes (FS or Git), but they don't carry
/// a tag; they're just the location of a launcher `.json5` file (any
/// filename; identified by `peppy_schema: "launcher/v1"` and keyed by
/// file stem).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LauncherCacheEntry {
    /// Name of the launcher (file stem of the `.json5` file).
    pub launcher_name: String,
    pub source_type: RepoSourceKind,
    /// Git repository URL or HTTP archive URL. `None` for FS entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_uri: Option<String>,
    /// Short ref name (branch/tag) actually checked out during the last
    /// refresh. `None` for FS and HTTP entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_ref: Option<String>,
    /// SHA-256 of the launcher manifest file bytes.
    #[serde(default)]
    pub sha256: String,
    /// Absolute path for FS entries; path-within-repo for Git entries,
    /// in both cases pointing at the `.json5` file itself.
    pub path: String,
    /// The id of the repository entry this launcher was discovered
    /// under (as read from `repositories.json5`). Derived at read time
    /// and never serialized back to disk.
    #[serde(skip)]
    pub repo_id: u32,
}

/// One entry as it appears in `contracts.json5`. Contracts are
/// stand-alone JSON5 documents (`peppy_schema: "contract/v1"`) that
/// describe a reusable contract of topics / services / actions. The
/// `sha256` of the manifest bytes is the primary way to disambiguate
/// entries that share `(name, tag)`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContractCacheEntry {
    pub contract_name: String,
    pub tag: String,
    /// SHA-256 of the manifest file bytes.
    pub sha256: String,
    pub source_type: RepoSourceKind,
    /// Git repository URL. `None` for FS entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_uri: Option<String>,
    /// Short ref name (branch/tag) actually checked out during the last
    /// refresh. `None` for FS entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_ref: Option<String>,
    /// Absolute path (fs) or repo-relative (git) path to the contract
    /// manifest file.
    pub path: String,
    /// The id of the repository entry this contract was discovered
    /// under. Derived at read time and never serialized back to disk.
    #[serde(skip)]
    pub repo_id: u32,
}

/// One entry as it appears in `pairings.json5`. Pairings are stand-alone
/// JSON5 documents (`peppy_schema: "pairing/v1"`) describing a two-role,
/// topics-only conversation contract. The `sha256` of the manifest bytes
/// is the primary way to disambiguate entries that share `(name, tag)`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PairingCacheEntry {
    pub pairing_name: String,
    pub tag: String,
    /// SHA-256 of the manifest file bytes.
    pub sha256: String,
    pub source_type: RepoSourceKind,
    /// Git repository URL. `None` for FS entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_uri: Option<String>,
    /// Short ref name (branch/tag) actually checked out during the last
    /// refresh. `None` for FS entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_ref: Option<String>,
    /// Absolute path (fs) or repo-relative (git) path to the pairing
    /// manifest file.
    pub path: String,
    /// The id of the repository entry this pairing was discovered under.
    /// Derived at read time and never serialized back to disk.
    #[serde(skip)]
    pub repo_id: u32,
}

/// Kind-independent fields of a just-discovered cache entry, produced
/// by the shared collector in `refresh.rs` and turned into a concrete
/// entry by [`RepoCacheEntry::from_discovered`]. `tag` is empty for
/// untagged kinds (launchers).
pub(crate) struct DiscoveredEntry {
    pub name: String,
    pub tag: String,
    pub sha256: String,
    pub path: String,
    pub source_type: RepoSourceKind,
    pub source_uri: Option<String>,
    pub resolved_ref: Option<String>,
}

/// Uniform view over the four cache-entry kinds, so the cache plumbing
/// (write/load/lookup here, collection and cross-repo merging in
/// `refresh.rs`) exists once instead of once per kind.
pub(crate) trait RepoCacheEntry:
    serde::Serialize + serde::de::DeserializeOwned + Clone
{
    /// Singular kind label used in error and log messages.
    const KIND: &'static str;
    /// Cache filename under the peppy cache dir.
    const FILE_NAME: &'static str;
    /// Wire-level kind reported in `repo refresh` discovery feedback.
    const ITEM_KIND: RepoItemKind;

    fn from_discovered(d: DiscoveredEntry) -> Self;

    fn name(&self) -> &str;
    /// Empty for untagged kinds (launchers), so `(name, tag)` is the
    /// identity key for every kind.
    fn tag(&self) -> &str;
    fn sha256(&self) -> &str;
    fn source_type(&self) -> RepoSourceKind;
    fn source_uri(&self) -> Option<&str>;
    fn resolved_ref(&self) -> Option<&str>;
    fn path(&self) -> &str;
    fn repo_id(&self) -> u32;
    fn set_repo_id(&mut self, id: u32);

    /// Whether the entry carries every field its kind requires; entries
    /// failing this (hand-edited or corrupt cache lines) are dropped
    /// with a warning at load time.
    fn is_well_formed(&self) -> bool;

    /// `name:tag` label for log messages (bare name for untagged kinds).
    fn display_id(&self) -> String {
        if self.tag().is_empty() {
            self.name().to_owned()
        } else {
            format!("{}:{}", self.name(), self.tag())
        }
    }
}

impl RepoCacheEntry for NodeCacheEntry {
    const KIND: &'static str = "node";
    const FILE_NAME: &'static str = "nodes.json5";
    const ITEM_KIND: RepoItemKind = RepoItemKind::Node;

    fn from_discovered(d: DiscoveredEntry) -> Self {
        Self {
            node_name: d.name,
            node_tag: d.tag,
            source_type: d.source_type,
            source_uri: d.source_uri,
            resolved_ref: d.resolved_ref,
            sha256: d.sha256,
            checksum: None,
            path: d.path,
            repo_id: 0,
        }
    }

    fn name(&self) -> &str {
        &self.node_name
    }
    fn tag(&self) -> &str {
        &self.node_tag
    }
    fn sha256(&self) -> &str {
        &self.sha256
    }
    fn source_type(&self) -> RepoSourceKind {
        self.source_type
    }
    fn source_uri(&self) -> Option<&str> {
        self.source_uri.as_deref()
    }
    fn resolved_ref(&self) -> Option<&str> {
        self.resolved_ref.as_deref()
    }
    fn path(&self) -> &str {
        &self.path
    }
    fn repo_id(&self) -> u32 {
        self.repo_id
    }
    fn set_repo_id(&mut self, id: u32) {
        self.repo_id = id;
    }
    // No sha256 requirement: node caches written before fingerprinting
    // existed are still valid (`sha256` deserializes to "" by default).
    fn is_well_formed(&self) -> bool {
        !self.node_name.is_empty() && !self.node_tag.is_empty() && !self.path.is_empty()
    }
}

impl RepoCacheEntry for LauncherCacheEntry {
    const KIND: &'static str = "launcher";
    const FILE_NAME: &'static str = "launchers.json5";
    const ITEM_KIND: RepoItemKind = RepoItemKind::Launcher;

    fn from_discovered(d: DiscoveredEntry) -> Self {
        Self {
            launcher_name: d.name,
            source_type: d.source_type,
            source_uri: d.source_uri,
            resolved_ref: d.resolved_ref,
            sha256: d.sha256,
            path: d.path,
            repo_id: 0,
        }
    }

    fn name(&self) -> &str {
        &self.launcher_name
    }
    fn tag(&self) -> &str {
        ""
    }
    fn sha256(&self) -> &str {
        &self.sha256
    }
    fn source_type(&self) -> RepoSourceKind {
        self.source_type
    }
    fn source_uri(&self) -> Option<&str> {
        self.source_uri.as_deref()
    }
    fn resolved_ref(&self) -> Option<&str> {
        self.resolved_ref.as_deref()
    }
    fn path(&self) -> &str {
        &self.path
    }
    fn repo_id(&self) -> u32 {
        self.repo_id
    }
    fn set_repo_id(&mut self, id: u32) {
        self.repo_id = id;
    }
    fn is_well_formed(&self) -> bool {
        !self.launcher_name.is_empty() && !self.path.is_empty()
    }
}

impl RepoCacheEntry for ContractCacheEntry {
    const KIND: &'static str = "contract";
    const FILE_NAME: &'static str = "contracts.json5";
    const ITEM_KIND: RepoItemKind = RepoItemKind::Contract;

    fn from_discovered(d: DiscoveredEntry) -> Self {
        Self {
            contract_name: d.name,
            tag: d.tag,
            sha256: d.sha256,
            source_type: d.source_type,
            source_uri: d.source_uri,
            resolved_ref: d.resolved_ref,
            path: d.path,
            repo_id: 0,
        }
    }

    fn name(&self) -> &str {
        &self.contract_name
    }
    fn tag(&self) -> &str {
        &self.tag
    }
    fn sha256(&self) -> &str {
        &self.sha256
    }
    fn source_type(&self) -> RepoSourceKind {
        self.source_type
    }
    fn source_uri(&self) -> Option<&str> {
        self.source_uri.as_deref()
    }
    fn resolved_ref(&self) -> Option<&str> {
        self.resolved_ref.as_deref()
    }
    fn path(&self) -> &str {
        &self.path
    }
    fn repo_id(&self) -> u32 {
        self.repo_id
    }
    fn set_repo_id(&mut self, id: u32) {
        self.repo_id = id;
    }
    // `sha256` is required: it is the primary way sha-pinned syncs
    // disambiguate same-`(name, tag)` entries.
    fn is_well_formed(&self) -> bool {
        !self.contract_name.is_empty()
            && !self.tag.is_empty()
            && !self.path.is_empty()
            && !self.sha256.is_empty()
    }
}

impl RepoCacheEntry for PairingCacheEntry {
    const KIND: &'static str = "pairing";
    const FILE_NAME: &'static str = "pairings.json5";
    const ITEM_KIND: RepoItemKind = RepoItemKind::Pairing;

    fn from_discovered(d: DiscoveredEntry) -> Self {
        Self {
            pairing_name: d.name,
            tag: d.tag,
            sha256: d.sha256,
            source_type: d.source_type,
            source_uri: d.source_uri,
            resolved_ref: d.resolved_ref,
            path: d.path,
            repo_id: 0,
        }
    }

    fn name(&self) -> &str {
        &self.pairing_name
    }
    fn tag(&self) -> &str {
        &self.tag
    }
    fn sha256(&self) -> &str {
        &self.sha256
    }
    fn source_type(&self) -> RepoSourceKind {
        self.source_type
    }
    fn source_uri(&self) -> Option<&str> {
        self.source_uri.as_deref()
    }
    fn resolved_ref(&self) -> Option<&str> {
        self.resolved_ref.as_deref()
    }
    fn path(&self) -> &str {
        &self.path
    }
    fn repo_id(&self) -> u32 {
        self.repo_id
    }
    fn set_repo_id(&mut self, id: u32) {
        self.repo_id = id;
    }
    fn is_well_formed(&self) -> bool {
        !self.pairing_name.is_empty()
            && !self.tag.is_empty()
            && !self.path.is_empty()
            && !self.sha256.is_empty()
    }
}

/// Path of `E`'s cache file under the peppy cache dir.
pub(crate) fn repo_cache_path<E: RepoCacheEntry>(peppy_dirs: &PeppyDirs) -> PathBuf {
    peppy_dirs.cache_dir().join(E::FILE_NAME)
}

/// Serializes `entries` to `E`'s cache file. Atomic via
/// [`daemon_config::atomic_write::publish_atomic`] so concurrent readers
/// never observe a partial file.
pub(crate) fn write_repo_cache<E: RepoCacheEntry>(
    peppy_dirs: &PeppyDirs,
    entries: &[E],
) -> Result<()> {
    let content = json5_pretty::to_string_pretty(entries).map_err(|e| {
        core_node_api::Error::Encoding(format!("failed to serialize {} cache: {e}", E::KIND))
    })?;
    daemon_config::atomic_write::publish_atomic(&repo_cache_path::<E>(peppy_dirs), |tmp| {
        std::fs::write(tmp, &content)
    })?;
    Ok(())
}

/// Reads `E`'s cache file, tags each entry with the `repo_id` of its
/// originating repository entry (derived from `repositories.json5` at
/// read time; never serialized back), and drops malformed entries with
/// a warning. Missing repository matches default to id 0 (highest
/// priority) to preserve behavior for hand-written caches. Returns an
/// empty vec when the file is missing.
pub(crate) fn load_repo_cache<E: RepoCacheEntry>(peppy_dirs: &PeppyDirs) -> Result<Vec<E>> {
    let path = repo_cache_path::<E>(peppy_dirs);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(&path)?;
    let raw: Vec<E> = serde_json5::from_str(&content).map_err(|e| {
        core_node_api::Error::Decoding(format!(
            "failed to parse {} cache at {}: {e}",
            E::KIND,
            path.display()
        ))
    })?;

    let repos = read_or_create_repos(peppy_dirs)?;
    let entries = raw
        .into_iter()
        .map(|mut e| {
            let id = lookup_repo_id(&repos, e.source_type(), e.source_uri(), e.path());
            e.set_repo_id(id);
            e
        })
        .filter(|e| {
            let ok = e.is_well_formed();
            if !ok {
                warn!(
                    "Skipping malformed {} entry: {}",
                    E::FILE_NAME,
                    e.display_id()
                );
            }
            ok
        })
        .collect();
    Ok(entries)
}

/// Returns the highest-priority (lowest `repo_id`) entry matching
/// `(name, tag)`, or `None`. The repo-priority tiebreak lives here so
/// all four caches resolve cross-repo name collisions identically.
/// Untagged kinds (launchers) match with `tag = ""`.
pub(crate) fn lookup_repo_entry<'a, E: RepoCacheEntry>(
    entries: &'a [E],
    name: &str,
    tag: &str,
) -> Option<&'a E> {
    entries
        .iter()
        .filter(|e| e.name() == name && e.tag() == tag)
        .min_by_key(|e| e.repo_id())
}

/// Returns the entry whose `(name, tag, sha256)` triple matches
/// exactly, bypassing the repo-priority tiebreak. Use when the caller
/// wants a specific manifest content rather than the
/// first-in-priority-order pick.
pub(crate) fn lookup_repo_entry_by_sha256<'a, E: RepoCacheEntry>(
    entries: &'a [E],
    name: &str,
    tag: &str,
    sha256: &str,
) -> Option<&'a E> {
    entries
        .iter()
        .find(|e| e.name() == name && e.tag() == tag && e.sha256() == sha256)
}

/// Reads the cache file plus the `nodes.json5` generation used for
/// the read.
pub fn load_with_generation(
    peppy_dirs: &PeppyDirs,
) -> Result<(Vec<NodeCacheEntry>, Option<SystemTime>)> {
    let path = repo_cache_path::<NodeCacheEntry>(peppy_dirs);
    let generation = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok());
    // `repo_id` on each cached entry is derived from `repositories.json5`,
    // so the memo must invalidate when that file changes too (not just
    // when nodes.json5 is rewritten). Missing file → UNIX_EPOCH so any
    // future appearance counts as a change.
    let repos_mtime = repositories_mtime(peppy_dirs);

    if let Some(mtime) = generation
        && let Some(cached) = memo_get(&path, mtime, repos_mtime)
    {
        return Ok(((*cached).clone(), Some(mtime)));
    }

    let entries = load_repo_cache::<NodeCacheEntry>(peppy_dirs)?;

    if let Some(mtime) = generation {
        memo_put(&path, mtime, repos_mtime, entries.clone());
    }
    Ok((entries, generation))
}

fn repositories_mtime(peppy_dirs: &PeppyDirs) -> SystemTime {
    let path = repositories_list_path(peppy_dirs);
    std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn lookup_repo_id(
    repos: &[serde_json::Value],
    source_type: RepoSourceKind,
    uri: Option<&str>,
    path: &str,
) -> u32 {
    for entry in repos {
        let Some(typ) = entry.get("type").and_then(|v| v.as_str()) else {
            continue;
        };
        let matches = match source_type {
            RepoSourceKind::Fs if typ == "fs" => entry
                .get("path")
                .and_then(|v| v.as_str())
                .is_some_and(|p| Path::new(path).starts_with(Path::new(p))),
            RepoSourceKind::Git if typ == "git" => entry.get("url").and_then(|v| v.as_str()) == uri,
            RepoSourceKind::Url if typ == "url" => entry.get("url").and_then(|v| v.as_str()) == uri,
            _ => false,
        };
        if matches {
            let id = entry.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            return u32::try_from(id).unwrap_or(0);
        }
    }
    0
}

/// Returns the highest-priority (lowest `repo_id`) node entry for
/// `(name, tag)`. Returns `None` when no entry matches.
pub fn lookup<'a>(
    entries: &'a [NodeCacheEntry],
    name: &str,
    tag: &str,
) -> Option<&'a NodeCacheEntry> {
    lookup_repo_entry(entries, name, tag)
}

/// Returns the highest-priority (lowest `repo_id`) launcher entry
/// matching `name`. Returns `None` when no entry matches.
pub fn lookup_launcher<'a>(
    entries: &'a [LauncherCacheEntry],
    name: &str,
) -> Option<&'a LauncherCacheEntry> {
    lookup_repo_entry(entries, name, "")
}

/// Returns the highest-priority (lowest `repo_id`) contract entry
/// matching `(name, tag)`. Returns `None` when no entry matches.
pub fn lookup_contract<'a>(
    entries: &'a [ContractCacheEntry],
    name: &str,
    tag: &str,
) -> Option<&'a ContractCacheEntry> {
    lookup_repo_entry(entries, name, tag)
}

/// Returns the contract entry whose `(name, tag, sha256)` triple
/// matches exactly. Returns `None` when no entry matches.
pub fn lookup_contract_by_sha256<'a>(
    entries: &'a [ContractCacheEntry],
    name: &str,
    tag: &str,
    sha256: &str,
) -> Option<&'a ContractCacheEntry> {
    lookup_repo_entry_by_sha256(entries, name, tag, sha256)
}

/// Returns the highest-priority (lowest `repo_id`) pairing entry
/// matching `(name, tag)`. Returns `None` when no entry matches.
pub fn lookup_pairing<'a>(
    entries: &'a [PairingCacheEntry],
    name: &str,
    tag: &str,
) -> Option<&'a PairingCacheEntry> {
    lookup_repo_entry(entries, name, tag)
}

/// Returns the pairing entry whose `(name, tag, sha256)` triple matches
/// exactly. Returns `None` when no entry matches.
pub fn lookup_pairing_by_sha256<'a>(
    entries: &'a [PairingCacheEntry],
    name: &str,
    tag: &str,
    sha256: &str,
) -> Option<&'a PairingCacheEntry> {
    lookup_repo_entry_by_sha256(entries, name, tag, sha256)
}

pub fn nodes_repo_cache_path(peppy_dirs: &PeppyDirs) -> PathBuf {
    repo_cache_path::<NodeCacheEntry>(peppy_dirs)
}

pub fn launchers_repo_cache_path(peppy_dirs: &PeppyDirs) -> PathBuf {
    repo_cache_path::<LauncherCacheEntry>(peppy_dirs)
}

pub fn contracts_repo_cache_path(peppy_dirs: &PeppyDirs) -> PathBuf {
    repo_cache_path::<ContractCacheEntry>(peppy_dirs)
}

pub fn pairings_repo_cache_path(peppy_dirs: &PeppyDirs) -> PathBuf {
    repo_cache_path::<PairingCacheEntry>(peppy_dirs)
}

pub fn repositories_list_path(peppy_dirs: &PeppyDirs) -> PathBuf {
    peppy_dirs.conf_dir().join("repositories.json5")
}

/// Reads `contracts.json5` (no memoization: contract resolution is a
/// sync-time event, not a hot path).
pub fn load_contract_cache(peppy_dirs: &PeppyDirs) -> Result<Vec<ContractCacheEntry>> {
    load_repo_cache(peppy_dirs)
}

/// Reads `pairings.json5` (no memoization: pairing resolution is a
/// sync-time event, not a hot path).
pub fn load_pairing_cache(peppy_dirs: &PeppyDirs) -> Result<Vec<PairingCacheEntry>> {
    load_repo_cache(peppy_dirs)
}

/// Looks up `(name, tag)` in the nodes cache and translates the matched
/// entry into a concrete `NodeSource` (Fs / Git / Http) that downstream
/// resolution can handle directly.
pub(crate) fn resolve_repo_node_source(
    name: &str,
    tag: &str,
    peppy_dirs: &PeppyDirs,
) -> std::result::Result<NodeSource, String> {
    let (entries, _) =
        load_with_generation(peppy_dirs).map_err(|e| format!("failed to load nodes cache: {e}"))?;
    let id = format!("{name}:{tag}");
    let entry = lookup(&entries, name, tag).ok_or_else(|| {
        format!(
            "repo-node `{id}` not found in {}",
            nodes_repo_cache_path(peppy_dirs).display()
        )
    })?;

    match entry.source_type {
        RepoSourceKind::Fs => {
            // `entry.path` points at the manifest file; the source
            // root is its parent directory.
            let dir = Path::new(&entry.path).parent().ok_or_else(|| {
                format!(
                    "fs cache entry for `{id}` has no parent directory in path {:?}",
                    entry.path
                )
            })?;
            Ok(NodeSource::Fs(dir.to_path_buf()))
        }
        RepoSourceKind::Git => {
            let repo_url_str = entry
                .source_uri
                .as_deref()
                .ok_or_else(|| format!("cache entry for `{id}` is git but has no source_uri"))?;
            let repo_url = gix_url::Url::try_from(repo_url_str)
                .map_err(|e| format!("invalid git URL in cache entry for `{id}`: {e}"))?;
            let repo_ref = entry
                .resolved_ref
                .clone()
                .ok_or_else(|| format!("cache entry for `{id}` is git but has no resolved_ref"))?;
            // `entry.path` is the repo-relative manifest path; the
            // source root is its parent directory.
            let repo_path = Path::new(&entry.path)
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .ok_or_else(|| {
                    format!(
                        "git cache entry for `{id}` has no parent directory in path {:?}",
                        entry.path
                    )
                })?;
            Ok(NodeSource::Git {
                repo_url,
                repo_path,
                repo_ref: Some(repo_ref),
            })
        }
        RepoSourceKind::Url => {
            let url_str = entry
                .source_uri
                .as_deref()
                .ok_or_else(|| format!("cache entry for `{id}` is url but has no source_uri"))?;
            let url = url::Url::parse(url_str)
                .map_err(|e| format!("invalid url in cache entry for `{id}`: {e}"))?;
            Ok(NodeSource::Http {
                url,
                sha256: entry.checksum.clone(),
            })
        }
    }
}

/// Looks up `name` in the launcher cache and resolves it to a concrete
/// on-disk path that the launch flow can open and parse.
///
/// For Git entries this materializes the repo's checkout via
/// [`crate::services::node::cache::git::ensure_checkout`] (blocking; wrap
/// callers in `spawn_blocking` when running inside Tokio). `on_feedback`
/// receives clone/refresh progress lines.
pub(crate) fn resolve_repo_launcher_path(
    name: &str,
    peppy_dirs: &PeppyDirs,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<PathBuf, String> {
    let entries: Vec<LauncherCacheEntry> =
        load_repo_cache(peppy_dirs).map_err(|e| format!("failed to load launcher cache: {e}"))?;

    let entry = lookup_launcher(&entries, name).ok_or_else(|| {
        format!(
            "launcher `{name}` not found in {}",
            launchers_repo_cache_path(peppy_dirs).display()
        )
    })?;

    resolve_cached_artifact_path(
        peppy_dirs,
        entry.source_type,
        entry.source_uri.as_deref(),
        entry.resolved_ref.as_deref(),
        &entry.path,
        on_feedback,
    )
    .map_err(|e| format!("launcher `{name}`: {e}"))
}

/// Translates a cache entry's `(source_type, source_uri, resolved_ref, path)`
/// tuple into a concrete on-disk path. Shared between launcher and contract
/// resolution so the Fs/Git/Url branching lives in one place.
///
/// For Git entries this materializes the repo's checkout via
/// [`crate::services::node::cache::git::ensure_checkout`] (blocking; wrap
/// callers in `spawn_blocking` when running inside Tokio) and joins the
/// repo-relative `path` onto the checkout dir. `on_feedback` receives
/// clone/refresh progress lines.
///
/// Errors are artifact-agnostic (no "launcher" / "contract" wording); the
/// caller is expected to `map_err` with its own context prefix.
pub(crate) fn resolve_cached_artifact_path(
    peppy_dirs: &PeppyDirs,
    source_type: RepoSourceKind,
    source_uri: Option<&str>,
    resolved_ref: Option<&str>,
    path: &str,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<PathBuf, String> {
    match source_type {
        RepoSourceKind::Fs => Ok(PathBuf::from(path)),
        RepoSourceKind::Git => {
            let repo_url =
                source_uri.ok_or_else(|| "git cache entry missing source_uri".to_string())?;
            let repo_ref =
                resolved_ref.ok_or_else(|| "git cache entry missing resolved_ref".to_string())?;
            let checkout = crate::services::node::cache::git::ensure_checkout(
                peppy_dirs,
                repo_url,
                Some(repo_ref),
                None,
                on_feedback,
            )?;
            Ok(checkout.join(path))
        }
        RepoSourceKind::Url => Err("url-sourced cache entry is not yet supported".to_string()),
    }
}

/// Borrowed view over the entry fields that [`resolve_cached_doc`]
/// needs, available for every cache-entry kind via [`RepoCacheEntry`].
pub(crate) struct CachedDocEntryRef<'a> {
    pub sha256: &'a str,
    pub source_type: RepoSourceKind,
    pub source_uri: Option<&'a str>,
    pub resolved_ref: Option<&'a str>,
    pub path: &'a str,
}

impl<'a, E: RepoCacheEntry> From<&'a E> for CachedDocEntryRef<'a> {
    fn from(e: &'a E) -> Self {
        Self {
            sha256: e.sha256(),
            source_type: e.source_type(),
            source_uri: e.source_uri(),
            resolved_ref: e.resolved_ref(),
            path: e.path(),
        }
    }
}

/// Shared tail of the contract/pairing document resolvers: turns a cache
/// lookup result into a parsed document — resolves the entry's on-disk
/// path, reads the bytes, rejects fingerprint drift, and hands the UTF-8
/// content to `parse`. `kind` labels every error ("contract" /
/// "pairing") and `id` is the document's `name:tag` label; `entry: None`
/// produces the cache-miss error (naming the sha pin when one was set).
pub(crate) fn resolve_cached_doc<T>(
    peppy_dirs: &PeppyDirs,
    kind: &str,
    id: &str,
    sha256_pin: Option<&str>,
    entry: Option<CachedDocEntryRef<'_>>,
    parse: impl FnOnce(&str) -> std::result::Result<T, String>,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<T, String> {
    let entry = entry.ok_or_else(|| match sha256_pin {
        Some(sha) => format!(
            "{kind} `{id}` (sha256 `{sha}`) not in {kind} cache; \
             run `peppy repo refresh`"
        ),
        None => format!("{kind} `{id}` not in {kind} cache; run `peppy repo refresh`"),
    })?;

    let resolved_path = resolve_cached_artifact_path(
        peppy_dirs,
        entry.source_type,
        entry.source_uri,
        entry.resolved_ref,
        entry.path,
        on_feedback,
    )
    .map_err(|e| format!("{kind} `{id}`: {e}"))?;

    let bytes = std::fs::read(&resolved_path).map_err(|e| {
        format!(
            "failed to read cached {kind} `{id}` at {}: {e}",
            resolved_path.display()
        )
    })?;
    let actual_sha = config::fingerprint::fingerprint_for_bytes(&bytes);
    if actual_sha != entry.sha256 {
        return Err(format!(
            "{kind} `{id}` content drifted from cache fingerprint \
             (expected `{}`, got `{actual_sha}`); run `peppy repo refresh`",
            entry.sha256
        ));
    }

    let content = std::str::from_utf8(&bytes)
        .map_err(|e| format!("cached {kind} `{id}` is not UTF-8: {e}"))?;
    parse(content).map_err(|e| format!("failed to parse cached {kind} `{id}`: {e}"))
}

struct MemoEntry {
    mtime: SystemTime,
    repos_mtime: SystemTime,
    entries: Arc<Vec<NodeCacheEntry>>,
}

fn memo_map() -> &'static Mutex<HashMap<PathBuf, MemoEntry>> {
    static MAP: OnceLock<Mutex<HashMap<PathBuf, MemoEntry>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

fn memo_get(
    path: &Path,
    mtime: SystemTime,
    repos_mtime: SystemTime,
) -> Option<Arc<Vec<NodeCacheEntry>>> {
    let map = memo_map().lock();
    map.get(path)
        .filter(|e| e.mtime == mtime && e.repos_mtime == repos_mtime)
        .map(|e| Arc::clone(&e.entries))
}

fn memo_put(path: &Path, mtime: SystemTime, repos_mtime: SystemTime, entries: Vec<NodeCacheEntry>) {
    memo_map().lock().insert(
        path.to_path_buf(),
        MemoEntry {
            mtime,
            repos_mtime,
            entries: Arc::new(entries),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_entry(name: &str, tag: &str, repo_id: u32) -> NodeCacheEntry {
        NodeCacheEntry {
            node_name: name.to_owned(),
            node_tag: tag.to_owned(),
            source_type: RepoSourceKind::Fs,
            source_uri: None,
            resolved_ref: None,
            sha256: String::new(),
            checksum: None,
            path: "/tmp/foo".to_owned(),
            repo_id,
        }
    }

    fn mk_fs_entry(name: &str, tag: &str, path: &str) -> NodeCacheEntry {
        NodeCacheEntry {
            node_name: name.to_owned(),
            node_tag: tag.to_owned(),
            source_type: RepoSourceKind::Fs,
            source_uri: None,
            resolved_ref: None,
            sha256: String::new(),
            checksum: None,
            path: path.to_owned(),
            repo_id: 0,
        }
    }

    fn mk_git_entry(
        name: &str,
        tag: &str,
        uri: Option<&str>,
        resolved_ref: Option<&str>,
    ) -> NodeCacheEntry {
        NodeCacheEntry {
            node_name: name.to_owned(),
            node_tag: tag.to_owned(),
            source_type: RepoSourceKind::Git,
            source_uri: uri.map(str::to_owned),
            resolved_ref: resolved_ref.map(str::to_owned),
            sha256: String::new(),
            checksum: None,
            path: "nodes/example".to_owned(),
            repo_id: 0,
        }
    }

    fn mk_url_entry(
        name: &str,
        tag: &str,
        uri: Option<&str>,
        checksum: Option<&str>,
    ) -> NodeCacheEntry {
        NodeCacheEntry {
            node_name: name.to_owned(),
            node_tag: tag.to_owned(),
            source_type: RepoSourceKind::Url,
            source_uri: uri.map(str::to_owned),
            resolved_ref: None,
            sha256: String::new(),
            checksum: checksum.map(str::to_owned),
            path: "nodes/example".to_owned(),
            repo_id: 0,
        }
    }

    fn mk_launcher_entry(
        name: &str,
        source_type: RepoSourceKind,
        uri: Option<&str>,
        resolved_ref: Option<&str>,
        path: &str,
        repo_id: u32,
    ) -> LauncherCacheEntry {
        LauncherCacheEntry {
            launcher_name: name.to_owned(),
            source_type,
            source_uri: uri.map(str::to_owned),
            resolved_ref: resolved_ref.map(str::to_owned),
            sha256: String::new(),
            path: path.to_owned(),
            repo_id,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn mk_contract_entry(
        name: &str,
        tag: &str,
        sha256: &str,
        source_type: RepoSourceKind,
        uri: Option<&str>,
        resolved_ref: Option<&str>,
        path: &str,
        repo_id: u32,
    ) -> ContractCacheEntry {
        ContractCacheEntry {
            contract_name: name.to_owned(),
            tag: tag.to_owned(),
            sha256: sha256.to_owned(),
            source_type,
            source_uri: uri.map(str::to_owned),
            resolved_ref: resolved_ref.map(str::to_owned),
            path: path.to_owned(),
            repo_id,
        }
    }

    #[test]
    fn lookup_picks_lowest_repo_id() {
        let entries = vec![
            mk_entry("a", "v1", 5),
            mk_entry("a", "v1", 2),
            mk_entry("a", "v1", 9),
        ];
        let hit = lookup(&entries, "a", "v1").unwrap();
        assert_eq!(hit.repo_id, 2);
    }

    /// Lookup falls back to repo priority alone: the highest-priority entry
    /// (lowest id) wins among entries that share `(name, tag)`.
    #[test]
    fn lookup_returns_highest_priority_when_multiple_match() {
        let entries = vec![mk_entry("a", "v1", 7), mk_entry("a", "v1", 3)];
        let hit = lookup(&entries, "a", "v1").unwrap();
        assert_eq!(hit.repo_id, 3);
    }

    /// `lookup_repo_entry_by_sha256` returns the entry whose content fingerprint
    /// matches exactly, bypassing the repo-priority tiebreak.
    #[test]
    fn lookup_by_sha256_returns_exact_match() {
        let mut older = mk_entry("a", "v1", 1);
        older.sha256 = "aaaa".to_owned();
        let mut newer = mk_entry("a", "v1", 9);
        newer.sha256 = "bbbb".to_owned();
        let entries = vec![older, newer];

        let hit = lookup_repo_entry_by_sha256(&entries, "a", "v1", "bbbb").unwrap();
        assert_eq!(hit.repo_id, 9);
        assert!(lookup_repo_entry_by_sha256(&entries, "a", "v1", "zzzz").is_none());
    }

    #[test]
    fn load_returns_empty_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let (entries, generation) = load_with_generation(&peppy_dirs).unwrap();
        assert!(entries.is_empty());
        assert!(generation.is_none());
    }

    #[test]
    fn write_then_load_roundtrips_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let input = vec![
            NodeCacheEntry {
                node_name: "a".to_owned(),
                node_tag: "v1".to_owned(),
                source_type: RepoSourceKind::Git,
                source_uri: Some("https://example.com/repo.git".to_owned()),
                resolved_ref: Some("main".to_owned()),
                sha256: "aaaa".to_owned(),
                checksum: None,
                path: "nodes/a/peppy.json5".to_owned(),
                repo_id: 0,
            },
            NodeCacheEntry {
                node_name: "b".to_owned(),
                node_tag: "v2".to_owned(),
                source_type: RepoSourceKind::Fs,
                source_uri: None,
                resolved_ref: None,
                sha256: "bbbb".to_owned(),
                checksum: None,
                path: "/tmp/b/peppy.json5".to_owned(),
                repo_id: 0,
            },
        ];
        write_repo_cache(&peppy_dirs, &input).unwrap();
        let (loaded, generation) = load_with_generation(&peppy_dirs).unwrap();
        assert!(generation.is_some());
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].node_name, "a");
        assert_eq!(loaded[0].resolved_ref.as_deref(), Some("main"));
        assert_eq!(loaded[0].sha256, "aaaa");
        assert_eq!(loaded[1].node_name, "b");
        assert_eq!(loaded[1].sha256, "bbbb");
    }

    // -- resolve_repo_node_source tests --

    #[test]
    fn resolve_fs_success() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let entries = vec![mk_fs_entry("mynode", "1.0", "/tmp/mynode/peppy.json5")];
        write_repo_cache(&peppy_dirs, &entries).unwrap();

        let src = resolve_repo_node_source("mynode", "1.0", &peppy_dirs).unwrap();
        // `entry.path` points at the manifest file; the resolved source
        // is the containing directory.
        assert_eq!(src, NodeSource::Fs(PathBuf::from("/tmp/mynode")));
    }

    #[test]
    fn resolve_git_success() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let mut entry = mk_git_entry(
            "g",
            "1.0",
            Some("https://example.com/repo.git"),
            Some("main"),
        );
        entry.path = "nodes/example/peppy.json5".to_owned();
        write_repo_cache(&peppy_dirs, &[entry]).unwrap();

        let src = resolve_repo_node_source("g", "1.0", &peppy_dirs).unwrap();
        match src {
            NodeSource::Git {
                repo_url,
                repo_path,
                repo_ref,
            } => {
                assert!(
                    repo_url
                        .to_bstring()
                        .to_string()
                        .contains("example.com/repo.git"),
                    "unexpected repo_url: {repo_url:?}",
                );
                // `repo_path` is the parent dir of the manifest file
                // inside the cloned repo.
                assert_eq!(repo_path, "nodes/example");
                assert_eq!(repo_ref.as_deref(), Some("main"));
            }
            other => panic!("expected NodeSource::Git, got {other:?}"),
        }
    }

    #[test]
    fn resolve_url_success() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let entries = vec![mk_url_entry(
            "u",
            "2.0",
            Some("https://example.com/archive.tzst"),
            Some("abc123"),
        )];
        write_repo_cache(&peppy_dirs, &entries).unwrap();

        let src = resolve_repo_node_source("u", "2.0", &peppy_dirs).unwrap();
        match src {
            NodeSource::Http { url, sha256 } => {
                assert_eq!(url.as_str(), "https://example.com/archive.tzst");
                assert_eq!(sha256.as_deref(), Some("abc123"));
            }
            other => panic!("expected NodeSource::Http, got {other:?}"),
        }
    }

    #[test]
    fn resolve_git_missing_source_uri() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let entries = vec![mk_git_entry("g", "1.0", None, Some("main"))];
        write_repo_cache(&peppy_dirs, &entries).unwrap();

        let err = resolve_repo_node_source("g", "1.0", &peppy_dirs).unwrap_err();
        assert!(err.contains("no source_uri"), "unexpected error: {err}");
    }

    #[test]
    fn resolve_git_invalid_source_uri() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let entries = vec![mk_git_entry("g", "1.0", Some(""), Some("main"))];
        write_repo_cache(&peppy_dirs, &entries).unwrap();

        let err = resolve_repo_node_source("g", "1.0", &peppy_dirs).unwrap_err();
        assert!(err.contains("invalid git URL"), "unexpected error: {err}",);
    }

    #[test]
    fn resolve_git_missing_resolved_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let entries = vec![mk_git_entry(
            "g",
            "1.0",
            Some("https://example.com/repo.git"),
            None,
        )];
        write_repo_cache(&peppy_dirs, &entries).unwrap();

        let err = resolve_repo_node_source("g", "1.0", &peppy_dirs).unwrap_err();
        assert!(err.contains("no resolved_ref"), "unexpected error: {err}");
    }

    #[test]
    fn resolve_url_missing_source_uri() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let entries = vec![mk_url_entry("u", "1.0", None, None)];
        write_repo_cache(&peppy_dirs, &entries).unwrap();

        let err = resolve_repo_node_source("u", "1.0", &peppy_dirs).unwrap_err();
        assert!(err.contains("no source_uri"), "unexpected error: {err}");
    }

    #[test]
    fn resolve_url_invalid_source_uri() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let entries = vec![mk_url_entry("u", "1.0", Some("not://[invalid"), None)];
        write_repo_cache(&peppy_dirs, &entries).unwrap();

        let err = resolve_repo_node_source("u", "1.0", &peppy_dirs).unwrap_err();
        assert!(err.contains("invalid url"), "unexpected error: {err}");
    }

    #[test]
    fn resolve_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        write_repo_cache::<NodeCacheEntry>(&peppy_dirs, &[]).unwrap();

        let err = resolve_repo_node_source("missing", "0.0", &peppy_dirs).unwrap_err();
        assert!(err.contains("not found"), "unexpected error: {err}");
    }

    // -- launcher cache tests --

    /// `write_repo_cache` writes launcher entries to the path returned by
    /// `launchers_repo_cache_path` with the on-disk launcher schema.
    #[test]
    fn write_launcher_cache_serializes_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let entries = vec![
            mk_launcher_entry(
                "openarm01_sim_teleop",
                RepoSourceKind::Git,
                Some("https://github.com/Peppy-bot/launchers-hub"),
                Some("main"),
                "openarm01_sim_teleop.json5",
                0,
            ),
            mk_launcher_entry(
                "local_demo",
                RepoSourceKind::Fs,
                None,
                None,
                "/tmp/local_demo.json5",
                0,
            ),
        ];
        write_repo_cache(&peppy_dirs, &entries).unwrap();

        let path = launchers_repo_cache_path(&peppy_dirs);
        assert!(
            path.exists(),
            "launcher cache file should exist at {}",
            path.display()
        );

        let raw = std::fs::read_to_string(&path).expect("read launcher cache");
        let parsed: serde_json::Value =
            serde_json5::from_str(&raw).expect("launcher cache should be valid JSON5");
        let arr = parsed.as_array().expect("expected array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["launcher_name"], "openarm01_sim_teleop");
        assert_eq!(
            arr[0]["source_uri"],
            "https://github.com/Peppy-bot/launchers-hub"
        );
        assert_eq!(arr[0]["resolved_ref"], "main");
        assert_eq!(arr[1]["launcher_name"], "local_demo");
        assert_eq!(arr[1]["source_type"], "fs");
    }

    // -- resolve_repo_launcher_path tests --

    /// Sanity: the helper turns the launcher name into the absolute path
    /// recorded for an Fs cache entry.
    #[test]
    fn resolve_launcher_fs_returns_recorded_path() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let abs = tmp.path().join("demo.json5");
        std::fs::write(&abs, "{}").unwrap();

        write_repo_cache(
            &peppy_dirs,
            &[mk_launcher_entry(
                "demo",
                RepoSourceKind::Fs,
                None,
                None,
                abs.to_string_lossy().as_ref(),
                0,
            )],
        )
        .unwrap();

        let path = resolve_repo_launcher_path("demo", &peppy_dirs, &|_| {})
            .expect("resolve should succeed");
        assert_eq!(path, abs);
    }

    /// A miss surfaces the launcher name and the cache path so users can
    /// jump straight to `peppy repo refresh` (or notice the typo).
    #[test]
    fn resolve_launcher_missing_name_includes_cache_path_in_error() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        write_repo_cache::<LauncherCacheEntry>(&peppy_dirs, &[]).unwrap();

        let err = resolve_repo_launcher_path("nope", &peppy_dirs, &|_| {})
            .expect_err("missing launcher should error");
        assert!(err.contains("launcher `nope` not found"), "got: {err}");
        assert!(err.contains("launchers.json5"), "got: {err}");
    }

    /// `lookup_launcher` resolves name collisions by repo priority: the
    /// entry from the lowest-id repository wins among launchers that
    /// share a name. We test this at the `lookup` boundary directly
    /// because `repo_id` is derived from `repositories.json5` at read
    /// time (`#[serde(skip)]` on the struct), so round-tripping through
    /// `write_repo_cache` would erase it.
    #[test]
    fn lookup_launcher_picks_lowest_repo_id() {
        let entries = vec![
            mk_launcher_entry(
                "demo",
                RepoSourceKind::Fs,
                None,
                None,
                "/path/to/demo_low_priority.json5",
                3,
            ),
            mk_launcher_entry(
                "demo",
                RepoSourceKind::Fs,
                None,
                None,
                "/path/to/demo_high_priority.json5",
                1,
            ),
        ];
        let hit = lookup_launcher(&entries, "demo").expect("should resolve");
        assert_eq!(hit.repo_id, 1);
        assert!(hit.path.ends_with("demo_high_priority.json5"));
    }

    /// `Url` repository launchers are never populated by `repo refresh`
    /// today, but if a hand-edited cache contains one we surface a clear
    /// "not yet supported" error rather than an opaque file-not-found.
    #[test]
    fn resolve_launcher_url_returns_not_supported_error() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        write_repo_cache(
            &peppy_dirs,
            &[mk_launcher_entry(
                "demo",
                RepoSourceKind::Url,
                Some("https://example.com/archive.tzst"),
                None,
                "demo.json5",
                0,
            )],
        )
        .unwrap();

        let err = resolve_repo_launcher_path("demo", &peppy_dirs, &|_| {})
            .expect_err("url launcher should error");
        assert!(err.contains("not yet supported"), "got: {err}");
    }

    // -- resolve_cached_artifact_path tests --

    /// Initializes a bare-bones git repository at `repo_dir` and writes a
    /// single committed file at the given repo-relative path. Returns the
    /// branch name resolved from HEAD (e.g. "main" / "master").
    fn init_repo_with_file(repo_dir: &Path, file_path: &str, contents: &str) -> String {
        let repo = git2::Repository::init(repo_dir).expect("git init");
        let abs = repo_dir.join(file_path);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(&abs, contents).expect("write committed file");

        let rel = Path::new(file_path);
        let mut index = repo.index().expect("open index");
        index.add_path(rel).expect("add file");
        index.write().expect("write index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        let sig = git2::Signature::now("Peppy", "peppy@example.com").expect("signature");
        repo.commit(Some("HEAD"), &sig, &sig, "seed", &tree, &[])
            .expect("commit");

        repo.head()
            .expect("head")
            .shorthand()
            .expect("shorthand")
            .to_owned()
    }

    /// `Fs` entries are already absolute on disk; the helper returns the
    /// recorded path verbatim without touching the git checkout cache.
    #[test]
    fn resolve_cached_artifact_path_fs_returns_path_verbatim() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let abs = tmp.path().join("artifact.json5");

        let resolved = resolve_cached_artifact_path(
            &peppy_dirs,
            RepoSourceKind::Fs,
            None,
            None,
            abs.to_string_lossy().as_ref(),
            &|_| {},
        )
        .expect("fs resolve should succeed");
        assert_eq!(resolved, abs);
    }

    /// `Url` source kind isn't wired through repo refresh yet; the helper
    /// surfaces a generic (artifact-agnostic) "not yet supported" error so
    /// callers can layer their own context prefix.
    #[test]
    fn resolve_cached_artifact_path_url_returns_not_yet_supported() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let err = resolve_cached_artifact_path(
            &peppy_dirs,
            RepoSourceKind::Url,
            Some("https://example.com/archive.tzst"),
            None,
            "artifact.json5",
            &|_| {},
        )
        .expect_err("url should error");
        assert!(err.contains("not yet supported"), "got: {err}");
        assert!(
            !err.contains("launcher") && !err.contains("contract"),
            "helper error should be artifact-agnostic, got: {err}"
        );
    }

    /// Git entries are useless without a clone URL; the helper rejects
    /// them up-front rather than crashing inside `ensure_checkout`.
    #[test]
    fn resolve_cached_artifact_path_git_requires_source_uri() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let err = resolve_cached_artifact_path(
            &peppy_dirs,
            RepoSourceKind::Git,
            None,
            Some("main"),
            "artifact.json5",
            &|_| {},
        )
        .expect_err("missing source_uri should error");
        assert!(err.contains("source_uri"), "got: {err}");
    }

    /// Git entries are also useless without a resolved ref to check out.
    #[test]
    fn resolve_cached_artifact_path_git_requires_resolved_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let err = resolve_cached_artifact_path(
            &peppy_dirs,
            RepoSourceKind::Git,
            Some("https://example.com/repo.git"),
            None,
            "artifact.json5",
            &|_| {},
        )
        .expect_err("missing resolved_ref should error");
        assert!(err.contains("resolved_ref"), "got: {err}");
    }

    /// Regression for the launcher side of the bug class: a git-sourced
    /// `LauncherCacheEntry` records a repo-relative `path`, so resolution
    /// must materialize the checkout and join the relative path on top,
    /// not just read `entry.path` from the CWD. This test covered nothing
    /// before; the missing coverage is what let the symmetric contract
    /// bug land.
    #[test]
    fn resolve_launcher_git_materializes_checkout() {
        let peppy_tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(peppy_tmp.path());

        let source_parent = tempfile::tempdir().unwrap();
        let source_repo_dir = source_parent.path().join("launchers_hub");
        std::fs::create_dir_all(&source_repo_dir).expect("create source repo dir");
        let branch = init_repo_with_file(&source_repo_dir, "launchers/demo.json5", "{}");
        let repo_url = source_repo_dir.display().to_string();

        write_repo_cache(
            &peppy_dirs,
            &[mk_launcher_entry(
                "demo",
                RepoSourceKind::Git,
                Some(&repo_url),
                Some(&branch),
                "launchers/demo.json5",
                0,
            )],
        )
        .unwrap();

        let resolved = resolve_repo_launcher_path("demo", &peppy_dirs, &|_| {})
            .expect("git launcher resolve should succeed");
        assert!(
            resolved.is_absolute(),
            "resolved path should be absolute, got {}",
            resolved.display()
        );
        assert!(resolved.ends_with("launchers/demo.json5"));
        assert!(
            resolved.exists(),
            "resolved path should exist on disk after ensure_checkout"
        );
    }

    /// The write is atomic: the final file is created via a tmp + rename
    /// dance so concurrent readers can't observe a half-written cache.
    /// We can't reliably observe the rename, but we can at least confirm
    /// no `.tmp` file is left behind on the happy path.
    #[test]
    fn write_launcher_cache_does_not_leak_tmp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        write_repo_cache(
            &peppy_dirs,
            &[mk_launcher_entry(
                "demo",
                RepoSourceKind::Fs,
                None,
                None,
                "/tmp/demo.json5",
                0,
            )],
        )
        .unwrap();

        let tmp_path = peppy_dirs.cache_dir().join("launchers.json5.tmp");
        assert!(
            !tmp_path.exists(),
            "tmp file should be renamed away, not left behind"
        );
    }

    // -- contract cache tests --

    /// `write_repo_cache` round-trips a contract entry through JSON5 with
    /// the documented field names: `contract_name`, `tag`, `sha256`.
    #[test]
    fn write_contract_cache_serializes_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let entries = vec![mk_contract_entry(
            "uvc_camera",
            "v1",
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
            RepoSourceKind::Git,
            Some("https://github.com/Peppy-bot/interfaces_hub"),
            Some("main"),
            "uvc_camera/peppy.json5",
            0,
        )];
        write_repo_cache(&peppy_dirs, &entries).unwrap();

        let path = contracts_repo_cache_path(&peppy_dirs);
        assert!(path.exists(), "contracts cache should exist");

        let raw = std::fs::read_to_string(&path).expect("read contracts cache");
        let parsed: serde_json::Value =
            serde_json5::from_str(&raw).expect("contracts cache should be valid JSON5");
        let arr = parsed.as_array().expect("expected array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["contract_name"], "uvc_camera");
        assert_eq!(arr[0]["tag"], "v1");
        assert_eq!(
            arr[0]["sha256"],
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        assert_eq!(arr[0]["source_type"], "git");
        assert_eq!(arr[0]["path"], "uvc_camera/peppy.json5");
    }

    /// Lookup picks the lowest-`repo_id` entry, even when two repos
    /// declare contracts with the same `(name, tag)` and different
    /// `sha256` fingerprints.
    #[test]
    fn lookup_contract_picks_highest_priority_repo() {
        let entries = vec![
            mk_contract_entry(
                "uvc_camera",
                "v1",
                "bbbb",
                RepoSourceKind::Git,
                Some("https://example.com/b"),
                Some("main"),
                "uvc_camera/peppy.json5",
                5,
            ),
            mk_contract_entry(
                "uvc_camera",
                "v1",
                "aaaa",
                RepoSourceKind::Git,
                Some("https://example.com/a"),
                Some("main"),
                "uvc_camera/peppy.json5",
                1,
            ),
        ];
        let hit = lookup_contract(&entries, "uvc_camera", "v1").unwrap();
        assert_eq!(hit.repo_id, 1);
        assert_eq!(hit.sha256, "aaaa");
    }

    /// `lookup_contract_by_sha256` returns the exact content match
    /// regardless of repo priority.
    #[test]
    fn lookup_contract_by_sha256_returns_exact_match() {
        let entries = vec![
            mk_contract_entry(
                "uvc_camera",
                "v1",
                "aaaa",
                RepoSourceKind::Fs,
                None,
                None,
                "/a/peppy.json5",
                1,
            ),
            mk_contract_entry(
                "uvc_camera",
                "v1",
                "bbbb",
                RepoSourceKind::Fs,
                None,
                None,
                "/b/peppy.json5",
                9,
            ),
        ];
        let hit = lookup_contract_by_sha256(&entries, "uvc_camera", "v1", "bbbb").unwrap();
        assert_eq!(hit.repo_id, 9);
        assert!(lookup_contract_by_sha256(&entries, "uvc_camera", "v1", "zzzz").is_none());
    }
}
