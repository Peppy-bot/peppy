//! Discovering what a repository publishes, and recording it in the
//! repository's own index.
//!
//! Two directions, and they meet at the index. `peppy repo index` walks a
//! repository to produce its `peppy_repository.json5`, which states the
//! identities the repository publishes and where each is declared, and
//! [`read_published_items`] reads that statement back to build a machine's
//! caches. The walk is how a repository author states what is there; the
//! index is what every reader agrees on.
//!
//! A given identity reaches [`WalkResult::items`] at most once per walk, but
//! every claimant is recorded: an identity with several claimants comes back
//! in [`WalkResult::conflicts`] so generation refuses rather than silently
//! promoting one of two answers. The cross-repository merge, where several
//! repositories claiming one identity is a supported feature rather than a
//! conflict, happens in `refresh.rs`.

use crate::services::repo::cache::{
    ContractCacheEntry, EntryOrigin, LauncherCacheEntry, NodeCacheEntry, PairingCacheEntry,
    RepoItems,
};
use crate::services::repo::refresh::RepoFailureKind;
use config::node::NodeConfigParser;
use config::schema::PeppySchema;
use core_node_api::encoding::RepoItemKind;
use daemon_config::contract::PeppyContractParser;
use daemon_config::launcher::PeppyLauncherParser;
use daemon_config::pairing::PeppyPairingParser;
use daemon_config::repository::{
    DeclaredItem, GitCommit, ItemName, ItemTag, ManifestFingerprint, PeppyRepositoryIndexParser,
    RepoRelativePath, RepositoryIndex,
};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tracing::debug;

/// Directory names that are never descended into while looking for the
/// files that declare items.
pub(crate) const PRUNED_DIR_NAMES: &[&str] = &[
    ".git",
    ".peppy",
    "target",
    "node_modules",
    ".venv",
    "__pycache__",
];

/// One item found by walking a repository: the identity it declares and
/// where it is declared.
///
/// The input to index generation, and only that: what a machine caches is
/// built from what the repository's committed index declares, not from one
/// machine's reading of the tree. The fields are exactly what an index
/// entry states, so a walk cannot record provenance the index has no way
/// to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WalkedItem {
    pub kind: RepoItemKind,
    pub name: String,
    /// Empty for launchers, matching the identity key the caches use.
    pub tag: String,
    /// Relative to the root of the scanned tree.
    pub relative_path: String,
}

/// Items found by walking one repository's working tree.
pub(crate) struct WalkResult {
    pub items: Vec<WalkedItem>,
    /// Identities claimed by more than one manifest in this repository.
    /// Non-empty means the repository has no defensible answer for those
    /// identities, so the caller refuses it rather than picking one.
    pub conflicts: Vec<RepoConflict>,
}

/// An identity claimed by several manifests inside one repository. There
/// is no priority rule that can settle this: the repository states two
/// different answers to the same question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoConflict {
    pub kind: RepoItemKind,
    pub name: String,
    /// Empty for untagged kinds (launchers).
    pub tag: String,
    /// Every claimant's path, sorted.
    pub paths: Vec<String>,
}

/// `name:tag`, or the bare name when the tag is empty. The empty string is
/// how the walk and caches spell an untagged identity (launchers), so this is
/// the single place that renders a `(name, tag)` pair as its label.
fn format_identity(name: &str, tag: &str) -> String {
    if tag.is_empty() {
        name.to_owned()
    } else {
        format!("{name}:{tag}")
    }
}

impl RepoConflict {
    /// `name:tag` label, or the bare name for untagged kinds.
    fn display_id(&self) -> String {
        format_identity(&self.name, &self.tag)
    }
}

impl std::fmt::Display for RepoConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} manifests claim `{}`: {}",
            self.paths.len(),
            self.kind,
            self.display_id(),
            self.paths.join(", ")
        )
    }
}

/// Every path that claimed a given `(name, tag)` during one repository
/// walk, in the order the walker met them. Recording all of them (rather
/// than remembering only that the identity was seen) is what lets a
/// conflict name both files instead of silently keeping one.
type ClaimMap = HashMap<(String, String), Vec<String>>;

/// Turns one kind's claim map into the conflicts it holds: every identity
/// with more than one claimant. Sorted by identity, with sorted paths, so
/// the report never inherits the filesystem's walk order and stays
/// comparable between machines.
fn conflicts_from_claims(kind: RepoItemKind, claims: ClaimMap) -> Vec<RepoConflict> {
    let mut conflicts: Vec<RepoConflict> = claims
        .into_iter()
        .filter(|(_, paths)| paths.len() > 1)
        .map(|((name, tag), mut paths)| {
            paths.sort();
            RepoConflict {
                kind,
                name,
                tag,
                paths,
            }
        })
        .collect();
    conflicts.sort_by(|a, b| (&a.name, &a.tag).cmp(&(&b.name, &b.tag)));
    conflicts
}

/// One item a repository publishes, resolved against the tree it was read
/// from: the identity the index states, the fingerprint of the bytes that
/// declare it, and where those bytes live.
#[derive(Debug)]
pub(crate) struct PublishedItem {
    pub kind: RepoItemKind,
    pub name: ItemName,
    /// `None` for a launcher, the only kind without a tag.
    pub tag: Option<ItemTag>,
    pub sha256: ManifestFingerprint,
    pub origin: EntryOrigin,
}

/// Where the tree being read came from, which is all that separates reading
/// a repository checked out on this machine from reading a clone of a
/// remote.
///
/// Held as data rather than passed as a closure that builds origins, so the
/// canonical root never leaves [`read_published_items`]: an fs origin is
/// absolute and canonical by construction instead of by every caller
/// remembering to resolve the root and join it back on.
pub(crate) enum ReadSource {
    /// A tree on this machine, read where it lies.
    Fs {
        /// Subtrees this machine will not answer for. A repository states
        /// one location per identity, so excluding part of the tree is a
        /// question about which of those locations to serve, and it is
        /// asked of the location the index declares rather than of a
        /// traversal.
        excluded: Vec<PathBuf>,
    },
    /// A clone of a remote, read at one commit.
    Git {
        repo_url: String,
        /// The ref the repository is configured to follow, absent when it
        /// follows whatever the remote's default branch is.
        repo_ref: Option<String>,
        commit: GitCommit,
    },
}

impl ReadSource {
    /// The origin the caches record for an item declared at `path`, under a
    /// `root` that has already been canonicalized.
    fn origin_of(&self, root: &Path, path: &RepoRelativePath) -> EntryOrigin {
        match self {
            ReadSource::Fs { .. } => EntryOrigin::Fs {
                path: root.join(path.as_path()),
            },
            ReadSource::Git {
                repo_url,
                repo_ref,
                commit,
            } => EntryOrigin::Git {
                repo_url: repo_url.clone(),
                repo_ref: repo_ref.clone(),
                commit: commit.clone(),
                path: path.clone(),
            },
        }
    }

