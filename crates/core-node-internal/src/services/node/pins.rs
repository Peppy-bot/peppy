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
use crate::services::node::Report;
use crate::services::repo::cache::{
    self as repo_cache, ContractCacheEntry, EntryOrigin, NodeCacheEntry, PairingCacheEntry,
    PinnableCacheEntry, RepoCacheEntry,
};
use config::node::{DocumentKind, DocumentReference, Manifest, NodeConfig, NodeConfigParser};
use daemon_config::consts::PeppyDirs;
use daemon_config::repository::{ItemName, ItemTag, PinKind, PinnedDocument, PinnedItem};
use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, StreamExt};
use std::collections::HashSet;
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
/// are minted separately ([`doc_pins_for_manifest_sets`]) so a launch can run
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
    entries: &Arc<Vec<NodeCacheEntry>>,
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
        // The index is SHARED with the blocking task, not copied into it:
        // up to `MATERIALIZE_CONCURRENCY` of these run at once and the index
        // holds every node the machine's repositories know about.
        let entries = Arc::clone(entries);
        let pin = pin.clone();
        tokio::task::spawn_blocking(move || {
            repo_cache::resolve_pin_to_bytes(&dirs, &entries, &pin, &|report| on_feedback(report))
        })
        .await
        .map_err(|e| format!("materialization task for {label} failed: {e}"))?
    } else {
        repo_cache::resolve_pin_to_bytes(peppy_dirs, entries, pin, &|report| on_feedback(report))
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

/// One bounded materialization, ready to push onto a `FuturesUnordered`.
///
/// Both closure walkers below go through this, so the concurrency policy (how
/// a permit is taken and what a completed materialization reports back) is
/// stated once rather than restated per walker.
fn spawn_materialize<'a>(
    semaphore: &Arc<Semaphore>,
    peppy_dirs: &'a PeppyDirs,
    entries: &'a Arc<Vec<NodeCacheEntry>>,
    pin: PinnedItem,
    is_root: bool,
    on_feedback: &node_cache::MaterializeFeedback,
) -> BoxFuture<'a, MaterializeOutput> {
    let permit_source = Arc::clone(semaphore);
    let feedback = Arc::clone(on_feedback);
    Box::pin(async move {
        let _permit = permit_source
            .acquire_owned()
            .await
            .expect("materialize semaphore is never closed");
        let result = materialize_pinned_node(peppy_dirs, entries, &pin, feedback).await;
        (pin, is_root, result)
    })
}

/// Materializes a set of already-minted pins concurrently, root first in the
/// returned order. The pinned executor's front half: no name is looked up,
/// no dependency is discovered, the pin set IS the batch.
pub(crate) async fn materialize_pin_set(
    peppy_dirs: &PeppyDirs,
    entries: &Arc<Vec<NodeCacheEntry>>,
    root: PinnedItem,
    dep_pins: Vec<PinnedItem>,
    on_feedback: node_cache::MaterializeFeedback,
) -> std::result::Result<Vec<MaterializedPin>, String> {
    let semaphore = Arc::new(Semaphore::new(MATERIALIZE_CONCURRENCY));
    let mut in_flight: FuturesUnordered<BoxFuture<'_, MaterializeOutput>> = FuturesUnordered::new();

    for (pin, is_root) in
        std::iter::once((root, true)).chain(dep_pins.into_iter().map(|p| (p, false)))
    {
        in_flight.push(spawn_materialize(
            &semaphore,
            peppy_dirs,
            entries,
            pin,
            is_root,
            &on_feedback,
        ));
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
    // Root first, so the executor finds the root at the head of the list
    // (add_batch reads `nodes.first()`) whatever the completion order.
    nodes.sort_by_key(|node| !node.is_root);
    Ok(nodes)
}

/// Resolves `root_name:root_tag` and its transitive closure through this
/// machine's caches, minting a pin per resolved item.
///
/// The ONE place a name becomes bytes. A launch coordinator runs it for
/// every deployment, and `peppy node add <name>:<tag>` runs it on the
/// daemon that receives the goal; everything downstream consumes the
/// pins it mints. Every missing or ambiguous identity in the closure is
/// aggregated into one report, so a user with several problems fixes them
/// all after one run.
pub(crate) async fn resolve_pinned_closure(
    peppy_dirs: &PeppyDirs,
    entries: &Arc<Vec<NodeCacheEntry>>,
    root_name: &str,
    root_tag: &str,
    on_feedback: node_cache::MaterializeFeedback,
) -> std::result::Result<PinnedClosure, peppylib::messaging::Failure> {
    use config::failure_kind::PeppyFailureKind;
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
            let pin = entry.pin();
            in_flight.push(spawn_materialize(
                &semaphore,
                peppy_dirs,
                entries,
                pin,
                is_root,
                &on_feedback,
            ));
        }

        let Some((pin, is_root, result)) = in_flight.next().await else {
            break;
        };

        let (root_dir, config) = result.map_err(|reason| {
            peppylib::messaging::Failure::new(PeppyFailureKind::AttemptFailed, reason)
        })?;

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
    // A name the caches do not hold, or hold twice, is a goal this machine
    // cannot resolve.
    if !problems.is_empty() {
        return Err(peppylib::messaging::Failure::new(
            PeppyFailureKind::Rejected,
            daemon_config::format_bulleted(&problems),
        ));
    }

    nodes.sort_by_key(|node| !node.is_root);
    Ok(PinnedClosure { nodes })
}

