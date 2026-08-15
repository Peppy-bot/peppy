//! Typed loaders for the four repo caches written by `repo_refresh`:
//! `~/.peppy/cache/nodes.json5`, `launchers.json5`, `contracts.json5`,
//! and `pairings.json5`. Each file lists every item of its kind that the
//! configured repositories publish. This module gives the rest of the
//! daemon a typed view over those entries so callers don't have to dig
//! through `serde_json::Value` every time.
//!
//! The four caches share one pipeline: every cache concern (atomic
//! write, load-time `repo_id` tagging, repo-priority lookup, refresh-side
//! collection) is implemented once, generic over [`RepoCacheEntry`]. The
//! entry structs stay distinct types because each cache file has its own
//! on-disk field names (`node_name`, `launcher_name`, …).
//!
//! Every entry carries an [`EntryOrigin`], which says both where the
//! bytes live and which revision they were read at, and a fingerprint of
//! the manifest file bytes. Two entries that share `(name, tag)` across
//! repositories are kept side by side (the fingerprint tells them apart);
//! lookup picks the entry from the lowest-id repository.
//!
//! Reads of the nodes cache are memoized by `(mtime-of-cache-file)` per
//! path so that a daemon hit by many `node add` / launch goals in a row
//! doesn't re-read and re-parse the cache file on every request.

use crate::Result;
use crate::services::repo::RepoOwners;
use crate::services::repo::refresh::read_or_create_repos;
use core_node_api::encoding::RepoItemKind;
use daemon_config::consts::PeppyDirs;
use daemon_config::repository::{ItemName, ItemTag, ManifestFingerprint, PinnedItem};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

// One vocabulary for "where bytes live and which revision they were read
// at", shared with the launch pins (`daemon_config::repository::pins`) so
// what a cache records and what a coordinator ships cannot drift.
pub use daemon_config::repository::EntryOrigin;

/// One entry as it appears in `nodes.json5`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NodeCacheEntry {
    pub node_name: ItemName,
    pub node_tag: ItemTag,
    /// Fingerprint of the manifest file bytes. Two entries that share
    /// `(name, tag)` across repositories are told apart by this.
    pub sha256: ManifestFingerprint,
    pub origin: EntryOrigin,
    /// The id of the repository entry this node was discovered under (as
    /// read from `repositories.json5`). Derived at read time and never
    /// serialized back to disk.
    #[serde(skip)]
    pub repo_id: u32,
}

/// One entry as it appears in `launchers.json5`. Launchers carry no tag:
/// they are identified by `peppy_schema: "launcher/v1"` and keyed by file
/// stem.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LauncherCacheEntry {
    /// Name of the launcher (file stem of the `.json5` file).
    pub launcher_name: ItemName,
    pub sha256: ManifestFingerprint,
    pub origin: EntryOrigin,
    #[serde(skip)]
    pub repo_id: u32,
}

/// One entry as it appears in `contracts.json5`. Contracts are stand-alone
/// JSON5 documents (`peppy_schema: "contract/v1"`) describing a reusable
/// set of topics, services and actions.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContractCacheEntry {
    pub contract_name: ItemName,
    pub tag: ItemTag,
    pub sha256: ManifestFingerprint,
    pub origin: EntryOrigin,
    #[serde(skip)]
    pub repo_id: u32,
}

/// One entry as it appears in `pairings.json5`. Pairings are stand-alone
/// JSON5 documents (`peppy_schema: "pairing/v1"`) describing a two-role,
/// topics-only conversation contract.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PairingCacheEntry {
    pub pairing_name: ItemName,
    pub tag: ItemTag,
    pub sha256: ManifestFingerprint,
    pub origin: EntryOrigin,
    #[serde(skip)]
    pub repo_id: u32,
}

/// One entry as it appears in `mcp_exposures.json5`. MCP exposures are
/// stand-alone JSON5 documents (`peppy_schema: "mcp_exposure/v1"`)
/// selecting the contract members a generated MCP server makes public.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct McpExposureCacheEntry {
    pub exposure_name: ItemName,
    pub tag: ItemTag,
    pub sha256: ManifestFingerprint,
    pub origin: EntryOrigin,
    #[serde(skip)]
    pub repo_id: u32,
}

/// One repository's items, split by kind. Whatever read the repository
/// hands back this shape, so the merge, retention and cache-writing below
/// it are written once.
#[derive(Debug, Default)]
pub(crate) struct RepoItems {
    pub nodes: Vec<NodeCacheEntry>,
    pub launchers: Vec<LauncherCacheEntry>,
    pub contracts: Vec<ContractCacheEntry>,
    pub pairings: Vec<PairingCacheEntry>,
    pub mcp_exposures: Vec<McpExposureCacheEntry>,
}

/// Uniform view over the five cache-entry kinds, so the cache plumbing
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

    fn name(&self) -> &str;
    /// Empty for untagged kinds (launchers), so `(name, tag)` is the
    /// identity key for every kind. Those kinds carry no tag field at all,
    /// so the default is what they use.
    fn tag(&self) -> &str {
        ""
    }
    fn sha256(&self) -> &ManifestFingerprint;
    fn origin(&self) -> &EntryOrigin;
    fn repo_id(&self) -> u32;
    fn set_repo_id(&mut self, id: u32);

    /// `name:tag` label for log messages (bare name for untagged kinds).
    fn display_id(&self) -> String {
        if self.tag().is_empty() {
            self.name().to_owned()
        } else {
            format!("{}:{}", self.name(), self.tag())
        }
    }
}

/// Writes the [`RepoCacheEntry`] impl for one kind. The five differ only in
/// their constants and in which field holds the name and the tag, so
/// spelling the accessors out five times would be five chances to wire one
/// to the wrong field.
/// The trailing tag field is omitted for launchers, the one untagged kind.
macro_rules! repo_cache_entry {
    ($ty:ident, $kind:literal, $file:literal, $item_kind:ident, $name_field:ident $(, $tag_field:ident)?) => {
        impl RepoCacheEntry for $ty {
            const KIND: &'static str = $kind;
            const FILE_NAME: &'static str = $file;
            const ITEM_KIND: RepoItemKind = RepoItemKind::$item_kind;

            fn name(&self) -> &str {
                self.$name_field.as_str()
            }
            $( fn tag(&self) -> &str {
                self.$tag_field.as_str()
            } )?
            fn sha256(&self) -> &ManifestFingerprint {
                &self.sha256
            }
            fn origin(&self) -> &EntryOrigin {
                &self.origin
            }
            fn repo_id(&self) -> u32 {
                self.repo_id
            }
            fn set_repo_id(&mut self, id: u32) {
                self.repo_id = id;
            }
        }
    };
}

