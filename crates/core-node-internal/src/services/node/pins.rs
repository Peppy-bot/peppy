//! Launch-pin closure resolution: the one place a `name:tag` becomes a set
//! of pins.
//!
//! A launch coordinator (and `peppy node add <name>:<tag>`, where the
//! receiving daemon coordinates its own add) resolves a root node here: the
//! root, its transitive node dependencies through the nodes cache, and the
//! contract and pairing documents every resolved manifest names. Each
//! resolved item becomes a [`PinnedItem`], and everything downstream,
//! including every other daemon in a federated launch, consumes pins and
//! never resolves a name.
//!
//! Materialization is shared with the pinned executor in `add_batch`:
//! [`materialize_pinned_node`] reuses local content when its fingerprint
//! matches the pin and fetches the pinned commit when it does not, so the
//! machine that minted a pin and a machine that has never seen the bytes
//! run exactly the same code.

use super::cache as node_cache;
use crate::services::repo::cache::{
    self as repo_cache, ContractCacheEntry, EntryOrigin, NodeCacheEntry, PairingCacheEntry,
    RepoCacheEntry,
};
use config::node::{Manifest, NodeConfig, NodeConfigParser};
use daemon_config::consts::PeppyDirs;
use daemon_config::repository::{ItemName, ItemTag, PinKind, PinnedItem};
use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, StreamExt};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Upper bound on concurrently-running materializations inside one closure
/// resolution. Git entries materialize through clones and fetches; spawning
/// an unbounded number of them at once thrashes disk and network. 8 is
/// empirical: enough to overlap IO latency, low enough to avoid saturating
/// a developer laptop.
pub(crate) const MATERIALIZE_CONCURRENCY: usize = 8;

/// One pinned node, materialized: the pin that names it and the on-disk
/// tree the add pipeline stages it from.
pub(crate) struct MaterializedPin {
    pub pin: PinnedItem,
    pub root_dir: PathBuf,
    pub config: NodeConfig,
    /// `true` only for the root of the closure. Controls which node the
    /// original goal's env vars and force flag apply to.
    pub is_root: bool,
}

/// A root's resolved node closure: every node to add, root first, then in
/// discovery order. The contract and pairing documents their manifests name
/// are minted separately ([`doc_pins_for_manifests`]) so a launch can run
/// its graph validation, whose refusals are the more actionable ones,
/// before any document is looked up.
pub(crate) struct PinnedClosure {
    pub nodes: Vec<MaterializedPin>,
}

impl PinnedClosure {
    /// The dependency-node pins, i.e. everything except the root, for
    /// `NodeAddGoal.pins_json5` and `DeploymentPins.closure`.
    pub fn dep_pins(&self) -> Vec<PinnedItem> {
        self.nodes
            .iter()
            .filter(|node| !node.is_root)
            .map(|node| node.pin.clone())
            .collect()
    }

    /// Every manifest in the closure, root first, for doc-pin minting.
    pub fn manifests(&self) -> Vec<Manifest> {
        self.nodes
            .iter()
            .map(|node| node.config.manifest.clone())
            .collect()
    }

    pub fn root(&self) -> &MaterializedPin {
        self.nodes
            .first()
            .expect("a resolved closure always holds its root")
    }
}

/// The pin a nodes-cache entry mints: the entry already records the content
/// fingerprint and the origin, so pinning is a restatement, not a decision.
pub(crate) fn pin_for_node_entry(entry: &NodeCacheEntry) -> PinnedItem {
    PinnedItem {
        kind: PinKind::Node,
        name: entry.node_name.clone(),
        tag: entry.node_tag.clone(),
        sha256: entry.sha256.clone(),
        origin: entry.origin.clone(),
    }
}