/// The contract and pairing documents that one manifest names, each in hand
/// beside its pin: the bytes an add generates the node's code from, and that
/// the entity of the node keeps for `instance_documents`.
///
/// One document per `(kind, name, tag)`: the pins they are read by give one
/// content for each identity (see [`DocPinMinter`]), so a lookup is never
/// ambiguous. An author `sha256` pin in a manifest is checked against the
/// pin, not consulted for resolution.
pub(crate) struct ManifestDocuments {
    documents: Vec<PinnedDocument>,
}

impl ManifestDocuments {
    /// Reads the document of each reference of `manifest` by its pin in
    /// `pins`, each document once, in manifest order. Node pins and the pins
    /// of documents the manifest does not name are left out.
    ///
    /// A reference absent from `pins` means the coordinator shipped an
    /// incomplete closure; an author pin disagreeing with its pin means the
    /// closure was minted from different bytes than this manifest. Both are
    /// refusals, never a fall back to resolving the name here. A document
    /// cache is loaded only when a pin of its kind needs it.
    ///
    /// Blocking: a pin whose local copy is gone or drifted is fetched from
    /// its origin. Use [`Self::read_async`] inside Tokio.
    pub fn read(
        peppy_dirs: &PeppyDirs,
        pins: &[PinnedItem],
        manifest: &Manifest,
        on_feedback: &dyn Fn(Report<'_>),
    ) -> std::result::Result<Self, String> {
        let mut selected: Vec<(DocumentKind, &PinnedItem)> = Vec::new();
        for reference in manifest.document_references() {
            let pin = find_pinned(pins, |pin| pin, reference)?;
            if !selected.iter().any(|(_, chosen)| *chosen == pin) {
                selected.push((reference.kind, pin));
            }
        }
        let needs = |kind: DocumentKind| selected.iter().any(|(chosen, _)| *chosen == kind);
        let contracts: Vec<ContractCacheEntry> =
            load_doc_cache_if_needed(peppy_dirs, needs(DocumentKind::Contract))?;
        let pairings: Vec<PairingCacheEntry> =
            load_doc_cache_if_needed(peppy_dirs, needs(DocumentKind::Pairing))?;
        let documents = selected
            .into_iter()
            .map(|(kind, pin)| match kind {
                DocumentKind::Contract => repo_cache::materialize_pinned_document(
                    peppy_dirs,
                    &contracts,
                    pin,
                    on_feedback,
                ),
                DocumentKind::Pairing => {
                    repo_cache::materialize_pinned_document(peppy_dirs, &pairings, pin, on_feedback)
                }
            })
            .collect::<std::result::Result<_, _>>()?;
        Ok(Self { documents })
    }

    /// [`Self::read`] on a blocking thread, with the feedback sink every
    /// async caller already holds.
    pub async fn read_async(
        peppy_dirs: &PeppyDirs,
        pins: Vec<PinnedItem>,
        manifest: Manifest,
        on_feedback: node_cache::MaterializeFeedback,
    ) -> std::result::Result<Self, String> {
        let dirs = peppy_dirs.clone();
        tokio::task::spawn_blocking(move || {
            Self::read(&dirs, &pins, &manifest, &|report| on_feedback(report))
        })
        .await
        .map_err(|e| format!("document read task failed: {e}"))?
    }

    /// The document of one reference of the manifest, parsed by `parse`.
    pub fn parse<T>(
        &self,
        reference: DocumentReference<'_>,
        parse: impl FnOnce(&str) -> std::result::Result<T, String>,
    ) -> std::result::Result<T, String> {
        let document = find_pinned(&self.documents, |document| &document.pin, reference)?;
        parse(&document.content)
            .map_err(|e| format!("failed to parse pinned {}: {e}", document.pin.label()))
    }

    /// The documents, each once, in manifest order.
    pub fn into_documents(self) -> Vec<PinnedDocument> {
        self.documents
    }
}

/// The item of `items` whose pin names the document of `reference`, checked
/// against the author's optional sha256 pin of the reference.
fn find_pinned<'a, T>(
    items: &'a [T],
    pin_of: impl Fn(&T) -> &PinnedItem,
    reference: DocumentReference<'_>,
) -> std::result::Result<&'a T, String> {
    let DocumentReference {
        kind,
        name,
        tag,
        sha256: author_sha256,
    } = reference;
    let kind = PinKind::from(kind);
    let item = items
        .iter()
        .find(|item| {
            let pin = pin_of(item);
            pin.kind == kind && pin.name.as_str() == name && pin.tag.as_str() == tag
        })
        .ok_or_else(|| {
            format!(
                "{kind} `{name}:{tag}` is not in this launch's pins; the coordinator \
                 shipped an incomplete closure"
            )
        })?;
    let pin = pin_of(item);
    if let Some(author) = author_sha256
        && pin.sha256 != author
    {
        return Err(format!(
            "{kind} `{name}:{tag}` is pinned to `{author}` by the manifest but the launch \
             pinned `{}`; the closure was minted from different bytes than this manifest",
            pin.sha256
        ));
    }
    Ok(item)
}