    /// Whether this machine answers for the item declared at `origin`.
    ///
    /// Only a tree on this machine can have parts of it held back: a git
    /// origin names a path inside somebody else's repository, which this
    /// machine has no subtree opinion about.
    fn serves(&self, origin: &EntryOrigin) -> bool {
        match (self, origin) {
            (ReadSource::Fs { excluded }, EntryOrigin::Fs { path }) => {
                !excluded.iter().any(|subtree| path.starts_with(subtree))
            }
            _ => true,
        }
    }
}

/// Reads what the repository at `root` publishes, and resolves every item
/// it declares against the tree.
///
/// The index is the only input. A repository states its own contents, so
/// every machine reading a given revision of it agrees on what is there and
/// where; a directory walk would instead produce one machine's reading of
/// the tree and present it as the repository's statement.
///
/// `source` is the one thing that differs between reading a checkout on
/// this machine and reading a clone of a remote; the origin each item
/// records is built from it here, against the root this function already
/// canonicalized.
///
/// Every declared item is resolved before anything is returned, so a
/// repository with three broken entries reports three rather than the first.
pub(crate) fn read_published_items(
    root: &Path,
    source: &ReadSource,
) -> std::result::Result<Vec<PublishedItem>, (RepoFailureKind, String)> {
    let unreachable = |detail: String| (RepoFailureKind::Unreachable, detail);
    let contradictory = |detail: String| (RepoFailureKind::Conflict, detail);

    let root = std::fs::canonicalize(root)
        .map_err(|e| unreachable(format!("{} cannot be read: {e}", root.display())))?;
    let index_file = daemon_config::consts::REPOSITORY_INDEX_FILE;
    if !root.join(index_file).exists() {
        // Nothing was read, rather than something read wrong: the
        // repository states nothing about what it holds.
        return Err(unreachable(format!(
            "{index_file} is missing from the repository root. A repository publishes what it \
             holds by committing that file; run `peppy repo index` in the repository and commit \
             the result"
        )));
    }

    let index = read_repository_index(&root).map_err(|e| contradictory(e.to_string()))?;
    let mut items = Vec::new();
    let mut problems = Vec::new();
    for item in index.declared_items() {
        // Asked before the file is opened: an item this machine does not
        // answer for is not read, not fingerprinted, and not reported on,
        // which is the whole point of naming a subtree it should stay out
        // of.
        let origin = source.origin_of(&root, item.path);
        if !source.serves(&origin) {
            continue;
        }
        match resolve_declared_item(&root, &item) {
            Ok(bytes) => items.push(PublishedItem {
                kind: item.kind,
                name: item.name.clone(),
                tag: item.tag.cloned(),
                sha256: ManifestFingerprint::of_bytes(&bytes),
                origin,
            }),
            Err(detail) => problems.push(format!("{item} points at {}, which {detail}", item.path)),
        }
    }

    if problems.is_empty() {
        return Ok(items);
    }
    Err(contradictory(problems.join("; ")))
}

/// Turns published items into the four cache-entry vectors.
///
/// The one place an identity crosses from what a repository declares into
/// what a machine caches, so the two cannot disagree about which field
/// holds what.
pub(crate) fn build_cache_entries(
    items: Vec<PublishedItem>,
) -> std::result::Result<RepoItems, String> {
    let mut built = RepoItems::default();
    for item in items {
        // A tag is present for exactly the three tagged kinds, because the
        // index nests those under one and launchers one level less deep.
        // Spelling the mismatch out keeps the conversion total rather than
        // resting the invariant on a panic.
        let tagged = |item: &PublishedItem| -> std::result::Result<ItemTag, String> {
            item.tag
                .clone()
                .ok_or_else(|| format!("{} `{}` is indexed without a tag", item.kind, item.name))
        };
        match item.kind {
            RepoItemKind::Node => built.nodes.push(NodeCacheEntry {
                node_name: item.name.clone(),
                node_tag: tagged(&item)?,
                sha256: item.sha256,
                origin: item.origin,
                repo_id: 0,
            }),
            RepoItemKind::Launcher => built.launchers.push(LauncherCacheEntry {
                launcher_name: item.name,
                sha256: item.sha256,
                origin: item.origin,
                repo_id: 0,
            }),
            RepoItemKind::Contract => built.contracts.push(ContractCacheEntry {
                contract_name: item.name.clone(),
                tag: tagged(&item)?,
                sha256: item.sha256,
                origin: item.origin,
                repo_id: 0,
            }),
            RepoItemKind::Pairing => built.pairings.push(PairingCacheEntry {
                pairing_name: item.name.clone(),
                tag: tagged(&item)?,
                sha256: item.sha256,
                origin: item.origin,
                repo_id: 0,
            }),
        }
    }
    Ok(built)
}

/// Why a repository's index could not be produced, read, or trusted.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("{path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("{0}")]
    Unreadable(String),
    /// One identity claimed by several manifests. There is no index that
    /// can state this repository's contents, so generation refuses rather
    /// than writing one of the two answers.
    #[error("{}", format_conflicts(.0))]
    ContestedIdentity(Vec<RepoConflict>),
    /// An item the index has no way to state, such as a tag the caches
    /// could never key on. Refused at generation rather than skipped: a
    /// silent skip would produce an index that `--check` calls correct
    /// while the item is invisible to peppy.
    #[error("{0}")]
    Unrepresentable(String),
}

fn format_conflicts(conflicts: &[RepoConflict]) -> String {
    conflicts
        .iter()
        .map(|conflict| conflict.to_string())
        .collect::<Vec<_>>()
        .join("; ")
}

/// How a committed index differs from the repository it describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexDrift {
    /// An item in the tree that the index does not publish. Left alone it
    /// would simply be invisible, which is why the check exists.
    Unlisted {
        kind: RepoItemKind,
        id: String,
        path: String,
    },
    /// An entry naming something that is not the item it was filed under:
    /// a missing file, a file of another kind, or one whose manifest
    /// declares a different identity.
    Unmatched {
        kind: RepoItemKind,
        id: String,
        path: String,
        detail: String,
    },
    /// The right identity, declared somewhere else than the index says.
    Moved {
        kind: RepoItemKind,
        id: String,
        listed: String,
        found: String,
    },
}

impl std::fmt::Display for IndexDrift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexDrift::Unlisted { kind, id, path } => write!(
                f,
                "{path} declares {kind} `{id}`, which is not listed in {}",
                daemon_config::consts::REPOSITORY_INDEX_FILE
            ),
            IndexDrift::Unmatched {
                kind,
                id,
                path,
                detail,
            } => write!(f, "{kind} `{id}` points at {path}, which {detail}"),
            IndexDrift::Moved {
                kind,
                id,
                listed,
                found,
            } => write!(
                f,
                "{kind} `{id}` is listed at {listed} but is declared at {found}"
            ),
        }
    }
}