/// Materializes one pinned node to `(root_dir, parsed config)`.
///
/// Content decides: a local cache entry whose fingerprint matches the pin is
/// used whatever repository or identity it arrived under, and a miss or a
/// drifted local copy fetches the pin's own origin. The parsed manifest must
/// declare the pinned identity; bytes that fingerprint-match but declare
/// another node mean a poisoned cache or a mis-minted pin, and both are
/// refusals rather than something to run.
pub(crate) async fn materialize_pinned_node(
    peppy_dirs: &PeppyDirs,
    entries: &[NodeCacheEntry],
    pin: &PinnedItem,
    on_feedback: node_cache::MaterializeFeedback,
) -> std::result::Result<(PathBuf, NodeConfig), String> {
    let label = pin.label();
    let may_block = pin.origin.resolution_may_block()
        || repo_cache::lookup_by_content(entries, pin)
            .map(|entry| entry.origin.resolution_may_block())
            .unwrap_or(true);

    let (manifest_path, bytes) = if may_block {
        let dirs = peppy_dirs.clone();
        let entries = entries.to_vec();
        let pin = pin.clone();
        tokio::task::spawn_blocking(move || {
            repo_cache::resolve_pin_to_bytes(&dirs, &entries, &pin, &|line| on_feedback(line))
        })
        .await
        .map_err(|e| format!("materialization task for {label} failed: {e}"))?
    } else {
        repo_cache::resolve_pin_to_bytes(peppy_dirs, entries, pin, &|line| on_feedback(line))
    }?;

    let content = std::str::from_utf8(&bytes)
        .map_err(|e| format!("pinned {label} content is not UTF-8: {e}"))?;
    let config = NodeConfigParser::from_content(content)
        .map_err(|e| format!("failed to parse pinned {label}: {e}"))?;
    if config.manifest.name.as_str() != pin.name.as_str() || config.manifest.tag != pin.tag.as_str()
    {
        return Err(format!(
            "pinned {label} resolved to bytes declaring `{}:{}`; the cache or the pin is wrong",
            config.manifest.name.as_str(),
            config.manifest.tag
        ));
    }

    // The pin names the file that declares the node; a node is built from
    // the directory holding it.
    let root_dir = manifest_path
        .parent()
        .ok_or_else(|| {
            format!(
                "pinned {label} resolved to {}, which has no parent directory",
                manifest_path.display()
            )
        })?
        .to_path_buf();
    Ok((root_dir, config))
}

type MaterializeOutput = (
    PinnedItem,
    bool,
    std::result::Result<(PathBuf, NodeConfig), String>,
);

/// Materializes a set of already-minted pins concurrently, root first in the
/// returned order. The pinned executor's front half: no name is looked up,
/// no dependency is discovered, the pin set IS the batch.
pub(crate) async fn materialize_pin_set(
    peppy_dirs: &PeppyDirs,
    entries: &[NodeCacheEntry],
    root: PinnedItem,
    dep_pins: Vec<PinnedItem>,
    on_feedback: node_cache::MaterializeFeedback,
) -> std::result::Result<Vec<MaterializedPin>, String> {
    let semaphore = Arc::new(Semaphore::new(MATERIALIZE_CONCURRENCY));
    let mut in_flight: FuturesUnordered<BoxFuture<'_, MaterializeOutput>> = FuturesUnordered::new();

    for (pin, is_root) in
        std::iter::once((root, true)).chain(dep_pins.into_iter().map(|p| (p, false)))
    {
        let permit_source = Arc::clone(&semaphore);
        let feedback = Arc::clone(&on_feedback);
        in_flight.push(Box::pin(async move {
            let _permit = permit_source
                .acquire_owned()
                .await
                .expect("materialize semaphore is never closed");
            let result = materialize_pinned_node(peppy_dirs, entries, &pin, feedback).await;
            (pin, is_root, result)
        }));
    }

    let mut nodes: Vec<MaterializedPin> = Vec::new();
    while let Some((pin, is_root, result)) = in_flight.next().await {
        let (root_dir, config) = result?;
        nodes.push(MaterializedPin {
            pin,
            root_dir,
            config,
            is_root,
        });
    }
    // Root first, so `PinnedClosure::root` and the executor's is-root
    // handling do not depend on completion order.
    nodes.sort_by_key(|node| !node.is_root);
    Ok(nodes)
}