/// Mints doc pins for manifests, refusing one identity at two contents.
///
/// Within one closure every reference to a document must agree on its
/// bytes: an add reads each document of its manifest once, by identity, and
/// the members of a contract have one wire key for each name and tag, so
/// two contents under one identity have no single meaning. The refusal
/// names both fingerprints so the author can align the sha pins.
#[derive(Default)]
struct DocPinMinter {
    /// Minted pins in first-reference order. A closure references tens of
    /// documents at most, so the dedup scan is linear over a tiny set and
    /// the order needs no second container to preserve it.
    pins: Vec<PinnedItem>,
}

/// The document cache of `E` when `needed`, and no entry otherwise: a cache
/// that does not parse fails only the reads that need one of its documents.
fn load_doc_cache_if_needed<E: RepoCacheEntry>(
    peppy_dirs: &PeppyDirs,
    needed: bool,
) -> std::result::Result<Vec<E>, String> {
    if !needed {
        return Ok(Vec::new());
    }
    repo_cache::load_repo_cache(peppy_dirs)
        .map_err(|e| format!("failed to load {} cache: {e}", E::KIND))
}

impl DocPinMinter {
    fn mint_for_manifest(
        &mut self,
        peppy_dirs: &PeppyDirs,
        manifest: &Manifest,
        contracts: &[ContractCacheEntry],
        pairings: &[PairingCacheEntry],
        on_feedback: &dyn Fn(Report<'_>),
    ) -> std::result::Result<(), String> {
        for reference in manifest.document_references() {
            match reference.kind {
                DocumentKind::Contract => {
                    self.mint(peppy_dirs, contracts, reference, on_feedback)?
                }
                DocumentKind::Pairing => self.mint(peppy_dirs, pairings, reference, on_feedback)?,
            }
        }
        Ok(())
    }

    fn mint<E: RepoCacheEntry>(
        &mut self,
        peppy_dirs: &PeppyDirs,
        entries: &[E],
        reference: DocumentReference<'_>,
        on_feedback: &dyn Fn(Report<'_>),
    ) -> std::result::Result<(), String> {
        let DocumentReference {
            kind,
            name,
            tag,
            sha256: author_sha256,
        } = reference;
        let kind = PinKind::from(kind);
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
        let existing = self
            .pins
            .iter()
            .find(|p| p.kind == kind && p.name.as_str() == name && p.tag.as_str() == tag)
            .map(|p| p.sha256.clone());
        match existing {
            None => {
                self.pins.push(pin);
                Ok(())
            }
            Some(existing) if existing == pin.sha256 => Ok(()),
            Some(existing) => Err(format!(
                "{kind} `{name}:{tag}` resolves to two contents, `{existing}` and `{}`; every \
                 reference to one name and tag of a document must name the same content, so \
                 give each reference the same sha256",
                pin.sha256
            )),
        }
    }
}

/// Mints the doc pins of several manifest sets against ONE load of this
/// machine's document caches: one pin per contract and pairing document each
/// set references, deduplicated within the set, refusing one identity at two
/// different contents.
///
/// Batched over sets rather than called per set because the caller is a
/// launch minting for every deployment, and both caches are read from disk
/// and parsed on each load. Each set keeps its own result so a deployment
/// whose documents are missing does not suppress the report for the others.
/// A cache is loaded only when a manifest names one of its documents.
///
/// Blocking (a drift check may materialize a checkout); use
/// [`doc_pins_for_manifest_sets_async`] inside Tokio.
pub(crate) fn doc_pins_for_manifest_sets(
    peppy_dirs: &PeppyDirs,
    sets: &[Vec<Manifest>],
    on_feedback: &dyn Fn(Report<'_>),
) -> std::result::Result<Vec<std::result::Result<Vec<PinnedItem>, String>>, String> {
    let named = |kind: DocumentKind| {
        sets.iter().flatten().any(|manifest| {
            manifest
                .document_references()
                .any(|reference| reference.kind == kind)
        })
    };
    let contracts = load_doc_cache_if_needed(peppy_dirs, named(DocumentKind::Contract))?;
    let pairings = load_doc_cache_if_needed(peppy_dirs, named(DocumentKind::Pairing))?;
    Ok(sets
        .iter()
        .map(|manifests| {
            let mut minted = DocPinMinter::default();
            for manifest in manifests {
                minted.mint_for_manifest(
                    peppy_dirs,
                    manifest,
                    &contracts,
                    &pairings,
                    on_feedback,
                )?;
            }
            Ok(minted.pins)
        })
        .collect())
}

/// [`doc_pins_for_manifest_sets`] on a blocking thread, with the feedback
/// sink every async caller already holds. Every minting site goes through
/// this, so the join-error wording has one owner.
pub(crate) async fn doc_pins_for_manifest_sets_async(
    peppy_dirs: &PeppyDirs,
    sets: Vec<Vec<Manifest>>,
    on_feedback: node_cache::MaterializeFeedback,
) -> std::result::Result<Vec<std::result::Result<Vec<PinnedItem>, String>>, String> {
    let dirs = peppy_dirs.clone();
    tokio::task::spawn_blocking(move || {
        doc_pins_for_manifest_sets(&dirs, &sets, &|report| on_feedback(report))
    })
    .await
    .map_err(|e| format!("doc pin minting task failed: {e}"))?
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
                read_ref: Some(core_node_api::encoding::ReadRef::Named("main".to_owned())),
                commit: GitCommit::parse(&"b".repeat(40)).expect("valid commit"),
                path: RepoRelativePath::parse(&format!("{name}.json5")).expect("valid path"),
            },
        }
    }