/// Walks `root` and produces the index it should contain.
pub fn generate_repository_index(root: &Path) -> Result<RepositoryIndex, IndexError> {
    let walked = walk_directory(root, &[]);
    if !walked.conflicts.is_empty() {
        return Err(IndexError::ContestedIdentity(walked.conflicts));
    }

    let mut index = RepositoryIndex::default();
    for item in walked.items {
        let declared = declare_walked_item(&mut index, &item);
        declared.map_err(|detail| {
            IndexError::Unrepresentable(format!("{}: {detail}", item.relative_path))
        })?;
    }
    Ok(index)
}

/// Records one walked item in `index`, converting the strings the walk
/// produced into the validated identity the index is keyed on.
fn declare_walked_item(
    index: &mut RepositoryIndex,
    item: &WalkedItem,
) -> std::result::Result<(), String> {
    let name = ItemName::parse(&item.name)?;
    let tag = if item.kind == RepoItemKind::Launcher {
        None
    } else {
        Some(ItemTag::parse(&item.tag)?)
    };
    let path = RepoRelativePath::parse(&item.relative_path).map_err(|e| e.to_string())?;
    index.declare(item.kind, name, tag, path)
}

/// Publishes what `root` holds: generates its index and writes it, returning
/// the index so a caller can report what it declared.
///
/// The pair is what "publishing a repository" means, and every caller wants
/// both halves, so the pair is named once rather than spelled out at each
/// site with its own wording for the same two steps.
pub fn publish_repository_index(root: &Path) -> Result<RepositoryIndex, IndexError> {
    let index = generate_repository_index(root)?;
    write_repository_index(root, &index)?;
    Ok(index)
}

/// Writes `index` to `root`, replacing any existing one.
pub fn write_repository_index(root: &Path, index: &RepositoryIndex) -> Result<(), IndexError> {
    let path = root.join(daemon_config::consts::REPOSITORY_INDEX_FILE);
    let content = json5_pretty::to_string_pretty(index)
        .map_err(|e| IndexError::Unreadable(format!("failed to serialize the index: {e}")))?;
    daemon_config::atomic_write::publish_atomic(&path, |tmp| {
        std::fs::write(tmp, format!("{content}\n"))
    })
    .map_err(|source| IndexError::Io {
        path: path.display().to_string(),
        source,
    })?;
    Ok(())
}

/// Reads the index committed at `root`.
pub fn read_repository_index(root: &Path) -> Result<RepositoryIndex, IndexError> {
    let path = root.join(daemon_config::consts::REPOSITORY_INDEX_FILE);
    PeppyRepositoryIndexParser::from_path(&path).map_err(|e| IndexError::Unreadable(e.to_string()))
}

/// Resolves one declared item against the tree at `root`, returning the
/// bytes of the file that declares it.
///
/// `root` must already be canonical. The parse is not redundant with the
/// index: the index states where an item is, the manifest states what it
/// is. Without checking the two agree, a stale index would silently serve
/// the wrong identity and a repository could file a README under `nodes`.
pub(crate) fn resolve_declared_item(
    root: &Path,
    item: &DeclaredItem<'_>,
) -> std::result::Result<Vec<u8>, String> {
    let joined = root.join(item.path.as_path());
    // Canonicalizing is the only way to catch a target reached through a
    // symlink that leaves the tree: the walk never follows a symlinked
    // directory, but reading a symlinked file does follow it.
    let canonical = std::fs::canonicalize(&joined).map_err(|e| format!("cannot be read: {e}"))?;
    if !canonical.starts_with(root) {
        return Err(format!(
            "resolves to {}, which is outside the repository",
            canonical.display()
        ));
    }
    if !canonical.is_file() {
        return Err("is not a file".to_owned());
    }
    let bytes = std::fs::read(&canonical).map_err(|e| format!("cannot be read: {e}"))?;
    let content = std::str::from_utf8(&bytes).map_err(|e| format!("is not valid UTF-8: {e}"))?;

    let (name, tag) = identity_of(item.kind, content, item.path.as_path())
        .map_err(|detail| format!("is not a {}: {detail}", item.kind))?;
    let expected_tag = item.tag.map(ItemTag::as_str).unwrap_or_default();
    if name != item.name.as_str() || tag != expected_tag {
        let found = format_identity(&name, &tag);
        return Err(format!("declares `{found}`"));
    }

    Ok(bytes)
}

/// The identity a document declares when read as `kind`: its `(name, tag)`,
/// with an empty tag for a launcher.
///
/// The single place that maps a kind to its parser and pulls the identity out.
/// Both the walk (discovering items) and [`resolve_declared_item`] (verifying
/// a listed path still declares what it was filed under) go through it, so the
/// two ways of learning what a file declares cannot disagree. `Err` carries why
/// the file is not a usable `kind`: it does not parse as one, carries a
/// `peppy_schema` that is not this kind's, or (for a launcher) has no usable
/// file stem to take a name from.
fn identity_of(
    kind: RepoItemKind,
    content: &str,
    path: &Path,
) -> std::result::Result<(String, String), String> {
    match kind {
        RepoItemKind::Node => {
            let parsed = NodeConfigParser::from_content(content).map_err(|e| e.to_string())?;
            Ok((
                parsed.manifest.name.as_str().to_owned(),
                parsed.manifest.tag.clone(),
            ))
        }
        RepoItemKind::Launcher => {
            let parsed = PeppyLauncherParser::from_content(content).map_err(|e| e.to_string())?;
            if parsed.peppy_schema != PeppySchema::LauncherV1 {
                return Err("wrong schema tag".to_owned());
            }
            // Launcher name = basename without `.json5` (launcher documents
            // carry no manifest name). This matches `resolve_launcher_path`,
            // which appends `.json5` to a bare name when looking up a launcher
            // file.
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|stem| !stem.is_empty())
                .ok_or_else(|| "has no usable file stem".to_owned())?;
            Ok((stem.to_owned(), String::new()))
        }
        RepoItemKind::Contract => {
            let parsed = PeppyContractParser::from_content(content).map_err(|e| e.to_string())?;
            Ok((
                parsed.manifest.name.as_str().to_owned(),
                parsed.manifest.tag.clone(),
            ))
        }
        RepoItemKind::Pairing => {
            let parsed = PeppyPairingParser::from_content(content).map_err(|e| e.to_string())?;
            Ok((
                parsed.manifest.name.as_str().to_owned(),
                parsed.manifest.tag.clone(),
            ))
        }
    }
}