/// Resolves `root_name:root_tag` and its transitive closure through this
/// machine's caches, minting a pin per resolved item.
///
/// The ONE place a name becomes bytes. A launch coordinator runs it for
/// every `repo:` deployment, and `peppy node add <name>:<tag>` runs it on
/// the daemon that receives the goal; everything downstream consumes the
/// pins it mints. Every missing or ambiguous identity in the closure is
/// aggregated into one report, so a user with several problems fixes them
/// all after one run.
pub(crate) async fn resolve_pinned_closure(
    peppy_dirs: &PeppyDirs,
    entries: &[NodeCacheEntry],
    root_name: &str,
    root_tag: &str,
    on_feedback: node_cache::MaterializeFeedback,
) -> std::result::Result<PinnedClosure, String> {
    let mut nodes: Vec<MaterializedPin> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut missing: Vec<(String, String)> = Vec::new();
    let mut ambiguous: Vec<String> = Vec::new();
    let mut pending: Vec<(String, String, bool)> =
        vec![(root_name.to_owned(), root_tag.to_owned(), true)];
    let mut in_flight: FuturesUnordered<BoxFuture<'_, MaterializeOutput>> = FuturesUnordered::new();
    let semaphore = Arc::new(Semaphore::new(MATERIALIZE_CONCURRENCY));

    loop {
        while let Some((name, tag, is_root)) = pending.pop() {
            let key = (name.clone(), tag.clone());
            if !seen.insert(key.clone()) {
                continue;
            }
            // An identity claimed twice is present, not absent, so it is
            // kept out of `missing`: telling the user it is missing sends
            // them to add a node that is already there twice.
            let resolved = match repo_cache::lookup(entries, &name, &tag) {
                Ok(resolved) => resolved,
                Err(ambiguity) => {
                    ambiguous.push(ambiguity.to_string());
                    continue;
                }
            };
            let Some(entry) = resolved else {
                missing.push(key);
                continue;
            };
            let pin = pin_for_node_entry(entry);
            let permit_source = Arc::clone(&semaphore);
            let feedback = Arc::clone(&on_feedback);
            in_flight.push(Box::pin(async move {
                let _permit = permit_source
                    .acquire_owned()
                    .await
                    .expect("materialize semaphore is never closed");
                let result = materialize_pinned_node(peppy_dirs, entries, &pin, feedback).await;
                (pin, is_root, result)
            }));
        }

        let Some((pin, is_root, result)) = in_flight.next().await else {
            break;
        };

        let (root_dir, config) = result?;

        if let Some(deps) = config.manifest.depends_on.as_ref() {
            for dep in &deps.nodes {
                let dep_name = dep.name.as_str().to_owned();
                let dep_tag = dep.tag.clone();
                if seen.contains(&(dep_name.clone(), dep_tag.clone())) {
                    continue;
                }
                pending.push((dep_name, dep_tag, false));
            }
        }

        nodes.push(MaterializedPin {
            pin,
            root_dir,
            config,
            is_root,
        });
    }

    let mut problems: Vec<String> = Vec::new();
    ambiguous.sort();
    problems.append(&mut ambiguous);
    if !missing.is_empty() {
        missing.sort();
        let list = missing
            .iter()
            .map(|(n, t)| format!("{}:{}", n, t))
            .collect::<Vec<_>>()
            .join(", ");
        problems.push(format!(
            "Dependencies missing from nodes cache ({}): {list}. Run `peppy repo refresh` or \
             add the missing nodes to a configured repository{}.",
            repo_cache::nodes_repo_cache_path(peppy_dirs).display(),
            repo_cache::excluded_repositories_hint(peppy_dirs)
        ));
    }
    if !problems.is_empty() {
        return Err(daemon_config::format_bulleted(&problems));
    }

    nodes.sort_by_key(|node| !node.is_root);
    Ok(PinnedClosure { nodes })
}