    /// A reference of `kind` to `name:v1`, with the author's `sha256`.
    fn reference<'a>(
        kind: DocumentKind,
        name: &'a str,
        sha256: Option<&'a str>,
    ) -> DocumentReference<'a> {
        DocumentReference {
            kind,
            name,
            tag: "v1",
            sha256,
        }
    }

    /// A pinned add finds the pin of a doc reference among the launch pins:
    /// present and unconflicted resolves, absent refuses as an incomplete
    /// closure, and an author sha pin disagreeing with the launch pin
    /// refuses naming both. Nothing here consults a cache by name.
    #[test]
    fn doc_pin_lookup_resolves_refuses_gaps_and_refuses_author_conflicts() {
        let pin = doc_pin(PinKind::Contract, "frames", "the contract body");
        let pins = std::slice::from_ref(&pin);
        let find = |reference| find_pinned(pins, |pin| pin, reference);

        let resolved = find(reference(DocumentKind::Contract, "frames", None))
            .expect("a pinned reference resolves");
        assert_eq!(resolved.sha256, pin.sha256);

        let matching_author = pin.sha256.as_str().to_owned();
        assert!(
            find(reference(
                DocumentKind::Contract,
                "frames",
                Some(&matching_author)
            ))
            .is_ok(),
            "an author pin agreeing with the launch pin resolves"
        );

        let err = find(reference(DocumentKind::Pairing, "frames", None))
            .expect_err("the wrong kind is not the same identity");
        assert!(err.contains("incomplete closure"), "{err}");

        let err = find(reference(DocumentKind::Contract, "absent", None))
            .expect_err("a missing reference is a gap");
        assert!(err.contains("incomplete closure"), "{err}");

        let other = "f".repeat(64);
        let err = find(reference(DocumentKind::Contract, "frames", Some(&other)))
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
        let pins = [doc_pin(PinKind::Node, "frames", "a node body")];
        assert!(
            find_pinned(
                &pins,
                |pin| pin,
                reference(DocumentKind::Contract, "frames", None)
            )
            .is_err(),
            "a node pin must not serve a contract reference"
        );
    }

    /// Writes `content` as the document `name:v1` of `kind` in `dir`, and
    /// gives the pin of a local repository entry for it.
    fn local_document(
        dir: &std::path::Path,
        kind: PinKind,
        name: &str,
        content: &str,
    ) -> PinnedItem {
        let path = dir.join(format!("{name}.json5"));
        std::fs::write(&path, content).expect("write the document");
        PinnedItem {
            kind,
            name: ItemName::parse(name).expect("valid name"),
            tag: ItemTag::parse("v1").expect("valid tag"),
            sha256: daemon_config::repository::ManifestFingerprint::of_bytes(content.as_bytes()),
            origin: EntryOrigin::Fs { path },
        }
    }

    /// The documents of a manifest are those it names: each once, in
    /// manifest order, with the bytes of its pin, and none of the other
    /// pins a launch closure carries.
    #[test]
    fn the_documents_of_a_manifest_are_read_once_each_in_manifest_order() {
        let dir = tempfile::tempdir().expect("temp dir");
        let peppy_dirs = PeppyDirs::new(dir.path());
        let manifest: Manifest = serde_json5::from_str(
            r#"{
                name: "arm",
                tag: "v1",
                description: ["A test node."],
                implements: [{ name: "limb_motion", tag: "v1", link_id: "arm" }],
                depends_on: {
                    contracts: [
                        { name: "camera", tag: "v1", link_id: "eyes" },
                        { name: "limb_motion", tag: "v1", link_id: "other_arm" },
                    ],
                    pairings: [{ name: "arm_link", tag: "v1", role: "arm", link_id: "controller" }],
                    pairing_observers: [{ name: "watch", tag: "v1", role: "arm", link_id: "watched" }],
                },
            }"#,
        )
        .expect("the manifest parses");
        let document = |kind, name: &str| {
            let content = format!(r#"{{ manifest: {{ name: "{name}", tag: "v1" }} }}"#);
            let pin = local_document(dir.path(), kind, name, &content);
            PinnedDocument::new(pin, content)
        };
        let limb_motion = document(PinKind::Contract, "limb_motion");
        let camera = document(PinKind::Contract, "camera");
        let arm_link = document(PinKind::Pairing, "arm_link");
        let watch = document(PinKind::Pairing, "watch");
        let pins = [
            doc_pin(PinKind::Contract, "gripper", "gripper body"),
            doc_pin(PinKind::Node, "arm", "arm body"),
            watch.pin.clone(),
            camera.pin.clone(),
            arm_link.pin.clone(),
            limb_motion.pin.clone(),
        ];

        let documents = ManifestDocuments::read(&peppy_dirs, &pins, &manifest, &|_| {})
            .expect("every document is pinned and readable");

        assert_eq!(
            documents.into_documents(),
            [limb_motion, camera, arm_link, watch]
        );
    }

    /// A manifest that names a document the pins do not hold has no
    /// documents to read: the lookup refuses before any read.
    #[test]
    fn the_documents_of_a_manifest_refuse_a_document_with_no_pin() {
        let dir = tempfile::tempdir().expect("temp dir");
        let manifest: Manifest = serde_json5::from_str(
            r#"{ name: "arm", tag: "v1", description: ["A test node."], implements: [{ name: "limb_motion", tag: "v1", link_id: "arm" }] }"#,
        )
        .expect("the manifest parses");
        let Err(err) =
            ManifestDocuments::read(&PeppyDirs::new(dir.path()), &[], &manifest, &|_| {})
        else {
            panic!("a document with no pin refuses");
        };
        assert!(err.contains("limb_motion:v1"), "{err}");
    }

    /// The read of the documents loads a document cache only when a pin
    /// needs it: a contract cache that does not parse fails no read of a
    /// manifest whose documents are pairings, and fails the read of one
    /// that names a contract.
    #[test]
    fn reading_the_documents_loads_only_the_caches_their_pins_need() {
        let dir = tempfile::tempdir().expect("temp dir");
        let peppy_dirs = PeppyDirs::new(dir.path());
        let contract_cache = repo_cache::contracts_repo_cache_path(&peppy_dirs);
        std::fs::create_dir_all(contract_cache.parent().expect("a cache dir"))
            .expect("create the cache dir");
        std::fs::write(&contract_cache, "not a cache {").expect("write the contract cache");
        let content = r#"{ manifest: { name: "arm_link", tag: "v1" } }"#;
        let pairing = local_document(dir.path(), PinKind::Pairing, "arm_link", content);
        let contract = PinnedItem {
            kind: PinKind::Contract,
            ..pairing.clone()
        };
        let manifest = |json5: &str| -> Manifest {
            serde_json5::from_str(json5).expect("the manifest parses")
        };
        let pairing_alone = manifest(
            r#"{ name: "arm", tag: "v1", description: ["A test node."], depends_on: { pairings: [{ name: "arm_link", tag: "v1", role: "arm", link_id: "controller" }] } }"#,
        );
        let a_contract = manifest(
            r#"{ name: "arm", tag: "v1", description: ["A test node."], implements: [{ name: "arm_link", tag: "v1", link_id: "arm" }] }"#,
        );

        let documents = ManifestDocuments::read(
            &peppy_dirs,
            std::slice::from_ref(&pairing),
            &pairing_alone,
            &|_| {},
        )
        .expect("the pairing reads with no contract cache");
        assert_eq!(
            documents.into_documents(),
            [PinnedDocument::new(pairing, content.to_owned())]
        );

        let Err(error) = ManifestDocuments::read(&peppy_dirs, &[contract], &a_contract, &|_| {})
        else {
            panic!("a contract pin needs the contract cache");
        };
        assert!(error.contains("failed to load contract cache"), "{error}");
    }

    /// Minting loads a document cache only when a manifest names one of its
    /// documents: a contract cache that does not parse fails neither a
    /// manifest that names no document nor one that names a pairing alone,
    /// and fails the minting of a manifest that names a contract.
    #[test]
    fn minting_loads_only_the_caches_of_the_documents_the_manifests_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        let peppy_dirs = PeppyDirs::new(dir.path());
        let contract_cache = repo_cache::contracts_repo_cache_path(&peppy_dirs);
        std::fs::create_dir_all(contract_cache.parent().expect("a cache dir"))
            .expect("create the cache dir");
        std::fs::write(&contract_cache, "not a cache {").expect("write the contract cache");
        let manifest = |json5: &str| -> Manifest {
            serde_json5::from_str(json5).expect("the manifest parses")
        };
        let no_document = manifest(r#"{ name: "bare", tag: "v1", description: ["A test node."] }"#);
        let pairing_alone = manifest(
            r#"{ name: "arm", tag: "v1", description: ["A test node."], depends_on: { pairings: [{ name: "arm_link", tag: "v1", role: "arm", link_id: "controller" }] } }"#,
        );
        let a_contract = manifest(
            r#"{ name: "arm", tag: "v1", description: ["A test node."], implements: [{ name: "limb_motion", tag: "v1", link_id: "arm" }] }"#,
        );

        let minted = doc_pins_for_manifest_sets(
            &peppy_dirs,
            &[vec![no_document], vec![pairing_alone]],
            &|_| {},
        )
        .expect("no manifest names a contract, so the contract cache is not loaded");
        assert_eq!(minted[0], Ok(Vec::new()));
        let error = minted[1].clone().expect_err("no cache holds the pairing");
        assert!(error.contains("arm_link"), "{error}");
        assert!(!error.contains("contract cache"), "{error}");

        let Err(error) = doc_pins_for_manifest_sets(&peppy_dirs, &[vec![a_contract]], &|_| {})
        else {
            panic!("the contract cache does not parse");
        };
        assert!(error.contains("failed to load contract cache"), "{error}");
    }
}