repo_cache_entry!(
    NodeCacheEntry,
    "node",
    "nodes.json5",
    Node,
    node_name,
    node_tag
);
repo_cache_entry!(
    LauncherCacheEntry,
    "launcher",
    "launchers.json5",
    Launcher,
    launcher_name
);
repo_cache_entry!(
    ContractCacheEntry,
    "contract",
    "contracts.json5",
    Contract,
    contract_name,
    tag
);
repo_cache_entry!(
    PairingCacheEntry,
    "pairing",
    "pairings.json5",
    Pairing,
    pairing_name,
    tag
);
repo_cache_entry!(
    McpExposureCacheEntry,
    "mcp exposure",
    "mcp_exposures.json5",
    McpExposure,
    exposure_name,
    tag
);

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

/// Reads `E`'s cache file and tags each entry with the `repo_id` of its
/// originating repository entry (derived from `repositories.json5` at
/// read time; never serialized back). Entries no repository claims fall
/// back to [`UNOWNED_REPO_ID`], so a hand-written cache still resolves
/// without outranking a configured repository. Returns an empty vec when
/// the file is missing.
///
/// A file that does not parse is refused whole rather than read for the
/// entries that happen to be readable. Dropping the rest would leave a
/// launch resolving part of a graph and reporting nothing about the part
/// it could not see, which is indistinguishable from a repository that
/// never published it.
pub(crate) fn load_repo_cache<E: RepoCacheEntry>(peppy_dirs: &PeppyDirs) -> Result<Vec<E>> {
    let path = repo_cache_path::<E>(peppy_dirs);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(&path)?;
    let raw: Vec<E> = serde_json5::from_str(&content).map_err(|e| {
        core_node_api::Error::Decoding(format!(
            "the {} cache at {} does not parse: {e}. Run `peppy repo update` to rewrite it",
            E::KIND,
            path.display()
        ))
    })?;

    let owners = RepoOwners::new(&read_or_create_repos(peppy_dirs)?);
    let entries = raw
        .into_iter()
        .map(|mut e| {
            let id = lookup_repo_id(&owners, e.origin());
            e.set_repo_id(id);
            e
        })
        .collect();
    Ok(entries)
}

/// One identity answered more than once by a single repository. Unlike
/// two repositories claiming an identity, which the `repo_id` order
/// settles deterministically, this has no defensible winner: picking one
/// would be an arbitrary choice presented as an answer.
///
/// `repo refresh` refuses a repository that contains such a conflict, so
/// reaching this at lookup means the cache predates that check or was
/// hand-edited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoAmbiguity {
    /// Singular kind label ("node", "launcher", ...).
    pub kind: &'static str,
    /// `name:tag` label, or the bare name for untagged kinds.
    pub id: String,
    pub repo_id: u32,
    /// Every claimant's path, sorted.
    pub paths: Vec<String>,
}

impl std::fmt::Display for RepoAmbiguity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} entries in repository {} claim `{}`: {}; \
             fix the repository so one manifest claims it, then run \
             `peppy repo refresh`",
            self.paths.len(),
            self.kind,
            self.repo_id,
            self.id,
            self.paths.join(", ")
        )
    }
}

/// Returns the highest-priority (lowest `repo_id`) entry matching
/// `(name, tag)`, `None` when nothing matches, or an error when the
/// winning repository claims the identity more than once.
///
/// The repo-priority tiebreak lives here so all four caches resolve
/// cross-repo name collisions identically. Untagged kinds (launchers)
/// match with `tag = ""`.
///
/// Only entries tied at the winning `repo_id` are ambiguous. A
/// lower-id repository shadowing a higher-id one stays a supported
/// feature with a documented order, and still resolves.
pub(crate) fn lookup_repo_entry<'a, E: RepoCacheEntry>(
    entries: &'a [E],
    name: &str,
    tag: &str,
) -> std::result::Result<Option<&'a E>, RepoAmbiguity> {
    let matches: Vec<&E> = entries
        .iter()
        .filter(|e| e.name() == name && e.tag() == tag)
        .collect();
    let Some(winning_id) = matches.iter().map(|e| e.repo_id()).min() else {
        return Ok(None);
    };

    let mut winners = matches.into_iter().filter(|e| e.repo_id() == winning_id);
    let first = winners.next().expect("min came from a non-empty set");
    let contested: Vec<&E> = winners.collect();
    if contested.is_empty() {
        return Ok(Some(first));
    }

    let mut paths: Vec<String> = std::iter::once(first)
        .chain(contested)
        .map(|e| e.origin().path_str().to_owned())
        .collect();
    paths.sort();
    Err(RepoAmbiguity {
        kind: E::KIND,
        id: first.display_id(),
        repo_id: winning_id,
        paths,
    })
}

/// Returns the entry whose `(name, tag, sha256)` triple matches
/// exactly, bypassing the repo-priority tiebreak. Use when the caller
/// wants a specific manifest content rather than the
/// first-in-priority-order pick.
pub(crate) fn lookup_repo_entry_by_sha256<'a, E: RepoCacheEntry>(
    entries: &'a [E],
    name: &str,
    tag: &str,
    sha256: &ManifestFingerprint,
) -> Option<&'a E> {
    entries
        .iter()
        .find(|e| e.name() == name && e.tag() == tag && e.sha256() == sha256)
}

/// Reads `nodes.json5`, memoized on the mtimes of the cache file and of
/// `repositories.json5`.
///
/// Both mtimes matter: the entries come from the first and the `repo_id`
/// tagging on each of them from the second, so a memo keyed on the cache
/// alone would serve stale priorities after a `repo add`.
pub fn load_node_cache(peppy_dirs: &PeppyDirs) -> Result<Vec<NodeCacheEntry>> {
    let path = repo_cache_path::<NodeCacheEntry>(peppy_dirs);
    let generation = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok());
    // Missing file maps to UNIX_EPOCH so any future appearance counts as a
    // change.
    let repos_mtime = repositories_mtime(peppy_dirs);

    if let Some(mtime) = generation
        && let Some(cached) = memo_get(&path, mtime, repos_mtime)
    {
        return Ok((*cached).clone());
    }

    let entries = load_repo_cache::<NodeCacheEntry>(peppy_dirs)?;

    if let Some(mtime) = generation {
        memo_put(&path, mtime, repos_mtime, entries.clone());
    }
    Ok(entries)
}