/// The launch pins for contract and pairing documents, keyed by identity, as
/// the pinned add consumes them.
///
/// One pin per `(kind, name, tag)`: the closure that minted these refused a
/// second content for one identity, so a lookup is never ambiguous. An
/// author `sha256` pin in a manifest is checked against the launch pin at
/// use, not consulted for resolution.
pub(crate) struct DocPins {
    by_identity: HashMap<(PinKind, String, String), PinnedItem>,
}

impl DocPins {
    /// Builds the lookup from a goal's decoded pin list, ignoring node pins.
    pub fn from_pins(pins: &[PinnedItem]) -> Self {
        let by_identity = pins
            .iter()
            .filter(|pin| pin.kind != PinKind::Node)
            .map(|pin| {
                (
                    (
                        pin.kind,
                        pin.name.as_str().to_owned(),
                        pin.tag.as_str().to_owned(),
                    ),
                    pin.clone(),
                )
            })
            .collect();
        Self { by_identity }
    }

    /// The launch pin for one doc reference, checked against the manifest's
    /// own optional sha pin.
    ///
    /// A reference absent from the pin set means the coordinator shipped an
    /// incomplete closure; an author pin disagreeing with the launch pin
    /// means the closure was minted from different bytes than this manifest.
    /// Both are refusals, never a fall back to resolving the name here.
    pub fn require(
        &self,
        kind: PinKind,
        name: &str,
        tag: &str,
        author_sha256: Option<&str>,
    ) -> std::result::Result<&PinnedItem, String> {
        let pin = self
            .by_identity
            .get(&(kind, name.to_owned(), tag.to_owned()))
            .ok_or_else(|| {
                format!(
                    "{kind} `{name}:{tag}` is not in this launch's pins; the coordinator \
                     shipped an incomplete closure"
                )
            })?;
        if let Some(author) = author_sha256
            && pin.sha256 != author
        {
            return Err(format!(
                "{kind} `{name}:{tag}` is pinned to `{author}` by the manifest but the launch \
                 pinned `{}`; the closure was minted from different bytes than this manifest",
                pin.sha256
            ));
        }
        Ok(pin)
    }
}

/// Mints doc pins for manifests, refusing one identity at two contents.
///
/// Within one closure every reference to a document must agree on its
/// bytes: the pinned add resolves references by identity, so two contents
/// under one identity would make that lookup ambiguous on another machine.
/// The refusal names both fingerprints so the author can align the sha pins.
#[derive(Default)]
struct DocPinMinter {
    by_identity: HashMap<(PinKind, String, String), PinnedItem>,
    order: Vec<(PinKind, String, String)>,
}