/// Verifies the index committed at `root` against the repository itself.
///
/// Three steps, and all three are load-bearing. Parsing refuses a
/// duplicated identity and a path that leaves the tree. Resolving proves
/// every listed path is a real file inside the tree declaring what it was
/// filed under, which is what catches a symlinked file pointing outside:
/// the walk collects it and regeneration reproduces it, so comparing alone
/// would pass while `repo refresh` refused the repository. Comparing
/// against a fresh walk is what catches an item nobody listed.
///
/// An empty result means the index matches the repository.
pub fn check_repository_index(root: &Path) -> Result<Vec<IndexDrift>, IndexError> {
    let root = std::fs::canonicalize(root).map_err(|source| IndexError::Io {
        path: root.display().to_string(),
        source,
    })?;
    let committed = read_repository_index(&root)?;
    let generated = generate_repository_index(&root)?;

    let listed_paths: HashMap<(&str, String), &str> = committed
        .declared_items()
        .map(|item| {
            (
                (item.kind.as_str(), identity_label(&item)),
                item.path.as_str(),
            )
        })
        .collect();

    let mut drifts = Vec::new();
    let mut moved_identities = HashSet::new();
    for item in generated.declared_items() {
        let id = identity_label(&item);
        let found = item.path.as_str().to_owned();
        match listed_paths.get(&(item.kind.as_str(), id.clone())) {
            None => drifts.push(IndexDrift::Unlisted {
                kind: item.kind,
                id,
                path: found,
            }),
            Some(listed) if **listed != found => {
                moved_identities.insert((item.kind.as_str(), id.clone()));
                drifts.push(IndexDrift::Moved {
                    kind: item.kind,
                    id,
                    listed: (*listed).to_owned(),
                    found,
                });
            }
            Some(_) => {}
        }
    }

    for item in committed.declared_items() {
        let id = identity_label(&item);
        // An identity found somewhere else is already reported as moved,
        // and the stale path failing to resolve is the same fact told
        // twice.
        if moved_identities.contains(&(item.kind.as_str(), id.clone())) {
            continue;
        }
        if let Err(detail) = resolve_declared_item(&root, &item) {
            drifts.push(IndexDrift::Unmatched {
                kind: item.kind,
                id,
                path: item.path.as_str().to_owned(),
                detail,
            });
        }
    }

    // Sorted by rendered text so the report is reproducible in a test and
    // comparable between two machines.
    drifts.sort_by_key(|drift| drift.to_string());
    Ok(drifts)
}

fn identity_label(item: &DeclaredItem<'_>) -> String {
    format_identity(
        item.name.as_str(),
        item.tag.map(ItemTag::as_str).unwrap_or_default(),
    )
}

/// Walk a repository's working tree for the files that declare items.
///
/// Any directory whose path matches one of the `excluded_paths` entries is
/// pruned from the walk (neither descended into nor scanned).
///
/// Each `.json5` file is read once and dispatched by the `peppy_schema` value
/// its body declares. A node manifest is required to declare `node/v1`, so the
/// filename carries no special meaning here: a node in `peppy.json5` and a node
/// in any other `.json5` are both found by the same schema dispatch.
pub(crate) fn walk_directory(root: &Path, excluded_paths: &[PathBuf]) -> WalkResult {
    // Canonicalize the root so that paths emitted by the walker share a
    // common prefix representation with the excluded paths (which come from
    // `ExclusionSet::load` already canonicalized). Without this, macOS
    // `/var/...` symlinks break subdirectory exclusion: the walker emits
    // `/var/...` while excluded paths resolve to `/private/var/...`.
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let excluded = excluded_paths.to_vec();
    let walker = ignore::WalkBuilder::new(&root)
        .hidden(true)
        // What a repository publishes must not depend on the machine doing
        // the reading. A developer's global gitignore or a clone's
        // `.git/info/exclude` are machine-local, so they are ignored here
        // while the repository's own `.gitignore` and `.ignore` still
        // apply. Without this, one contributor's git config could change
        // the index they generate and commit.
        .git_global(false)
        .git_exclude(false)
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

    let mut nodes_seen = ClaimMap::new();
    let mut launchers_seen = ClaimMap::new();
    let mut contracts_seen = ClaimMap::new();
    let mut pairings_seen = ClaimMap::new();
    let mut items: Vec<WalkedItem> = Vec::new();

    for entry in walker.flatten() {
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
            config_path,
            bytes: &bytes,
        };
        let Some(schema) = peek_peppy_schema(&bytes) else {
            continue;
        };
        match schema {
            PeppySchema::NodeV1 => {
                collect_item(&ctx, RepoItemKind::Node, &mut nodes_seen, &mut items)
            }
            PeppySchema::LauncherV1 => collect_item(
                &ctx,
                RepoItemKind::Launcher,
                &mut launchers_seen,
                &mut items,
            ),
            PeppySchema::ContractV1 => collect_item(
                &ctx,
                RepoItemKind::Contract,
                &mut contracts_seen,
                &mut items,
            ),
            PeppySchema::PairingV1 => {
                collect_item(&ctx, RepoItemKind::Pairing, &mut pairings_seen, &mut items)
            }
            // The repository's own index declares no item. It is the
            // output of this walk, not an input to it.
            PeppySchema::RepositoryV1 => {}
        }
    }

    let mut conflicts = conflicts_from_claims(RepoItemKind::Node, nodes_seen);
    conflicts.extend(conflicts_from_claims(
        RepoItemKind::Launcher,
        launchers_seen,
    ));
    conflicts.extend(conflicts_from_claims(
        RepoItemKind::Contract,
        contracts_seen,
    ));
    conflicts.extend(conflicts_from_claims(RepoItemKind::Pairing, pairings_seen));

    WalkResult { items, conflicts }
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

/// File context shared by every collector. The collectors only need read
/// access; bundling these arguments keeps their signatures focused on the
/// entry-specific state (claim map + output vector).
struct EntryContext<'a> {
    root: &'a Path,
    config_path: &'a Path,
    bytes: &'a [u8],
}