fn repositories_mtime(peppy_dirs: &PeppyDirs) -> SystemTime {
    let path = repositories_list_path(peppy_dirs);
    std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Priority given to a cached entry that no configured repository
/// claims: the lowest there is. Such an entry still resolves when
/// nothing else claims its identity, which is what keeps a hand-written
/// cache working, but it never outranks a configured repository.
///
/// Configuration and the caches are published as separate files, so an
/// entry outlives its repository for as long as the re-index that
/// follows a `repo remove` or `repo exclude` takes to land, and longer
/// still when that re-index fails. Treating those entries as the highest
/// priority would let the repository a user just removed shadow the ones
/// they kept.
pub(crate) const UNOWNED_REPO_ID: u32 = u32::MAX;

/// Owning repository id narrowed to the wire's `u32`, falling back to
/// [`UNOWNED_REPO_ID`] when no repository matches.
fn lookup_repo_id(owners: &RepoOwners, origin: &EntryOrigin) -> u32 {
    owners
        .owner_of(origin)
        .and_then(|id| u32::try_from(id).ok())
        .unwrap_or(UNOWNED_REPO_ID)
}

/// Returns the highest-priority (lowest `repo_id`) node entry for
/// `(name, tag)`, `None` when no entry matches, or [`RepoAmbiguity`]
/// when the winning repository claims it more than once.
pub fn lookup<'a>(
    entries: &'a [NodeCacheEntry],
    name: &str,
    tag: &str,
) -> std::result::Result<Option<&'a NodeCacheEntry>, RepoAmbiguity> {
    lookup_repo_entry(entries, name, tag)
}

/// Returns the highest-priority (lowest `repo_id`) launcher entry
/// matching `name`, `None` when no entry matches, or [`RepoAmbiguity`]
/// when the winning repository claims it more than once.
pub fn lookup_launcher<'a>(
    entries: &'a [LauncherCacheEntry],
    name: &str,
) -> std::result::Result<Option<&'a LauncherCacheEntry>, RepoAmbiguity> {
    lookup_repo_entry(entries, name, "")
}

