//! Discovering what a repository publishes, and recording it in the
//! repository's own index.
//!
//! The walk here is the authoring tool: `peppy repo index` runs it to
//! produce a repository's `peppy_repository.json5`, which is what states the
//! identities the repository publishes and where each is declared. Reading
//! that file is what resolution uses; walking is what writes it.
//!
//! A given identity reaches [`WalkResult::items`] at most once per walk, but
//! every claimant is recorded: an identity with several claimants comes back
//! in [`WalkResult::conflicts`] so the caller refuses rather than silently
//! promoting one of two answers. The cross-repository merge, where several
//! repositories claiming one identity is a supported feature rather than a
//! conflict, happens in `refresh.rs`.

use crate::services::repo::cache::{
    ContractCacheEntry, DiscoveredEntry, LauncherCacheEntry, NodeCacheEntry, PairingCacheEntry,
    RepoCacheEntry, RepoItems,
};
use config::consts::NODE_CONFIG_FILE;
use config::fingerprint::fingerprint_for_bytes;
use config::node::NodeConfigParser;
use config::schema::PeppySchema;
use core_node_api::encoding::{RepoItemKind, RepoSourceKind};
use daemon_config::contract::PeppyContractParser;
use daemon_config::launcher::PeppyLauncherParser;
use daemon_config::pairing::PeppyPairingParser;
use daemon_config::repository::{
    DeclaredItem, ItemName, ItemTag, PeppyRepositoryIndexParser, RepoRelativePath, RepositoryIndex,
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

/// One item found in a repository: the identity it declares and where it is
/// declared, plus the fingerprint of the bytes that declared it.
///
/// This is the seam between finding items and caching them. Both the walk
/// and the index reader produce these, and [`build_cache_entries`] is the
/// single place that turns them into cache entries, so the two ways of
/// learning what a repository holds cannot drift in what they record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WalkedItem {
    pub kind: RepoItemKind,
    pub name: String,
    /// Empty for launchers, matching the identity key the caches use.
    pub tag: String,
    /// Relative to the root of the scanned tree.
    pub relative_path: String,
    /// The same file, absolute on this machine.
    pub absolute_path: PathBuf,
    pub sha256: String,
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

/// Turns found items into the four cache-entry vectors, attributing each to
/// the source it came from.
///
/// Git entries record a repository-relative path because the checkout they
/// resolve against is materialized per machine; fs entries record the
/// absolute path, which is also what `entry_belongs_to_repo` matches a
/// configured repository path against.
pub(crate) fn build_cache_entries(
    items: Vec<WalkedItem>,
    source_type: RepoSourceKind,
    source_uri: Option<&str>,
    resolved_ref: Option<&str>,
) -> RepoItems {
    let mut built = RepoItems::default();
    for item in items {
        let path = if source_type == RepoSourceKind::Git {
            item.relative_path
        } else {
            item.absolute_path.to_string_lossy().into_owned()
        };
        let discovered = DiscoveredEntry {
            name: item.name,
            tag: item.tag,
            sha256: item.sha256,
            path,
            source_type,
            source_uri: source_uri.map(str::to_owned),
            resolved_ref: resolved_ref.map(str::to_owned),
        };
        match item.kind {
            RepoItemKind::Node => built
                .nodes
                .push(NodeCacheEntry::from_discovered(discovered)),
            RepoItemKind::Launcher => built
                .launchers
                .push(LauncherCacheEntry::from_discovered(discovered)),
            RepoItemKind::Contract => built
                .contracts
                .push(ContractCacheEntry::from_discovered(discovered)),
            RepoItemKind::Pairing => built
                .pairings
                .push(PairingCacheEntry::from_discovered(discovered)),
        }
    }
    built
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

    let (name, tag) = declared_identity(item.kind, content, item.path.as_path())?;
    let expected_tag = item.tag.map(ItemTag::as_str).unwrap_or_default();
    if name != item.name.as_str() || tag != expected_tag {
        let found = format_identity(&name, &tag);
        return Err(format!("declares `{found}`"));
    }

    Ok(bytes)
}

/// The identity a file declares, read with the parser for `kind`. A file
/// that is not that kind at all is reported as such, which is what catches
/// an item filed under the wrong section.
fn declared_identity(
    kind: RepoItemKind,
    content: &str,
    path: &Path,
) -> std::result::Result<(String, String), String> {
    let not_this_kind = |e: String| format!("is not a {kind}: {e}");
    match kind {
        RepoItemKind::Node => {
            let parsed = NodeConfigParser::from_content(content)
                .map_err(|e| not_this_kind(e.to_string()))?;
            Ok((
                parsed.manifest.name.as_str().to_owned(),
                parsed.manifest.tag.clone(),
            ))
        }
        RepoItemKind::Launcher => {
            let parsed = PeppyLauncherParser::from_content(content)
                .map_err(|e| not_this_kind(e.to_string()))?;
            if parsed.peppy_schema != PeppySchema::LauncherV1 {
                return Err(not_this_kind("wrong schema tag".to_owned()));
            }
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|stem| !stem.is_empty())
                .ok_or_else(|| "has no usable file stem".to_owned())?;
            Ok((stem.to_owned(), String::new()))
        }
        RepoItemKind::Contract => {
            let parsed = PeppyContractParser::from_content(content)
                .map_err(|e| not_this_kind(e.to_string()))?;
            Ok((
                parsed.manifest.name.as_str().to_owned(),
                parsed.manifest.tag.clone(),
            ))
        }
        RepoItemKind::Pairing => {
            let parsed = PeppyPairingParser::from_content(content)
                .map_err(|e| not_this_kind(e.to_string()))?;
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
/// Each `.json5` file is read once. Files named `peppy.json5` are tried as
/// nodes first (preserving the filename-driven node convention); any
/// `.json5` whose body declares a `peppy_schema` value is dispatched to the
/// matching collector.
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
            config_path,
            bytes: &bytes,
        };
        if file_name == NODE_CONFIG_FILE {
            // Try node parse first to preserve the documented filename
            // convention for nodes. If the file's schema doesn't match,
            // fall through to the launcher/contract dispatch; that
            // way a non-node `peppy.json5` is still discoverable.
            if collect_node_item(&ctx, &mut nodes_seen, &mut items) {
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
                collect_node_item(&ctx, &mut nodes_seen, &mut items);
            }
            PeppySchema::LauncherV1 => {
                collect_launcher_item(&ctx, &mut launchers_seen, &mut items);
            }
            PeppySchema::ContractV1 => {
                collect_contract_item(&ctx, &mut contracts_seen, &mut items);
            }
            PeppySchema::PairingV1 => {
                collect_pairing_item(&ctx, &mut pairings_seen, &mut items);
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

/// Shared body of the four collectors: UTF-8 check, strict parse via
/// `identity`, claim recording, then item construction. The strict parse
/// catches structural problems (unknown fields, malformed sections) that
/// the cheap schema peek can't.
///
/// `identity` returns the document's `(name, tag)`, `Ok(None)` to skip the
/// file silently (wrong schema variant, unusable file stem), or `Err` when
/// the content does not parse as that document kind at all.
/// `parse_failure_label` words that last case: a `peppy.json5` failing the
/// node parse is usually a different document kind rather than a malformed
/// node, so the node collector logs "non-node".
///
/// Every claimant is recorded in `claims`, including the second and later
/// ones that are not pushed to `out`; the caller turns identities with
/// several claimants into [`RepoConflict`]s and refuses the whole
/// repository. Which claimant reaches `out` is walk-order dependent and
/// deliberately not relied upon.
///
/// Returns `false` only on parse failure, so the node collector can fall
/// back to schema dispatch; repeat claimants return `true` because the file
/// is a valid document of the kind.
fn collect_walked_item(
    ctx: &EntryContext<'_>,
    kind: RepoItemKind,
    parse_failure_label: &str,
    claims: &mut ClaimMap,
    out: &mut Vec<WalkedItem>,
    identity: impl FnOnce(&str) -> std::result::Result<Option<(String, String)>, String>,
) -> bool {
    let content = match std::str::from_utf8(ctx.bytes) {
        Ok(s) => s,
        Err(e) => {
            debug!(
                "Skipping non-utf8 {} .json5 at {}: {}",
                kind,
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

    let relative_path = ctx
        .config_path
        .strip_prefix(ctx.root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let claimants = claims.entry((name.clone(), tag.clone())).or_default();
    claimants.push(relative_path.clone());
    if claimants.len() > 1 {
        return true;
    }

    out.push(WalkedItem {
        kind,
        name,
        tag,
        relative_path,
        absolute_path: ctx.config_path.to_path_buf(),
        sha256: fingerprint_for_bytes(ctx.bytes),
    });
    true
}

/// Returns `true` when the file parsed cleanly as a node and its claim
/// was recorded. `false` means parsing failed; the caller can fall back
/// to a different schema dispatch.
fn collect_node_item(
    ctx: &EntryContext<'_>,
    claims: &mut ClaimMap,
    out: &mut Vec<WalkedItem>,
) -> bool {
    collect_walked_item(
        ctx,
        RepoItemKind::Node,
        "non-node",
        claims,
        out,
        |content| {
            let parsed = NodeConfigParser::from_content(content).map_err(|e| e.to_string())?;
            Ok(Some((
                parsed.manifest.name.as_str().to_string(),
                parsed.manifest.tag.clone(),
            )))
        },
    )
}

fn collect_launcher_item(ctx: &EntryContext<'_>, claims: &mut ClaimMap, out: &mut Vec<WalkedItem>) {
    collect_walked_item(
        ctx,
        RepoItemKind::Launcher,
        "malformed launcher",
        claims,
        out,
        |content| {
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
        },
    );
}

fn collect_contract_item(ctx: &EntryContext<'_>, claims: &mut ClaimMap, out: &mut Vec<WalkedItem>) {
    collect_walked_item(
        ctx,
        RepoItemKind::Contract,
        "malformed contract",
        claims,
        out,
        |content| {
            let parsed = PeppyContractParser::from_content(content).map_err(|e| e.to_string())?;
            Ok(Some((
                parsed.manifest.name.as_str().to_string(),
                parsed.manifest.tag.clone(),
            )))
        },
    );
}

fn collect_pairing_item(ctx: &EntryContext<'_>, claims: &mut ClaimMap, out: &mut Vec<WalkedItem>) {
    collect_walked_item(
        ctx,
        RepoItemKind::Pairing,
        "malformed pairing",
        claims,
        out,
        |content| {
            let parsed = PeppyPairingParser::from_content(content).map_err(|e| e.to_string())?;
            Ok(Some((
                parsed.manifest.name.as_str().to_string(),
                parsed.manifest.tag.clone(),
            )))
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(item.absolute_path.ends_with("nodes/my_sensor/peppy.json5"));
        assert!(
            !item.sha256.is_empty(),
            "the manifest bytes are fingerprinted"
        );
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
        assert_eq!(
            found(&walked, RepoItemKind::Node),
            vec!["my_sensor:v1", "my_sensor:v2"]
        );
    }

    /// A git repository's entries carry the repository-relative path,
    /// because the checkout they resolve against is materialized per
    /// machine; an fs repository's entries carry the absolute path,
    /// which is what repository attribution matches against.
    #[test]
    fn build_cache_entries_picks_the_path_form_the_source_needs() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_node_json5(&repo.join("nodes/a"), "a", "v1");
        let items = walk_directory(&repo, &[]).items;

        let git = build_cache_entries(
            items.clone(),
            RepoSourceKind::Git,
            Some("https://example.invalid/hub"),
            Some("main"),
        );
        assert_eq!(git.nodes[0].path, "nodes/a/peppy.json5");
        assert_eq!(git.nodes[0].resolved_ref.as_deref(), Some("main"));

        let fs = build_cache_entries(items, RepoSourceKind::Fs, None, None);
        assert!(Path::new(&fs.nodes[0].path).is_absolute());
        assert_eq!(fs.nodes[0].source_uri, None);
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
                (
                    item.kind.as_str(),
                    format_identity(&item.name, &item.tag),
                    item.relative_path.clone(),
                    item.sha256.clone(),
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