/// Reads one discovered `.json5` and, when it declares a usable identity of
/// `kind`, records the claim and pushes the item.
///
/// Every claimant is recorded in `claims`, including the second and later ones
/// that are not pushed to `out`; the caller turns identities with several
/// claimants into [`RepoConflict`]s and refuses the whole repository. Which
/// claimant reaches `out` is walk-order dependent and deliberately not relied
/// upon.
///
/// A file that does not name a usable identity (non-UTF-8 bytes, a parse
/// failure, a launcher with no usable stem) is skipped with a debug line
/// rather than failing the walk: the strict parse in [`identity_of`] catches
/// structural problems the cheap schema peek can't, and a repository is free
/// to hold `.json5` files that declare nothing.
fn collect_item(
    ctx: &EntryContext<'_>,
    kind: RepoItemKind,
    claims: &mut ClaimMap,
    out: &mut Vec<WalkedItem>,
) {
    let content = match std::str::from_utf8(ctx.bytes) {
        Ok(s) => s,
        Err(e) => {
            debug!(
                "Skipping non-utf8 {} .json5 at {}: {}",
                kind,
                ctx.config_path.display(),
                e
            );
            return;
        }
    };
    let (name, tag) = match identity_of(kind, content, ctx.config_path) {
        Ok(identity) => identity,
        Err(e) => {
            debug!(
                "Skipping {} .json5 at {}: {}",
                kind,
                ctx.config_path.display(),
                e
            );
            return;
        }
    };

    let relative_path = ctx
        .config_path
        .strip_prefix(ctx.root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let claimants = claims.entry((name.clone(), tag.clone())).or_default();
    claimants.push(relative_path.clone());
    if claimants.len() > 1 {
        return;
    }

    out.push(WalkedItem {
        kind,
        name,
        tag,
        relative_path,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::consts::NODE_CONFIG_FILE;
    use config::fingerprint::fingerprint_for_bytes;

    /// Helper: write a minimal valid node manifest under `dir`.
    fn write_node_json5(dir: &Path, name: &str, tag: &str) {
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

    /// Helper: write a minimal valid contract manifest at `path`.
    fn write_contract_json5(path: &Path, name: &str, tag: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(
            path,
            format!(
                r#"{{
  peppy_schema: "contract/v1",
  manifest: {{ name: "{name}", tag: "{tag}" }},
  interfaces: {{}}
}}"#
            ),
        )
        .unwrap();
    }

    /// Helper: write a minimal valid pairing manifest at `path`.
    fn write_pairing_json5(path: &Path, name: &str, tag: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(
            path,
            format!(
                r#"{{
  peppy_schema: "pairing/v1",
  manifest: {{ name: "{name}", tag: "{tag}" }},
  roles: ["leader", "follower"],
  topics: [
    {{ emitted_by: "leader", name: "setpoints" }},
    {{ emitted_by: "follower", name: "states" }}
  ]
}}"#
            ),
        )
        .unwrap();
    }

    /// The identities the walk found, of one kind, as `name:tag` labels.
    fn found(walked: &WalkResult, kind: RepoItemKind) -> Vec<String> {
        walked
            .items
            .iter()
            .filter(|item| item.kind == kind)
            .map(|item| format_identity(&item.name, &item.tag))
            .collect()
    }

    /// The walk dispatches `.json5` files by `peppy_schema`: a node
    /// manifest, a launcher, a contract and a pairing coexisting in the
    /// same repository each land under the matching kind.
    #[test]
    fn walk_directory_dispatches_by_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("mixed");
        write_node_json5(&repo.join("nodes/my_sensor"), "my_sensor", "v1");
        write_launcher_json5(&repo.join("teleop.json5"));
        write_contract_json5(
            &repo.join("interfaces/uvc_camera.json5"),
            "uvc_camera",
            "v1",
        );
        write_pairing_json5(&repo.join("robot/joint.json5"), "joint_link", "v1");

        let walked = walk_directory(&repo, &[]);

        assert_eq!(found(&walked, RepoItemKind::Node), vec!["my_sensor:v1"]);
        assert_eq!(found(&walked, RepoItemKind::Launcher), vec!["teleop"]);
        assert_eq!(
            found(&walked, RepoItemKind::Contract),
            vec!["uvc_camera:v1"]
        );
        assert_eq!(found(&walked, RepoItemKind::Pairing), vec!["joint_link:v1"]);
        assert!(walked.conflicts.is_empty(), "nothing claimed twice");
    }

    /// Every item records where it was found, relative to the scanned
    /// root, which is what an index entry states.
    #[test]
    fn walk_directory_records_repository_relative_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_node_json5(&repo.join("nodes/my_sensor"), "my_sensor", "v1");

        let walked = walk_directory(&repo, &[]);

        let item = &walked.items[0];
        assert_eq!(item.relative_path, "nodes/my_sensor/peppy.json5");
    }

    /// The repository's own index declares no item. It is the output of
    /// this walk, not an input to it.
    #[test]
    fn walk_directory_skips_the_repository_index_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_node_json5(&repo.join("a"), "a", "v1");
        std::fs::write(
            repo.join(daemon_config::consts::REPOSITORY_INDEX_FILE),
            r#"{ peppy_schema: "repository/v1", nodes: { "a": { "v1": { path: "a/peppy.json5" } } } }"#,
        )
        .unwrap();

        let walked = walk_directory(&repo, &[]);

        assert_eq!(found(&walked, RepoItemKind::Node), vec!["a:v1"]);
        assert_eq!(walked.items.len(), 1, "the index is not an item");
    }

    /// Build artifacts and vendored trees are never descended into, so a
    /// manifest under one of them is not published.
    #[test]
    fn walk_directory_skips_pruned_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_node_json5(&repo.join("real"), "real", "v1");
        for pruned in PRUNED_DIR_NAMES {
            write_node_json5(&repo.join(pruned).join("buried"), "buried", "v1");
        }

        let walked = walk_directory(&repo, &[]);

        assert_eq!(found(&walked, RepoItemKind::Node), vec!["real:v1"]);
    }

    /// The motivating case: one repository declaring one node identity
    /// twice. Both claimants are named, so the report points at the two
    /// files to look at rather than silently keeping whichever the
    /// filesystem happened to yield first.
    #[test]
    fn walk_directory_reports_node_claimed_twice() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("nodes-hub");
        write_node_json5(&repo.join("uvc_recon/rust"), "uvc_recon", "v1");
        write_node_json5(&repo.join("uvc_recon/python"), "uvc_recon", "v1");

        let walked = walk_directory(&repo, &[]);

        assert_eq!(walked.conflicts.len(), 1, "one contested identity");
        let conflict = &walked.conflicts[0];
        assert_eq!(conflict.kind, RepoItemKind::Node);
        assert_eq!(conflict.name, "uvc_recon");
        assert_eq!(conflict.tag, "v1");
        assert_eq!(
            conflict.paths,
            vec![
                "uvc_recon/python/peppy.json5".to_owned(),
                "uvc_recon/rust/peppy.json5".to_owned()
            ],
            "both claimants named, by repository-relative path"
        );
        // Only one claimant reaches the item vector; which one is walk
        // order and deliberately not asserted.
        assert_eq!(walked.items.len(), 1);
    }

    /// Launchers are keyed by file stem, so two identically named
    /// launcher files in one repository contest the same identity even
    /// though they live in different directories.
    #[test]
    fn walk_directory_reports_launcher_stem_claimed_twice() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("launchers-hub");
        write_launcher_json5(&repo.join("sim/bringup.json5"));
        write_launcher_json5(&repo.join("real/bringup.json5"));

        let walked = walk_directory(&repo, &[]);

        assert_eq!(walked.conflicts.len(), 1);
        assert_eq!(walked.conflicts[0].kind, RepoItemKind::Launcher);
        assert_eq!(walked.conflicts[0].name, "bringup");
        assert_eq!(walked.conflicts[0].tag, "", "launchers carry no tag");
        assert_eq!(walked.conflicts[0].paths.len(), 2);
    }

    /// Contracts and pairings are contested the same way as nodes, so a
    /// duplicate in either is caught rather than silently resolved.
    #[test]
    fn walk_directory_reports_contract_and_pairing_claimed_twice() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("contracts-hub");
        write_contract_json5(&repo.join("a/rgb.json5"), "rgb_camera", "v1");
        write_contract_json5(&repo.join("b/rgb.json5"), "rgb_camera", "v1");
        write_pairing_json5(&repo.join("a/joint.json5"), "joint_link", "v1");
        write_pairing_json5(&repo.join("b/joint.json5"), "joint_link", "v1");

        let walked = walk_directory(&repo, &[]);

        let kinds: Vec<RepoItemKind> = walked.conflicts.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            vec![RepoItemKind::Contract, RepoItemKind::Pairing],
            "one conflict per kind, grouped by kind"
        );
        assert_eq!(walked.conflicts[0].name, "rgb_camera");
        assert_eq!(walked.conflicts[1].name, "joint_link");
    }

    /// The report is ordered by identity with sorted paths, so it is
    /// reproducible in a test and comparable between two machines whose
    /// filesystems hand back directories in different orders.
    #[test]
    fn walk_directory_conflicts_are_ordered_independently_of_walk_order() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        for dir in ["zzz", "aaa", "mmm"] {
            write_node_json5(&repo.join(dir), "beta", "v1");
        }
        write_node_json5(&repo.join("one"), "alpha", "v1");
        write_node_json5(&repo.join("two"), "alpha", "v1");

        let walked = walk_directory(&repo, &[]);

        let names: Vec<&str> = walked.conflicts.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"], "sorted by identity");

        let beta_paths = &walked.conflicts[1].paths;
        assert_eq!(beta_paths.len(), 3, "every claimant is named, not just two");
        let mut sorted = beta_paths.clone();
        sorted.sort();
        assert_eq!(beta_paths, &sorted, "paths sorted");
    }

    /// Same name under different tags is two identities, not a conflict.
    /// This is the guard against the check firing on ordinary versioning.
    #[test]
    fn walk_directory_same_name_different_tags_is_not_a_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_node_json5(&repo.join("v1"), "my_sensor", "v1");
        write_node_json5(&repo.join("v2"), "my_sensor", "v2");

        let walked = walk_directory(&repo, &[]);

        assert!(walked.conflicts.is_empty(), "{:?}", walked.conflicts);
        let mut identities = found(&walked, RepoItemKind::Node);
        identities.sort();
        assert_eq!(identities, vec!["my_sensor:v1", "my_sensor:v2"]);
    }

    /// Reading a repository yields the identity the index states, the
    /// fingerprint of the bytes that declare it, and where those bytes are.
    /// A git origin carries the commit, which is the whole point: it is
    /// what lets another machine read the same tree.
    #[test]
    fn read_published_items_records_the_identity_and_where_it_lives() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_node_json5(&repo.join("nodes/a"), "a", "v1");
        let index = generate_repository_index(&repo).expect("index the repository");
        write_repository_index(&repo, &index).expect("write the index");

        let commit = daemon_config::repository::GitCommit::parse(&"a".repeat(40)).unwrap();
        let git = read_published_items(
            &repo,
            &ReadSource::Git {
                repo_url: "https://example.invalid/hub".to_owned(),
                repo_ref: Some("main".to_owned()),
                commit: commit.clone(),
            },
        )
        .expect("read the published items");
        let entries = build_cache_entries(git).expect("build cache entries");
        assert_eq!(entries.nodes.len(), 1);
        assert_eq!(entries.nodes[0].origin.path_str(), "nodes/a/peppy.json5");
        assert_eq!(entries.nodes[0].origin.commit(), Some(&commit));

        let manifest = repo.join("nodes/a").join(config::consts::NODE_CONFIG_FILE);
        assert_eq!(
            entries.nodes[0].sha256,
            daemon_config::repository::ManifestFingerprint::of_bytes(
                &std::fs::read(&manifest).unwrap()
            ),
            "the fingerprint is of the bytes that declare the node"
        );

        let fs = read_published_items(
            &repo,
            &ReadSource::Fs {
                excluded: Vec::new(),
            },
        )
        .expect("read the published items");
        let entries = build_cache_entries(fs).expect("build cache entries");
        let path = Path::new(entries.nodes[0].origin.path_str());
        assert!(path.is_absolute());
        assert!(
            path.starts_with(std::fs::canonicalize(&repo).unwrap()),
            "an fs origin is joined onto the canonical root, so attribution \
             and exclusion can compare it against one"
        );
    }

    /// A repository with no index publishes nothing, and the refusal names
    /// the file and how to produce it. There is no walk to fall back to:
    /// what a machine caches is what the repository stated, or nothing.
    #[test]
    fn read_published_items_refuses_a_repository_with_no_index() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_node_json5(&repo.join("nodes/a"), "a", "v1");

        let (kind, detail) = read_published_items(
            &repo,
            &ReadSource::Fs {
                excluded: Vec::new(),
            },
        )
        .expect_err("an unpublished repository contributes nothing");
        assert_eq!(
            kind,
            RepoFailureKind::Unreachable,
            "nothing was read, which is not the same as reading something wrong"
        );
        assert!(
            detail.contains(daemon_config::consts::REPOSITORY_INDEX_FILE),
            "got: {detail}"
        );
        assert!(detail.contains("peppy repo index"), "got: {detail}");
    }

    /// An index entry naming a file that is not there names the item and
    /// the path, rather than dropping it and leaving the identity to
    /// resolve as "not found" somewhere much later.
    #[test]
    fn read_published_items_names_an_entry_whose_file_is_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_node_json5(&repo.join("nodes/a"), "a", "v1");
        let index = generate_repository_index(&repo).expect("index the repository");
        write_repository_index(&repo, &index).expect("write the index");
        std::fs::remove_file(repo.join("nodes/a").join(config::consts::NODE_CONFIG_FILE)).unwrap();

        let (kind, detail) = read_published_items(
            &repo,
            &ReadSource::Fs {
                excluded: Vec::new(),
            },
        )
        .expect_err("a listed path that is not there is a refusal");
        assert_eq!(
            kind,
            RepoFailureKind::Conflict,
            "the index states something the tree does not hold"
        );
        assert!(detail.contains("a:v1"), "got: {detail}");
        assert!(detail.contains("nodes/a/peppy.json5"), "got: {detail}");
    }

    /// The rollout depends on this: a peppy that predates `repository/v1`
    /// meets the schema tag, fails to recognize it, and skips the file. So
    /// a repository can commit its index before the peppy that requires
    /// one exists, and nothing in the fleet notices.
    #[test]
    fn an_unrecognized_schema_tag_is_skipped_rather_than_refused() {
        assert_eq!(
            peek_peppy_schema(br#"{ peppy_schema: "repository/v99" }"#),
            None
        );
        assert_eq!(peek_peppy_schema(br#"{ unrelated: true }"#), None);
        assert_eq!(
            peek_peppy_schema(br#"{ peppy_schema: "repository/v1" }"#),
            Some(PeppySchema::RepositoryV1)
        );
    }

    /// A repository with every kind is indexed exactly once per identity,
    /// in a deterministic order.
    #[test]
    fn generate_finds_every_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("hub");
        write_node_json5(&repo.join("nodes/my_sensor"), "my_sensor", "v1");
        write_node_json5(&repo.join("nodes/my_sensor_v2"), "my_sensor", "v2");
        write_launcher_json5(&repo.join("teleop.json5"));
        write_contract_json5(&repo.join("interfaces/rgb.json5"), "rgb_camera", "v1");
        write_pairing_json5(&repo.join("robot/joint.json5"), "joint_link", "v1");

        let index = generate_repository_index(&repo).expect("generates");

        let declared: Vec<String> = index
            .declared_items()
            .map(|item| format!("{item} -> {}", item.path))
            .collect();
        assert_eq!(
            declared,
            vec![
                "node `my_sensor:v1` -> nodes/my_sensor/peppy.json5",
                "node `my_sensor:v2` -> nodes/my_sensor_v2/peppy.json5",
                "launcher `teleop` -> teleop.json5",
                "contract `rgb_camera:v1` -> interfaces/rgb.json5",
                "pairing `joint_link:v1` -> robot/joint.json5",
            ]
        );
    }

    /// The same tree yields the same index regardless of the order its
    /// files were created in, so a generated index is comparable between
    /// machines rather than a record of one filesystem's walk order.
    #[test]
    fn generate_is_deterministic_across_creation_order() {
        let render = |order: [&str; 4]| {
            let tmp = tempfile::tempdir().unwrap();
            let repo = tmp.path().join("hub");
            for name in order {
                write_node_json5(&repo.join(name), name, "v1");
            }
            let index = generate_repository_index(&repo).expect("generates");
            json5_pretty::to_string_pretty(&index).expect("serializes")
        };
        assert_eq!(
            render(["zulu", "alpha", "mike", "bravo"]),
            render(["bravo", "mike", "alpha", "zulu"])
        );
    }

    /// Generation refuses a repository that claims one identity twice and
    /// names both files, so the author sees which two claims collide.
    #[test]
    fn generate_fails_naming_both_paths_on_a_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("hub");
        write_node_json5(&repo.join("uvc/rust"), "uvc", "v1");
        write_node_json5(&repo.join("uvc/python"), "uvc", "v1");

        let err = generate_repository_index(&repo).expect_err("a contested identity is refused");

        assert!(matches!(err, IndexError::ContestedIdentity(_)));
        let message = err.to_string();
        assert!(message.contains("uvc:v1"), "{message}");
        assert!(message.contains("uvc/rust/peppy.json5"), "{message}");
        assert!(message.contains("uvc/python/peppy.json5"), "{message}");
    }

    #[test]
    fn generate_fails_on_a_duplicate_launcher_stem() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("hub");
        write_launcher_json5(&repo.join("sim/bringup.json5"));
        write_launcher_json5(&repo.join("real/bringup.json5"));

        let err = generate_repository_index(&repo).expect_err("a contested stem is refused");
        assert!(err.to_string().contains("bringup"), "{err}");
    }

    /// An identity the index has no way to state is refused rather than
    /// skipped. Skipping would write an index that `--check` calls correct
    /// while the item is invisible to peppy.
    #[test]
    fn generate_fails_on_an_identity_the_index_cannot_state() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("hub");
        write_node_json5(&repo.join("dotted"), "my_sensor", "v0.1.0");

        let err = generate_repository_index(&repo).expect_err("a dotted tag is refused");
        assert!(matches!(err, IndexError::Unrepresentable(_)));
        let message = err.to_string();
        assert!(message.contains("dotted/peppy.json5"), "{message}");
        assert!(message.contains("v0.1.0"), "{message}");
    }

    /// A repository that publishes nothing gets an index with no sections,
    /// which is valid. Only a missing index means it has not adopted one.
    #[test]
    fn generate_produces_an_empty_index_for_an_empty_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("empty");
        std::fs::create_dir_all(&repo).unwrap();

        let index = generate_repository_index(&repo).expect("generates");
        assert_eq!(index.declared_count(), 0);

        write_repository_index(&repo, &index).expect("writes");
        assert!(check_repository_index(&repo).expect("checks").is_empty());
    }

    /// The load-bearing test for the migration: what the index reader
    /// resolves from a generated index is exactly what the walk found,
    /// identity for identity, path for path, byte for byte.
    #[test]
    fn generate_write_read_round_trip_matches_the_walk() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("hub");
        write_node_json5(&repo.join("nodes/a"), "a", "v1");
        write_node_json5(&repo.join("nodes/b"), "b", "v2");
        write_launcher_json5(&repo.join("teleop.json5"));
        write_contract_json5(&repo.join("interfaces/rgb.json5"), "rgb_camera", "v1");
        write_pairing_json5(&repo.join("robot/joint.json5"), "joint_link", "v1");

        let walked = walk_directory(&repo, &[]);
        let index = generate_repository_index(&repo).expect("generates");
        write_repository_index(&repo, &index).expect("writes");

        let root = std::fs::canonicalize(&repo).unwrap();
        let committed = read_repository_index(&root).expect("reads back");
        let resolved: Vec<(&str, String, String, String)> = committed
            .declared_items()
            .map(|item| {
                let bytes = resolve_declared_item(&root, &item).expect("resolves");
                (
                    item.kind.as_str(),
                    identity_label(&item),
                    item.path.as_str().to_owned(),
                    fingerprint_for_bytes(&bytes),
                )
            })
            .collect();

        let mut from_walk: Vec<(&str, String, String, String)> = walked
            .items
            .iter()
            .map(|item| {
                let bytes = std::fs::read(root.join(&item.relative_path)).expect("walked file");
                (
                    item.kind.as_str(),
                    format_identity(&item.name, &item.tag),
                    item.relative_path.clone(),
                    fingerprint_for_bytes(&bytes),
                )
            })
            .collect();
        from_walk.sort();
        let mut from_index = resolved;
        from_index.sort();
        assert_eq!(from_index, from_walk);
    }

    /// A freshly generated index matches the repository it came from.
    #[test]
    fn check_passes_on_a_freshly_generated_index() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("hub");
        write_node_json5(&repo.join("nodes/a"), "a", "v1");
        write_launcher_json5(&repo.join("teleop.json5"));
        let index = generate_repository_index(&repo).expect("generates");
        write_repository_index(&repo, &index).expect("writes");

        assert_eq!(check_repository_index(&repo).expect("checks"), vec![]);
    }

    /// The mistake people will actually make: add an item, forget the
    /// index. The item would otherwise be silently invisible.
    #[test]
    fn check_reports_an_item_nobody_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("hub");
        write_node_json5(&repo.join("nodes/a"), "a", "v1");
        let index = generate_repository_index(&repo).expect("generates");
        write_repository_index(&repo, &index).expect("writes");
        write_pairing_json5(&repo.join("robot/wrist.json5"), "wrist_link", "v1");

        let drifts = check_repository_index(&repo).expect("checks");

        assert_eq!(
            drifts,
            vec![IndexDrift::Unlisted {
                kind: RepoItemKind::Pairing,
                id: "wrist_link:v1".to_owned(),
                path: "robot/wrist.json5".to_owned(),
            }]
        );
        assert!(
            drifts[0].to_string().contains("peppy_repository.json5"),
            "{}",
            drifts[0]
        );
    }

    /// An item moved without updating the index, and an entry whose file
    /// is gone, are both named rather than silently resolving.
    #[test]
    fn check_reports_moved_and_unmatched_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("hub");
        write_node_json5(&repo.join("old"), "a", "v1");
        write_contract_json5(&repo.join("gone.json5"), "rgb_camera", "v1");
        let index = generate_repository_index(&repo).expect("generates");
        write_repository_index(&repo, &index).expect("writes");

        std::fs::remove_file(repo.join("gone.json5")).unwrap();
        std::fs::remove_dir_all(repo.join("old")).unwrap();
        write_node_json5(&repo.join("new"), "a", "v1");

        let drifts = check_repository_index(&repo).expect("checks");

        assert_eq!(
            drifts
                .iter()
                .filter(|d| matches!(d, IndexDrift::Moved { .. }))
                .cloned()
                .collect::<Vec<_>>(),
            vec![IndexDrift::Moved {
                kind: RepoItemKind::Node,
                id: "a:v1".to_owned(),
                listed: "old/peppy.json5".to_owned(),
                found: "new/peppy.json5".to_owned(),
            },],
            "a moved identity is reported once, as a move"
        );
        let unmatched = drifts
            .iter()
            .find_map(|d| match d {
                IndexDrift::Unmatched { id, detail, .. } => Some((id, detail)),
                _ => None,
            })
            .expect("the deleted contract");
        assert_eq!(unmatched.0, "rgb_camera:v1");
        assert!(unmatched.1.contains("cannot be read"), "{}", unmatched.1);
    }

    /// Kind is checked as much as identity: a contract filed under
    /// `nodes` produces both an unmatched entry and an unlisted item,
    /// because the comparison is per kind and the two cannot cancel out.
    #[test]
    fn check_reports_an_item_filed_under_the_wrong_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("hub");
        write_contract_json5(&repo.join("rgb.json5"), "rgb_camera", "v1");
        std::fs::write(
            repo.join(daemon_config::consts::REPOSITORY_INDEX_FILE),
            r#"{
  peppy_schema: "repository/v1",
  nodes: { "rgb_camera": { "v1": { path: "rgb.json5" } } },
}"#,
        )
        .unwrap();

        let drifts = check_repository_index(&repo).expect("checks");

        assert_eq!(drifts.len(), 2, "{drifts:?}");
        assert!(drifts.iter().any(|d| matches!(
            d,
            IndexDrift::Unlisted {
                kind: RepoItemKind::Contract,
                ..
            }
        )));
        assert!(drifts.iter().any(|d| matches!(
            d,
            IndexDrift::Unmatched {
                kind: RepoItemKind::Node,
                ..
            }
        )));
    }

    /// An entry pointing at the right kind but the wrong identity is
    /// caught by re-reading the manifest, which is why the reader parses
    /// rather than trusting the index.
    #[test]
    fn check_reports_an_entry_whose_manifest_declares_another_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("hub");
        write_node_json5(&repo.join("nodes/a"), "a", "v1");
        std::fs::write(
            repo.join(daemon_config::consts::REPOSITORY_INDEX_FILE),
            r#"{
  peppy_schema: "repository/v1",
  nodes: { "renamed": { "v1": { path: "nodes/a/peppy.json5" } } },
}"#,
        )
        .unwrap();

        let drifts = check_repository_index(&repo).expect("checks");

        let unmatched = drifts
            .iter()
            .find_map(|d| match d {
                IndexDrift::Unmatched { id, detail, .. } => Some((id, detail)),
                _ => None,
            })
            .expect("an unmatched entry");
        assert_eq!(unmatched.0, "renamed:v1");
        assert!(unmatched.1.contains("declares `a:v1`"), "{}", unmatched.1);
    }

    /// The one case comparing alone would miss: the walk does not follow
    /// a symlinked directory, but reading a symlinked file does follow
    /// it, so regeneration reproduces the entry and only resolving the
    /// path catches that it leaves the tree.
    #[test]
    #[cfg(unix)]
    fn check_rejects_a_symlinked_file_that_escapes_the_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        write_node_json5(&outside, "smuggled", "v1");

        let repo = tmp.path().join("hub");
        std::fs::create_dir_all(repo.join("nodes")).unwrap();
        std::os::unix::fs::symlink(
            outside.join(NODE_CONFIG_FILE),
            repo.join("nodes/peppy.json5"),
        )
        .unwrap();

        let index = generate_repository_index(&repo).expect("the walk reads through the symlink");
        write_repository_index(&repo, &index).expect("writes");

        let drifts = check_repository_index(&repo).expect("checks");
        let detail = drifts
            .iter()
            .find_map(|d| match d {
                IndexDrift::Unmatched { detail, .. } => Some(detail.clone()),
                _ => None,
            })
            .expect("the escaping entry is refused");
        assert!(detail.contains("outside the repository"), "{detail}");
    }

    /// A missing index is not the same as an empty one, and the message
    /// says which file was looked for.
    #[test]
    fn check_fails_when_the_repository_has_no_index() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("hub");
        write_node_json5(&repo.join("nodes/a"), "a", "v1");

        let err = check_repository_index(&repo).expect_err("a missing index is refused");
        assert!(err.to_string().contains("peppy_repository.json5"), "{err}");
    }
}