impl DocPinMinter {
    fn mint_for_manifest(
        &mut self,
        peppy_dirs: &PeppyDirs,
        manifest: &Manifest,
        contracts: &[ContractCacheEntry],
        pairings: &[PairingCacheEntry],
        on_feedback: &dyn Fn(&str),
    ) -> std::result::Result<(), String> {
        for entry in &manifest.implements {
            self.mint::<ContractCacheEntry>(
                peppy_dirs,
                contracts,
                PinKind::Contract,
                entry.name.as_str(),
                &entry.tag,
                entry.sha256.as_deref(),
                on_feedback,
            )?;
        }
        let Some(depends_on) = manifest.depends_on.as_ref() else {
            return Ok(());
        };
        for entry in &depends_on.contracts {
            self.mint::<ContractCacheEntry>(
                peppy_dirs,
                contracts,
                PinKind::Contract,
                entry.name.as_str(),
                &entry.tag,
                entry.sha256.as_deref(),
                on_feedback,
            )?;
        }
        for slot in &depends_on.pairings {
            self.mint::<PairingCacheEntry>(
                peppy_dirs,
                pairings,
                PinKind::Pairing,
                slot.name.as_str(),
                &slot.tag,
                slot.sha256.as_deref(),
                on_feedback,
            )?;
        }
        for slot in &depends_on.pairing_observers {
            self.mint::<PairingCacheEntry>(
                peppy_dirs,
                pairings,
                PinKind::Pairing,
                slot.name.as_str(),
                &slot.tag,
                slot.sha256.as_deref(),
                on_feedback,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)] // One reference, spelled out.
    fn mint<E: RepoCacheEntry>(
        &mut self,
        peppy_dirs: &PeppyDirs,
        entries: &[E],
        kind: PinKind,
        name: &str,
        tag: &str,
        author_sha256: Option<&str>,
        on_feedback: &dyn Fn(&str),
    ) -> std::result::Result<(), String> {
        let (entry, _bytes) = repo_cache::resolve_cached_doc_entry(
            peppy_dirs,
            entries,
            name,
            tag,
            author_sha256,
            on_feedback,
        )?;
        let pin = PinnedItem {
            kind,
            name: ItemName::parse(name).map_err(|e| format!("{kind} `{name}:{tag}`: {e}"))?,
            tag: ItemTag::parse(tag).map_err(|e| format!("{kind} `{name}:{tag}`: {e}"))?,
            sha256: entry.sha256().clone(),
            origin: entry.origin().clone(),
        };
        let key = (kind, name.to_owned(), tag.to_owned());
        match self.by_identity.get(&key) {
            None => {
                self.order.push(key.clone());
                self.by_identity.insert(key, pin);
                Ok(())
            }
            Some(existing) if existing.sha256 == pin.sha256 => Ok(()),
            Some(existing) => Err(format!(
                "{kind} `{name}:{tag}` is needed at two different contents in one closure \
                 (`{}` and `{}`); align the manifests' sha256 pins so one set of bytes serves \
                 every reference",
                existing.sha256, pin.sha256
            )),
        }
    }

    fn into_pins(self) -> Vec<PinnedItem> {
        let mut by_identity = self.by_identity;
        self.order
            .into_iter()
            .filter_map(|key| by_identity.remove(&key))
            .collect()
    }
}

/// Mints the doc pins for a set of manifests against this machine's caches:
/// one pin per contract and pairing document they reference, deduplicated,
/// refusing one identity at two different contents. Blocking (a drift check
/// may materialize a checkout); wrap in `spawn_blocking` inside Tokio.
pub(crate) fn doc_pins_for_manifests(
    peppy_dirs: &PeppyDirs,
    manifests: &[Manifest],
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<Vec<PinnedItem>, String> {
    let contracts = repo_cache::load_contract_cache(peppy_dirs)
        .map_err(|e| format!("failed to load contract cache: {e}"))?;
    let pairings = repo_cache::load_pairing_cache(peppy_dirs)
        .map_err(|e| format!("failed to load pairing cache: {e}"))?;
    let mut minted = DocPinMinter::default();
    for manifest in manifests {
        minted.mint_for_manifest(peppy_dirs, manifest, &contracts, &pairings, on_feedback)?;
    }
    Ok(minted.into_pins())
}

/// Resolves a launcher's `git:` deployment to a pinned root: clones the
/// repository at the requested ref, records the commit that ref resolved to,
/// donates the clone to the commit-keyed checkout cache, and mints the pin
/// from the manifest bytes it read.
///
/// A `git:` ref is usually a branch, which is a moving name; capturing the
/// commit here is what makes every machine in the launch read the tree this
/// machine read, rather than whatever the branch points at when each of
/// them clones.
pub(crate) async fn resolve_git_deployment(
    peppy_dirs: &PeppyDirs,
    repo_url: &str,
    repo_path: &str,
    repo_ref: Option<&str>,
    on_feedback: node_cache::MaterializeFeedback,
) -> std::result::Result<MaterializedPin, String> {
    let dirs = peppy_dirs.clone();
    let url = repo_url.to_owned();
    let path_spec = repo_path.to_owned();
    let git_ref = repo_ref.map(str::to_owned);

    tokio::task::spawn_blocking(move || {
        let sanitized = super::git_utils::sanitize_repo_path(&path_spec)?;

        on_feedback(&format!("Cloning repository {url}..."));
        let temp_dir = tempfile::Builder::new()
            .prefix("peppy-git-deploy-")
            .tempdir()
            .map_err(|e| format!("Failed to create temporary directory: {e}"))?;
        let repo = super::git_utils::clone_repo_with_deadline(&url, temp_dir.path(), None)?;
        if let Some(reference) = git_ref.as_deref() {
            super::git_utils::checkout_repo_ref(&repo, reference)
                .map_err(|e| format!("Failed to checkout git ref '{reference}': {e}"))?;
        }
        let commit = super::git_utils::head_commit(&repo)?;
        drop(repo);

        // Donate the clone, then resolve the checkout back through the same
        // cache the pinned adds use: the donation makes this the one clone
        // any machine-local consumer of the pin ever pays for.
        node_cache::git::adopt_checkout(&dirs, &url, &commit, temp_dir.keep());
        let checkout = node_cache::git::ensure_checkout_at_commit(
            &dirs,
            &url,
            git_ref.as_deref(),
            &commit,
            &|line| on_feedback(line),
        )?;

        // The pin names the file that declares the node, so a directory
        // deployment path gains the manifest filename.
        let manifest_rel = if sanitized
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json5"))
        {
            sanitized
        } else {
            sanitized.join(config::consts::NODE_CONFIG_FILE)
        };
        let manifest_rel =
            daemon_config::repository::RepoRelativePath::parse(manifest_rel.to_str().ok_or_else(
                || format!("deployment path {} is not UTF-8", manifest_rel.display()),
            )?)
            .map_err(|e| format!("deployment path does not stay inside the repository: {e}"))?;

        let manifest_path = checkout.join(manifest_rel.as_path());
        let bytes = std::fs::read(&manifest_path).map_err(|e| {
            format!(
                "no node manifest at `{}` in {url} at {commit}: {e}",
                manifest_rel.as_str()
            )
        })?;
        let content = std::str::from_utf8(&bytes)
            .map_err(|e| format!("manifest at `{}` is not UTF-8: {e}", manifest_rel.as_str()))?;
        let config = NodeConfigParser::from_content(content).map_err(|e| {
            format!(
                "Failed to parse node config at {}: {e}",
                manifest_path.display()
            )
        })?;

        let pin = PinnedItem {
            kind: PinKind::Node,
            name: ItemName::parse(config.manifest.name.as_str()).map_err(|e| {
                format!(
                    "node `{}` cannot be pinned: {e}",
                    config.manifest.name.as_str()
                )
            })?,
            tag: ItemTag::parse(&config.manifest.tag)
                .map_err(|e| format!("tag `{}` cannot be pinned: {e}", config.manifest.tag))?,
            sha256: daemon_config::repository::ManifestFingerprint::of_bytes(&bytes),
            origin: EntryOrigin::Git {
                repo_url: url,
                repo_ref: git_ref,
                commit,
                path: manifest_rel,
            },
        };
        let root_dir = manifest_path
            .parent()
            .ok_or_else(|| {
                format!(
                    "manifest at {} has no parent directory",
                    manifest_path.display()
                )
            })?
            .to_path_buf();
        Ok(MaterializedPin {
            pin,
            root_dir,
            config,
            is_root: true,
        })
    })
    .await
    .map_err(|e| format!("git deployment resolution task failed: {e}"))?
}

/// Decodes the JSON5 pins of a goal, refusing the batch on the first pin
/// that does not validate.
pub(crate) fn decode_pins(pins_json5: &[String]) -> std::result::Result<Vec<PinnedItem>, String> {
    pins_json5
        .iter()
        .enumerate()
        .map(|(index, raw)| {
            serde_json5::from_str::<PinnedItem>(raw)
                .map_err(|e| format!("pin #{index} is not decodable: {e}"))
        })
        .collect()
}

/// Serializes pins for the wire. The inverse of [`decode_pins`].
pub(crate) fn encode_pins(pins: &[PinnedItem]) -> std::result::Result<Vec<String>, String> {
    pins.iter()
        .map(|pin| {
            serde_json5::to_string(pin)
                .map_err(|e| format!("could not encode the pin for {}: {e}", pin.label()))
        })
        .collect()
}

/// A materialized origin check for pins that must be portable: a pin whose
/// origin is a tree on this machine cannot back a deployment placed on
/// another one.
pub(crate) fn portable_pin_refusal(pin: &PinnedItem) -> Option<String> {
    match &pin.origin {
        EntryOrigin::Git { .. } => None,
        EntryOrigin::Fs { path } => Some(format!(
            "{} resolves to a filesystem repository entry at {}, which cannot be placed on \
             another core node: the path names a tree on the coordinator's filesystem. Keep the \
             deployment's instances on one core node, or serve the node from a git repository.",
            pin.label(),
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use daemon_config::repository::GitCommit;
    use daemon_config::repository::RepoRelativePath;

    fn doc_pin(kind: PinKind, name: &str, sha_seed: &str) -> PinnedItem {
        PinnedItem {
            kind,
            name: ItemName::parse(name).expect("valid name"),
            tag: ItemTag::parse("v1").expect("valid tag"),
            sha256: daemon_config::repository::ManifestFingerprint::of_bytes(sha_seed.as_bytes()),
            origin: EntryOrigin::Git {
                repo_url: "https://example.com/hub".to_owned(),
                repo_ref: Some("main".to_owned()),
                commit: GitCommit::parse(&"b".repeat(40)).expect("valid commit"),
                path: RepoRelativePath::parse(&format!("{name}.json5")).expect("valid path"),
            },
        }
    }

    /// A pinned add resolves a doc reference through the launch pin: present
    /// and unconflicted resolves, absent refuses as an incomplete closure,
    /// and an author sha pin disagreeing with the launch pin refuses naming
    /// both. Nothing here consults a cache by name.
    #[test]
    fn doc_pin_lookup_resolves_refuses_gaps_and_refuses_author_conflicts() {
        let pin = doc_pin(PinKind::Contract, "frames", "the contract body");
        let pins = DocPins::from_pins(std::slice::from_ref(&pin));

        let resolved = pins
            .require(PinKind::Contract, "frames", "v1", None)
            .expect("a pinned reference resolves");
        assert_eq!(resolved.sha256, pin.sha256);

        let matching_author = pin.sha256.as_str().to_owned();
        assert!(
            pins.require(PinKind::Contract, "frames", "v1", Some(&matching_author))
                .is_ok(),
            "an author pin agreeing with the launch pin resolves"
        );

        let err = pins
            .require(PinKind::Pairing, "frames", "v1", None)
            .expect_err("the wrong kind is not the same identity");
        assert!(err.contains("incomplete closure"), "{err}");

        let err = pins
            .require(PinKind::Contract, "absent", "v1", None)
            .expect_err("a missing reference is a gap");
        assert!(err.contains("incomplete closure"), "{err}");

        let other = "f".repeat(64);
        let err = pins
            .require(PinKind::Contract, "frames", "v1", Some(&other))
            .expect_err("a disagreeing author pin must refuse");
        assert!(err.contains(&other), "names the author pin: {err}");
        assert!(
            err.contains(pin.sha256.as_str()),
            "names the launch pin: {err}"
        );
    }

    /// Node pins never enter the doc lookup: a contract that happens to
    /// share a node's name must not resolve through the node's pin.
    #[test]
    fn node_pins_stay_out_of_the_doc_lookup() {
        let pins = DocPins::from_pins(&[doc_pin(PinKind::Node, "frames", "a node body")]);
        assert!(
            pins.require(PinKind::Contract, "frames", "v1", None)
                .is_err(),
            "a node pin must not serve a contract reference"
        );
    }
}