/// Suffix appended to a cache-miss message when the machine excludes
/// repositories, so an identity that is absent because its repository was
/// excluded is attributable rather than looking like missing content.
///
/// An excluded repository is never scanned, so peppy cannot know whether
/// it would have provided the identity; the wording says so instead of
/// implying it did.
pub(crate) fn excluded_repositories_hint(peppy_dirs: &PeppyDirs) -> String {
    let excluded = crate::services::repo::exclude::ExclusionSet::load(peppy_dirs);
    if excluded.entries.is_empty() {
        return String::new();
    }
    let mut identities: Vec<&str> = excluded
        .entries
        .iter()
        .map(|e| e.identity.as_str())
        .collect();
    identities.sort();
    let plural = if identities.len() == 1 { "y" } else { "ies" };
    format!(
        ". {} excluded repositor{plural} ({}) {} not indexed at all and may have provided it",
        identities.len(),
        identities.join(", "),
        if identities.len() == 1 { "is" } else { "are" },
    )
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

pub fn mcp_exposures_repo_cache_path(peppy_dirs: &PeppyDirs) -> PathBuf {
    repo_cache_path::<McpExposureCacheEntry>(peppy_dirs)
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

/// Looks up `name` in the launcher cache and resolves it to a concrete
/// on-disk path that the launch flow can open and parse.
///
/// For Git entries this materializes the repo's checkout via
/// [`crate::services::node::cache::git::ensure_checkout`] (blocking; wrap
/// callers in `spawn_blocking` when running inside Tokio). `on_feedback`
/// receives clone/refresh progress lines. Touches no stack state, so
/// read-only callers (`stack resolve`) share it with the launch flow.
pub fn resolve_repo_launcher_path(
    name: &str,
    peppy_dirs: &PeppyDirs,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<PathBuf, String> {
    let entries: Vec<LauncherCacheEntry> =
        load_repo_cache(peppy_dirs).map_err(|e| format!("failed to load launcher cache: {e}"))?;

    let entry = lookup_launcher(&entries, name)
        .map_err(|ambiguity| ambiguity.to_string())?
        .ok_or_else(|| {
            format!(
                "launcher `{name}` not found in {}{}",
                launchers_repo_cache_path(peppy_dirs).display(),
                excluded_repositories_hint(peppy_dirs)
            )
        })?;

    resolve_cached_artifact_path(peppy_dirs, &entry.origin, on_feedback)
        .map_err(|e| format!("launcher `{name}`: {e}"))
}

/// Materializes an [`EntryOrigin`] into a concrete on-disk path.
///
/// For git origins this materializes the checkout of the pinned commit via
/// [`crate::services::node::cache::git::ensure_checkout_at_commit`]
/// (blocking; wrap callers in `spawn_blocking` when running inside Tokio)
/// and joins the repository-relative path onto it. `on_feedback` receives
/// clone and fetch progress lines.
///
/// Errors are artifact-agnostic (no "launcher" / "contract" wording); the
/// caller is expected to `map_err` with its own context prefix.
pub(crate) fn resolve_cached_artifact_path(
    peppy_dirs: &PeppyDirs,
    origin: &EntryOrigin,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<PathBuf, String> {
    match origin {
        EntryOrigin::Fs { path } => Ok(path.clone()),
        EntryOrigin::Git {
            repo_url,
            repo_ref,
            commit,
            path,
        } => {
            let checkout = crate::services::node::cache::git::ensure_checkout_at_commit(
                peppy_dirs,
                repo_url,
                repo_ref.as_deref(),
                commit,
                on_feedback,
            )?;
            Ok(checkout.join(path.as_path()))
        }
    }
}

/// Resolves one `name:tag[@sha256]` reference in a manifest into a parsed
/// document: validates the pin, picks the entry it names (or the
/// highest-priority entry for `(name, tag)` when there is none), resolves
/// that entry's on-disk path, reads the bytes, rejects fingerprint drift,
/// and hands the UTF-8 content to `parse`.
///
/// The whole path from what a manifest states to a parsed document lives
/// here, generic over the cache kind, so contracts and pairings cannot
/// drift in how a pin is validated, which entry wins, or how a miss is
/// worded. Errors are labelled with [`RepoCacheEntry::KIND`], so a fifth
/// kind cannot inherit a fourth's wording by copy-paste.
pub(crate) fn resolve_cached_doc<E: RepoCacheEntry, T>(
    peppy_dirs: &PeppyDirs,
    entries: &[E],
    name: &str,
    tag: &str,
    sha256_pin: Option<&str>,
    parse: impl FnOnce(&str) -> std::result::Result<T, String>,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<T, String> {
    let kind = E::KIND;
    let id = format!("{name}:{tag}");
    let (_entry, bytes) =
        resolve_cached_doc_entry(peppy_dirs, entries, name, tag, sha256_pin, on_feedback)?;
    let content = std::str::from_utf8(&bytes)
        .map_err(|e| format!("cached {kind} `{id}` is not UTF-8: {e}"))?;
    parse(content).map_err(|e| format!("failed to parse cached {kind} `{id}`: {e}"))
}

/// The entry-returning core of [`resolve_cached_doc`]: everything up to the
/// parse, handing back the winning entry beside the verified bytes.
///
/// Split out so a launch coordinator can mint a pin from the entry (its
/// fingerprint and origin) through exactly the rules a plain resolution
/// uses, rather than restating the pin validation, the priority tiebreak
/// and the drift check a second way.
pub(crate) fn resolve_cached_doc_entry<'a, E: RepoCacheEntry>(
    peppy_dirs: &PeppyDirs,
    entries: &'a [E],
    name: &str,
    tag: &str,
    sha256_pin: Option<&str>,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<(&'a E, Vec<u8>), String> {
    let kind = E::KIND;
    let id = format!("{name}:{tag}");

    // A manifest is hand-written, so the pin is a claim until it is parsed.
    // A malformed one is refused by name here rather than silently matching
    // nothing and surfacing as "not in cache", which would send the reader
    // to `peppy repo refresh` for a typo.
    let pinned = sha256_pin
        .map(|sha| {
            ManifestFingerprint::parse(sha)
                .map_err(|e| format!("{kind} `{id}` is pinned to an unusable sha256 `{sha}`: {e}"))
        })
        .transpose()?;

    // A sha pin names one exact manifest, so it is never ambiguous.
    let found = match &pinned {
        Some(sha) => lookup_repo_entry_by_sha256(entries, name, tag, sha),
        None => lookup_repo_entry(entries, name, tag).map_err(|ambiguity| ambiguity.to_string())?,
    };

    let entry = found.ok_or_else(|| {
        let hint = excluded_repositories_hint(peppy_dirs);
        match sha256_pin {
            Some(sha) => format!(
                "{kind} `{id}` (sha256 `{sha}`) not in {kind} cache; \
                 run `peppy repo refresh`{hint}"
            ),
            None => format!("{kind} `{id}` not in {kind} cache; run `peppy repo refresh`{hint}"),
        }
    })?;

    let resolved_path = resolve_cached_artifact_path(peppy_dirs, entry.origin(), on_feedback)
        .map_err(|e| format!("{kind} `{id}`: {e}"))?;

    let bytes = std::fs::read(&resolved_path).map_err(|e| {
        format!(
            "failed to read cached {kind} `{id}` at {}: {e}",
            resolved_path.display()
        )
    })?;
    let actual_sha = ManifestFingerprint::of_bytes(&bytes);
    if &actual_sha != entry.sha256() {
        return Err(format!(
            "{kind} `{id}` content drifted from cache fingerprint \
             (expected `{}`, got `{actual_sha}`); run `peppy repo refresh`",
            entry.sha256()
        ));
    }
    Ok((entry, bytes))
}

/// Materializes a launch pin's bytes from its own origin: the fetch half of
/// pin resolution, taken when no local content matches.
///
/// For a git origin this checks out the pinned commit (blocking; wrap in
/// `spawn_blocking` inside Tokio), re-checks that the pinned path stays
/// inside the checkout after symlink resolution, reads the bytes, and
/// compares their fingerprint against the pin. A mismatch is a refusal
/// naming both fingerprints: it means the remote moved under the pin or the
/// path resolved to something else, and neither may be papered over. There
/// is no fallback to resolving the pin's name.
///
/// The path re-check mirrors `resolve_declared_item` in `repo/index.rs`:
/// [`daemon_config::repository::RepoRelativePath`] already refused traversal
/// at decode, so what is left is a symlink inside the fetched tree pointing
/// out of it.
pub(crate) fn resolve_pinned_bytes(
    peppy_dirs: &PeppyDirs,
    pin: &PinnedItem,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<(PathBuf, Vec<u8>), String> {
    let label = pin.label();
    let resolved_path = match &pin.origin {
        EntryOrigin::Fs { path } => path.clone(),
        EntryOrigin::Git {
            repo_url,
            repo_ref,
            commit,
            path,
        } => {
            let checkout = crate::services::node::cache::git::ensure_checkout_at_commit(
                peppy_dirs,
                repo_url,
                repo_ref.as_deref(),
                commit,
                on_feedback,
            )
            .map_err(|e| format!("{label}: {e}"))?;
            let candidate = checkout.join(path.as_path());
            let canonical_root = checkout.canonicalize().map_err(|e| {
                format!(
                    "{label}: checkout {} is unreadable: {e}",
                    checkout.display()
                )
            })?;
            let canonical = candidate.canonicalize().map_err(|e| {
                format!(
                    "{label}: pinned path {} is absent from commit {commit}: {e}",
                    path.as_str()
                )
            })?;
            if !canonical.starts_with(&canonical_root) {
                return Err(format!(
                    "{label}: pinned path {} escapes the fetched tree (resolves to {})",
                    path.as_str(),
                    canonical.display()
                ));
            }
            canonical
        }
    };

    let bytes = std::fs::read(&resolved_path)
        .map_err(|e| format!("{label}: failed to read {}: {e}", resolved_path.display()))?;
    let actual = ManifestFingerprint::of_bytes(&bytes);
    if actual != pin.sha256 {
        return Err(format!(
            "{label}: content at {} does not match its pin (pinned `{}`, got `{actual}`); \
             the remote moved under the pin or the location resolved to something else",
            resolved_path.display(),
            pin.sha256
        ));
    }
    Ok((resolved_path, bytes))
}

/// The entry whose content matches `pin`, whatever identity or repository it
/// arrived under.
///
/// Content is what authorises reuse, not provenance: a machine holding the
/// pinned bytes from another repository uses its own copy rather than
/// fetching. Matching by fingerprint alone (not name-first) also keeps two
/// same-named entries with different content from shadowing the right one.
pub(crate) fn lookup_by_content<'a, E: RepoCacheEntry>(
    entries: &'a [E],
    pin: &PinnedItem,
) -> Option<&'a E> {
    entries.iter().find(|e| e.sha256() == &pin.sha256)
}

/// Resolves a launch pin to its bytes: the local content match first, the
/// pin's own origin second, a name lookup never.
///
/// The content-match half re-reads and re-fingerprints the local copy, so a
/// cache whose file drifted since refresh falls through to the fetch instead
/// of shipping the wrong bytes. Blocking (the fetch half may clone); wrap in
/// `spawn_blocking` inside Tokio.
pub(crate) fn resolve_pin_to_bytes<E: RepoCacheEntry>(
    peppy_dirs: &PeppyDirs,
    entries: &[E],
    pin: &PinnedItem,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<(PathBuf, Vec<u8>), String> {
    if let Some(entry) = lookup_by_content(entries, pin) {
        match resolve_cached_artifact_path(peppy_dirs, entry.origin(), on_feedback).and_then(
            |path| match std::fs::read(&path) {
                Ok(bytes) => Ok((path, bytes)),
                Err(e) => Err(format!("failed to read {}: {e}", path.display())),
            },
        ) {
            Ok((path, bytes)) if ManifestFingerprint::of_bytes(&bytes) == pin.sha256 => {
                return Ok((path, bytes));
            }
            Ok(_) => on_feedback(&format!(
                "Local copy of {} drifted from its fingerprint; fetching the pin",
                pin.label()
            )),
            Err(reason) => on_feedback(&format!(
                "Local copy of {} is unusable ({reason}); fetching the pin",
                pin.label()
            )),
        }
    }
    resolve_pinned_bytes(peppy_dirs, pin, on_feedback)
}

/// Resolves a pinned contract or pairing document to a parsed value, through
/// [`resolve_pin_to_bytes`]. The pinned counterpart of [`resolve_cached_doc`]:
/// where that function turns a manifest's `name:tag[@sha256]` reference into
/// a document via this machine's own cache rules, this one consumes a
/// coordinator's decision and never consults them.
pub(crate) fn resolve_pinned_doc<E: RepoCacheEntry, T>(
    peppy_dirs: &PeppyDirs,
    entries: &[E],
    pin: &PinnedItem,
    parse: impl FnOnce(&str) -> std::result::Result<T, String>,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<T, String> {
    let (path, bytes) = resolve_pin_to_bytes(peppy_dirs, entries, pin, on_feedback)?;
    let content = std::str::from_utf8(&bytes).map_err(|e| {
        format!(
            "{}: content at {} is not UTF-8: {e}",
            pin.label(),
            path.display()
        )
    })?;
    parse(content).map_err(|e| format!("failed to parse pinned {}: {e}", pin.label()))
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

/// Constructors for the parsed values cache entries hold, so tests state
/// the fact they care about rather than a fingerprint and a commit spelled
/// out in full every time.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use daemon_config::repository::{GitCommit, RepoRelativePath};

    /// A distinct, valid fingerprint per `seed`. Deterministic, so a test
    /// that asserts on one reads the same way twice.
    pub(crate) fn fingerprint(seed: &str) -> ManifestFingerprint {
        ManifestFingerprint::of_bytes(seed.as_bytes())
    }

    /// A distinct, valid commit per `seed`.
    pub(crate) fn commit(seed: &str) -> GitCommit {
        GitCommit::parse(&fingerprint(seed).as_str()[..40]).expect("40 hex chars is a commit")
    }

    pub(crate) fn fs_origin(path: &str) -> EntryOrigin {
        EntryOrigin::Fs {
            path: PathBuf::from(path),
        }
    }

    pub(crate) fn git_origin(
        repo_url: &str,
        repo_ref: &str,
        seed: &str,
        path: &str,
    ) -> EntryOrigin {
        EntryOrigin::Git {
            repo_url: repo_url.to_owned(),
            repo_ref: Some(repo_ref.to_owned()),
            commit: commit(seed),
            path: RepoRelativePath::parse(path).expect("test path is repository-relative"),
        }
    }

    pub(crate) fn node_entry(name: &str, tag: &str, origin: EntryOrigin) -> NodeCacheEntry {
        NodeCacheEntry {
            node_name: ItemName::parse(name).expect("test node name is valid"),
            node_tag: ItemTag::parse(tag).expect("test node tag is valid"),
            sha256: fingerprint(&format!("{name}:{tag}")),
            origin,
            repo_id: 0,
        }
    }

    pub(crate) fn launcher_entry(name: &str, origin: EntryOrigin) -> LauncherCacheEntry {
        LauncherCacheEntry {
            launcher_name: ItemName::parse(name).expect("test launcher name is valid"),
            sha256: fingerprint(name),
            origin,
            repo_id: 0,
        }
    }

    pub(crate) fn contract_entry(name: &str, tag: &str, origin: EntryOrigin) -> ContractCacheEntry {
        ContractCacheEntry {
            contract_name: ItemName::parse(name).expect("test contract name is valid"),
            tag: ItemTag::parse(tag).expect("test contract tag is valid"),
            sha256: fingerprint(&format!("{name}:{tag}")),
            origin,
            repo_id: 0,
        }
    }

    /// The same entry, attributed to `repo_id`.
    pub(crate) fn owned_by(mut entry: NodeCacheEntry, repo_id: u32) -> NodeCacheEntry {
        entry.repo_id = repo_id;
        entry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use daemon_config::repository::{GitCommit, RepoRelativePath};

    use super::test_support::*;

    fn mk_entry(name: &str, tag: &str, repo_id: u32) -> NodeCacheEntry {
        owned_by(
            node_entry(name, tag, fs_origin("/tmp/foo/peppy.json5")),
            repo_id,
        )
    }

    fn mk_fs_entry(name: &str, tag: &str, path: &str) -> NodeCacheEntry {
        node_entry(name, tag, fs_origin(path))
    }

    fn mk_launcher_entry(name: &str, path: &str, repo_id: u32) -> LauncherCacheEntry {
        let mut entry = launcher_entry(name, fs_origin(path));
        entry.repo_id = repo_id;
        entry
    }

    fn mk_git_launcher_entry(
        name: &str,
        repo_url: &str,
        repo_ref: &str,
        path: &str,
    ) -> LauncherCacheEntry {
        launcher_entry(name, git_origin(repo_url, repo_ref, name, path))
    }

    fn mk_contract_entry(name: &str, tag: &str, path: &str, repo_id: u32) -> ContractCacheEntry {
        let mut entry = contract_entry(name, tag, fs_origin(path));
        entry.repo_id = repo_id;
        entry
    }

    #[test]
    fn lookup_picks_lowest_repo_id() {
        let entries = vec![
            mk_entry("a", "v1", 5),
            mk_entry("a", "v1", 2),
            mk_entry("a", "v1", 9),
        ];
        let hit = lookup(&entries, "a", "v1")
            .expect("distinct repo ids are not ambiguous")
            .expect("should resolve");
        assert_eq!(hit.repo_id, 2);
    }

    /// Lookup falls back to repo priority alone: the highest-priority entry
    /// (lowest id) wins among entries that share `(name, tag)`.
    #[test]
    fn lookup_returns_highest_priority_when_multiple_match() {
        let entries = vec![mk_entry("a", "v1", 7), mk_entry("a", "v1", 3)];
        let hit = lookup(&entries, "a", "v1")
            .expect("distinct repo ids are not ambiguous")
            .expect("should resolve");
        assert_eq!(hit.repo_id, 3);
    }

    /// Two entries tied at the winning repo id have no defensible
    /// winner, so lookup reports the ambiguity instead of picking one.
    #[test]
    fn lookup_reports_ambiguity_within_one_repo() {
        let first = owned_by(mk_fs_entry("a", "v1", "/repo/rust/peppy.json5"), 2);
        let second = owned_by(mk_fs_entry("a", "v1", "/repo/python/peppy.json5"), 2);

        let err = lookup(&[first, second], "a", "v1").expect_err("should be ambiguous");

        assert_eq!(err.kind, "node");
        assert_eq!(err.id, "a:v1");
        assert_eq!(err.repo_id, 2);
        assert_eq!(
            err.paths,
            vec!["/repo/python/peppy.json5", "/repo/rust/peppy.json5"],
            "both claimants named, sorted"
        );
    }

    /// The message names both files and says what to do. It must not
    /// read as "not found": the identity is present, twice.
    #[test]
    fn ambiguity_message_names_both_claimants() {
        let first = owned_by(
            mk_fs_entry("uvc_recon", "v1", "uvc_recon/rust/peppy.json5"),
            1000,
        );
        let second = owned_by(
            mk_fs_entry("uvc_recon", "v1", "uvc_recon/python/peppy.json5"),
            1000,
        );

        let msg = lookup(&[first, second], "uvc_recon", "v1")
            .expect_err("should be ambiguous")
            .to_string();

        assert!(msg.contains("uvc_recon:v1"), "got: {msg}");
        assert!(msg.contains("uvc_recon/rust/peppy.json5"), "got: {msg}");
        assert!(msg.contains("uvc_recon/python/peppy.json5"), "got: {msg}");
        assert!(msg.contains("repository 1000"), "got: {msg}");
        assert!(!msg.contains("not found"), "got: {msg}");
    }

    /// A lower-id repository shadowing a higher-id one is a supported
    /// feature, so a tie at the *losing* id must not make the winner
    /// ambiguous.
    #[test]
    fn lookup_ignores_ties_below_the_winning_repo_id() {
        let entries = vec![
            mk_entry("a", "v1", 1),
            mk_entry("a", "v1", 7),
            mk_entry("a", "v1", 7),
        ];

        let hit = lookup(&entries, "a", "v1")
            .expect("only the winning id is checked")
            .expect("should resolve");

        assert_eq!(hit.repo_id, 1);
    }

    /// Launchers are untagged, so the ambiguity label is the bare name
    /// rather than a `name:` with an empty tag.
    #[test]
    fn launcher_ambiguity_uses_the_bare_name() {
        let entries = vec![
            mk_launcher_entry("bringup", "/repo/sim/bringup.json5", 3),
            mk_launcher_entry("bringup", "/repo/real/bringup.json5", 3),
        ];

        let err = lookup_launcher(&entries, "bringup").expect_err("should be ambiguous");

        assert_eq!(err.kind, "launcher");
        assert_eq!(err.id, "bringup");
    }

    /// `lookup_repo_entry_by_sha256` returns the entry whose content fingerprint
    /// matches exactly, bypassing the repo-priority tiebreak.
    #[test]
    fn lookup_by_sha256_returns_exact_match() {
        let mut older = mk_entry("a", "v1", 1);
        older.sha256 = fingerprint("older");
        let mut newer = mk_entry("a", "v1", 9);
        newer.sha256 = fingerprint("newer");
        let entries = vec![older, newer];

        let hit = lookup_repo_entry_by_sha256(&entries, "a", "v1", &fingerprint("newer")).unwrap();
        assert_eq!(hit.repo_id, 9);
        assert!(lookup_repo_entry_by_sha256(&entries, "a", "v1", &fingerprint("absent")).is_none());
    }

    #[test]
    fn load_returns_empty_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        assert!(load_node_cache(&peppy_dirs).unwrap().is_empty());
    }

    #[test]
    fn write_then_load_roundtrips_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let input = vec![
            node_entry(
                "a",
                "v1",
                git_origin(
                    "https://example.com/repo.git",
                    "main",
                    "a",
                    "nodes/a/peppy.json5",
                ),
            ),
            node_entry("b", "v2", fs_origin("/tmp/b/peppy.json5")),
        ];
        write_repo_cache(&peppy_dirs, &input).unwrap();
        let loaded = load_node_cache(&peppy_dirs).unwrap();
        assert_eq!(loaded.len(), 2);
        // `repo_id` is re-derived from `repositories.json5` at load time
        // rather than stored, so it is the one field that does not survive
        // as written.
        let written: Vec<NodeCacheEntry> = loaded
            .into_iter()
            .map(|mut e| {
                assert_eq!(e.repo_id, UNOWNED_REPO_ID, "no repository claims these");
                e.repo_id = 0;
                e
            })
            .collect();
        assert_eq!(
            written, input,
            "an entry survives the round trip whole, commit included"
        );
    }

    /// An entry no configured repository claims must not outrank one that
    /// a repository does claim. `repo remove` and `repo exclude` publish
    /// the new configuration before the re-index rewrites the caches, and
    /// a lookup landing in that window (or after a re-index that failed)
    /// would otherwise resolve to the repository the user just dropped.
    #[test]
    fn unowned_entries_rank_below_configured_repositories() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        let live = tmp.path().join("live_repo");
        let dropped = tmp.path().join("dropped_repo");
        std::fs::create_dir_all(peppy_dirs.conf_dir()).unwrap();
        std::fs::write(
            repositories_list_path(&peppy_dirs),
            serde_json::to_string(&serde_json::json!([
                { "id": 7, "type": "fs", "path": live.to_string_lossy() }
            ]))
            .unwrap(),
        )
        .unwrap();

        write_repo_cache(
            &peppy_dirs,
            &[
                mk_fs_entry("a", "v1", &dropped.join("a/peppy.json5").to_string_lossy()),
                mk_fs_entry("a", "v1", &live.join("a/peppy.json5").to_string_lossy()),
                mk_fs_entry("b", "v1", &dropped.join("b/peppy.json5").to_string_lossy()),
            ],
        )
        .unwrap();

        let entries = load_repo_cache::<NodeCacheEntry>(&peppy_dirs).unwrap();
        assert_eq!(entries[0].repo_id, UNOWNED_REPO_ID);
        assert_eq!(entries[1].repo_id, 7);

        let hit = lookup(&entries, "a", "v1")
            .expect("one entry per repository is not ambiguous")
            .expect("should resolve");
        assert_eq!(hit.repo_id, 7, "the configured repository wins");

        // Still resolvable on its own: a hand-written cache with no
        // matching repository entry keeps working.
        let hit = lookup(&entries, "b", "v1")
            .expect("a single claimant is not ambiguous")
            .expect("should resolve");
        assert_eq!(hit.repo_id, UNOWNED_REPO_ID);
    }

    // -- launcher cache tests --

    /// `write_repo_cache` writes launcher entries to the path returned by
    /// `launchers_repo_cache_path` with the on-disk launcher schema.
    #[test]
    fn write_launcher_cache_serializes_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let entries = vec![
            mk_git_launcher_entry(
                "openarm01_sim_teleop",
                "https://github.com/Peppy-bot/launchers-hub",
                "main",
                "openarm01_sim_teleop.json5",
            ),
            mk_launcher_entry("local_demo", "/tmp/local_demo.json5", 0),
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
            arr[0]["origin"]["repo_url"],
            "https://github.com/Peppy-bot/launchers-hub"
        );
        assert_eq!(arr[0]["origin"]["repo_ref"], "main");
        assert_eq!(
            arr[0]["origin"]["commit"].as_str().unwrap().len(),
            40,
            "a git origin records the commit it was read at"
        );
        assert_eq!(arr[1]["launcher_name"], "local_demo");
        assert_eq!(arr[1]["origin"]["source_type"], "fs");
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
            &[mk_launcher_entry("demo", abs.to_string_lossy().as_ref(), 0)],
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
            mk_launcher_entry("demo", "/path/to/demo_low_priority.json5", 3),
            mk_launcher_entry("demo", "/path/to/demo_high_priority.json5", 1),
        ];
        let hit = lookup_launcher(&entries, "demo")
            .expect("distinct repo ids are not ambiguous")
            .expect("should resolve");
        assert_eq!(hit.repo_id, 1);
        assert!(hit.origin.path_str().ends_with("demo_high_priority.json5"));
    }

    // -- resolve_cached_artifact_path tests --

    /// Initializes a bare-bones git repository at `repo_dir` and writes a
    /// single committed file at the given repo-relative path. Returns the
    /// branch name resolved from HEAD and the commit it created.
    fn init_repo_with_file(
        repo_dir: &Path,
        file_path: &str,
        contents: &str,
    ) -> (String, GitCommit) {
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
        let oid = repo
            .commit(Some("HEAD"), &sig, &sig, "seed", &tree, &[])
            .expect("commit");

        let branch = repo
            .head()
            .expect("head")
            .shorthand()
            .expect("shorthand")
            .to_owned();
        (
            branch,
            GitCommit::parse(&oid.to_string()).expect("a real commit is a full hash"),
        )
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
            &fs_origin(abs.to_string_lossy().as_ref()),
            &|_| {},
        )
        .expect("fs resolve should succeed");
        assert_eq!(resolved, abs);
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
        let (branch, commit) = init_repo_with_file(&source_repo_dir, "launchers/demo.json5", "{}");
        let repo_url = source_repo_dir.display().to_string();

        write_repo_cache(
            &peppy_dirs,
            &[launcher_entry(
                "demo",
                EntryOrigin::Git {
                    repo_url: repo_url.clone(),
                    repo_ref: Some(branch),
                    commit,
                    path: RepoRelativePath::parse("launchers/demo.json5").unwrap(),
                },
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

    // -- pin resolution tests --

    use daemon_config::repository::{PinKind, PinnedItem};

    fn pin_for(
        name: &str,
        tag: &str,
        sha256: ManifestFingerprint,
        origin: EntryOrigin,
    ) -> PinnedItem {
        PinnedItem {
            kind: PinKind::Node,
            name: ItemName::parse(name).expect("valid name"),
            tag: ItemTag::parse(tag).expect("valid tag"),
            sha256,
            origin,
        }
    }

    /// Content authorises reuse, provenance does not: a machine holding the
    /// pinned bytes as an entry of ANOTHER repository under another identity
    /// serves them from its own disk, and the pin's own remote is never
    /// contacted. The unreachable URL in the pin is what proves it: a fetch
    /// attempt would fail loudly.
    #[test]
    fn a_pin_reuses_local_content_from_another_repository_without_fetching() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let local_file = tmp.path().join("other_repo").join("peppy.json5");
        std::fs::create_dir_all(local_file.parent().unwrap()).unwrap();
        std::fs::write(&local_file, b"{ pinned bytes }").unwrap();
        let sha = ManifestFingerprint::of_bytes(b"{ pinned bytes }");

        let mut local_entry = node_entry(
            "other_name",
            "v9",
            fs_origin(local_file.to_string_lossy().as_ref()),
        );
        local_entry.sha256 = sha.clone();

        let pin = pin_for(
            "camera",
            "v1",
            sha,
            git_origin(
                "https://unreachable.invalid/hub.git",
                "main",
                "camera:v1",
                "camera/peppy.json5",
            ),
        );

        let (path, bytes) = resolve_pin_to_bytes(
            &peppy_dirs,
            std::slice::from_ref(&local_entry),
            &pin,
            &|_| {},
        )
        .expect("local content serves the pin");
        assert_eq!(path, local_file);
        assert_eq!(bytes, b"{ pinned bytes }");
    }

    /// A local entry whose file drifted from its recorded fingerprint is a
    /// miss, not an answer: the pin falls through to its own origin rather
    /// than shipping bytes that no longer match anything.
    #[test]
    fn a_drifted_local_copy_falls_through_to_the_pins_origin() {
        let peppy_tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(peppy_tmp.path());

        let source_parent = tempfile::tempdir().unwrap();
        let source_repo_dir = source_parent.path().join("hub");
        std::fs::create_dir_all(&source_repo_dir).unwrap();
        let pinned_body = "{ the pinned bytes }";
        let (branch, commit) =
            init_repo_with_file(&source_repo_dir, "camera/peppy.json5", pinned_body);
        let sha = ManifestFingerprint::of_bytes(pinned_body.as_bytes());

        // A local entry claims the same content but its file says otherwise.
        let drifted = peppy_tmp.path().join("drifted.json5");
        std::fs::write(&drifted, b"{ something else }").unwrap();
        let mut local_entry = node_entry(
            "camera",
            "v1",
            fs_origin(drifted.to_string_lossy().as_ref()),
        );
        local_entry.sha256 = sha.clone();

        let pin = pin_for(
            "camera",
            "v1",
            sha,
            EntryOrigin::Git {
                repo_url: source_repo_dir.display().to_string(),
                repo_ref: Some(branch),
                commit,
                path: RepoRelativePath::parse("camera/peppy.json5").unwrap(),
            },
        );

        let (path, bytes) = resolve_pin_to_bytes(
            &peppy_dirs,
            std::slice::from_ref(&local_entry),
            &pin,
            &|_| {},
        )
        .expect("the pin's own origin serves it");
        assert_ne!(path, drifted);
        assert_eq!(bytes, pinned_body.as_bytes());
    }

    /// Content fetched from the pin's origin that does not fingerprint to
    /// the pin is a refusal naming both fingerprints: the remote moved under
    /// the pin or the location resolved to something else, and neither may
    /// be papered over by running what was found.
    #[test]
    fn fetched_content_that_does_not_match_its_pin_is_refused_naming_both() {
        let peppy_tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(peppy_tmp.path());

        let source_parent = tempfile::tempdir().unwrap();
        let source_repo_dir = source_parent.path().join("hub");
        std::fs::create_dir_all(&source_repo_dir).unwrap();
        let (branch, commit) =
            init_repo_with_file(&source_repo_dir, "camera/peppy.json5", "{ actual bytes }");

        let pinned_sha = ManifestFingerprint::of_bytes(b"{ some other bytes }");
        let actual_sha = ManifestFingerprint::of_bytes(b"{ actual bytes }");
        let pin = pin_for(
            "camera",
            "v1",
            pinned_sha.clone(),
            EntryOrigin::Git {
                repo_url: source_repo_dir.display().to_string(),
                repo_ref: Some(branch),
                commit,
                path: RepoRelativePath::parse("camera/peppy.json5").unwrap(),
            },
        );

        let err = resolve_pinned_bytes(&peppy_dirs, &pin, &|_| {})
            .expect_err("a fingerprint mismatch must refuse");
        assert!(err.contains(pinned_sha.as_str()), "names the pin: {err}");
        assert!(
            err.contains(actual_sha.as_str()),
            "names what was found: {err}"
        );
        assert!(err.contains("moved under the pin"), "{err}");
    }

    /// A pinned path that is absent from the pinned commit refuses naming
    /// the path and the commit, so the operator can see whether the pin or
    /// the repository is wrong.
    #[test]
    fn a_pinned_path_absent_from_the_commit_is_refused() {
        let peppy_tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(peppy_tmp.path());

        let source_parent = tempfile::tempdir().unwrap();
        let source_repo_dir = source_parent.path().join("hub");
        std::fs::create_dir_all(&source_repo_dir).unwrap();
        let (branch, commit) = init_repo_with_file(&source_repo_dir, "camera/peppy.json5", "{}");

        let pin = pin_for(
            "camera",
            "v1",
            fingerprint("whatever"),
            EntryOrigin::Git {
                repo_url: source_repo_dir.display().to_string(),
                repo_ref: Some(branch),
                commit,
                path: RepoRelativePath::parse("elsewhere/peppy.json5").unwrap(),
            },
        );

        let err = resolve_pinned_bytes(&peppy_dirs, &pin, &|_| {})
            .expect_err("an absent pinned path must refuse");
        assert!(err.contains("elsewhere/peppy.json5"), "{err}");
        assert!(err.contains("absent from commit"), "{err}");
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
            &[mk_launcher_entry("demo", "/tmp/demo.json5", 0)],
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
        let entry = contract_entry(
            "uvc_camera",
            "v1",
            git_origin(
                "https://github.com/Peppy-bot/interfaces_hub",
                "main",
                "uvc_camera",
                "uvc_camera/peppy.json5",
            ),
        );
        let expected_sha = entry.sha256.to_string();
        let entries = vec![entry];
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
        assert_eq!(arr[0]["sha256"], expected_sha);
        assert_eq!(arr[0]["origin"]["source_type"], "git");
        assert_eq!(arr[0]["origin"]["path"], "uvc_camera/peppy.json5");
    }

    /// Lookup picks the lowest-`repo_id` entry, even when two repos
    /// declare contracts with the same `(name, tag)` and different
    /// `sha256` fingerprints.
    #[test]
    fn lookup_contract_picks_highest_priority_repo() {
        let entries = vec![
            mk_contract_entry("uvc_camera", "v1", "/b/peppy.json5", 5),
            mk_contract_entry("uvc_camera", "v1", "/a/peppy.json5", 1),
        ];
        let hit = lookup_repo_entry(&entries, "uvc_camera", "v1")
            .expect("distinct repo ids are not ambiguous")
            .expect("should resolve");
        assert_eq!(hit.repo_id, 1);
        assert_eq!(hit.origin.path_str(), "/a/peppy.json5");
    }

    /// A sha256 lookup returns the exact content match regardless of repo
    /// priority.
    #[test]
    fn lookup_contract_by_sha256_returns_exact_match() {
        let entries = vec![
            {
                let mut e = mk_contract_entry("uvc_camera", "v1", "/a/peppy.json5", 1);
                e.sha256 = fingerprint("a");
                e
            },
            {
                let mut e = mk_contract_entry("uvc_camera", "v1", "/b/peppy.json5", 9);
                e.sha256 = fingerprint("b");
                e
            },
        ];
        let hit =
            lookup_repo_entry_by_sha256(&entries, "uvc_camera", "v1", &fingerprint("b")).unwrap();
        assert_eq!(hit.repo_id, 9);
        assert!(
            lookup_repo_entry_by_sha256(&entries, "uvc_camera", "v1", &fingerprint("absent"))
                .is_none()
        );
    }
}
