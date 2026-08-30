//! Who uses a contract or pairing, read from this machine's caches.
//!
//! Every hit comes from the links `repo refresh` recorded on each node entry
//! ([`DeclaredLinks`]), so a search never reads a manifest or materializes a
//! checkout: it answers for what the last refresh saw, which is also what a
//! launch resolves. Nodes are attributed to repositories, and shadowed,
//! exactly as `repo list` shows them, through [`nodes_by_repository`].
//!
//! A claim whose tag breaks the `ItemTag` rules is never found: the query is
//! validated before the search, and such a claim is equally unresolvable
//! through the caches at sync, so nothing a launch could use is hidden.

use crate::services::repo::cache::{
    DeclaredLinks, NodeCacheEntry, RepoCacheEntry, excluded_repositories_hint, load_contract_cache,
    load_node_cache, load_pairing_cache, lookup_repo_entry, lookup_repo_entry_by_sha256,
};
use crate::services::repo::{
    AttributedNode, ListedRepo, RepoNodes, listed_repositories, nodes_by_repository,
};
use config::node::Cardinality;
use core_node_api::encoding::RepoSourceKind;
use daemon_config::consts::PeppyDirs;
use daemon_config::repository::ManifestFingerprint;

/// The node a hit belongs to, as `repo list` would show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedNode {
    pub node_name: String,
    pub node_tag: String,
    pub repo_id: u32,
    pub repo_label: String,
    pub source_type: RepoSourceKind,
    pub path: String,
    /// The label of the lower-id repository that provides the same
    /// `name:tag`: a launch resolves to that node, which may not make this
    /// claim at all.
    pub shadowed_by: Option<String>,
}

/// What a claim's `sha256` pin does at sync time, decided from the cache
/// alone by the rule `resolve_cached_doc_entry` applies: no pin resolves to
/// the published document, a pin resolves to the cached copy carrying
/// exactly that fingerprint or fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinStatus {
    /// No pin: the claim resolves to the published document.
    Unpinned,
    /// The pin equals the published document's fingerprint.
    Current,
    /// The pin names another cached copy of the identity; a sync resolves
    /// through that copy rather than the published one.
    Resolvable {
        repo_id: u32,
        repo_label: String,
        path: String,
    },
    /// No cached copy carries the pin; a sync fails with "not in cache".
    Unresolvable,
    /// The pin is not a fingerprint; a sync refuses it by name.
    Unusable { reason: String },
}

/// A `manifest.implements` claim on the searched identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Implementer {
    pub node: IndexedNode,
    pub link_id: String,
    pub sha256: Option<String>,
    pub pin: PinStatus,
}

/// A `depends_on.contracts` slot on the searched identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Consumer {
    pub node: IndexedNode,
    pub link_id: String,
    pub cardinality: Cardinality,
    pub sha256: Option<String>,
    pub pin: PinStatus,
}

/// A `depends_on.pairings` slot on the searched identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Participant {
    pub node: IndexedNode,
    pub role: String,
    pub link_id: String,
    pub optional: bool,
    pub sha256: Option<String>,
    pub pin: PinStatus,
}

/// A `depends_on.pairing_observers` slot on the searched identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observer {
    pub node: IndexedNode,
    pub role: String,
    pub link_id: String,
    pub cardinality: Cardinality,
    pub sha256: Option<String>,
    pub pin: PinStatus,
}

/// The document a plain `name:tag` reference resolves to: the lowest-id
/// repository's copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedDoc {
    pub repo_id: u32,
    /// The repository's label, or "no configured repository" for a cached
    /// copy whose repository is no longer listed.
    pub repo_label: String,
    pub path: String,
    pub sha256: ManifestFingerprint,
}

/// Everything a search says about one identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchReport {
    /// The published contract of that name, from the contracts cache.
    pub contract: Option<PublishedDoc>,
    /// The published pairing of that name, from the pairings cache. The two
    /// namespaces are separate, so both are reported.
    pub pairing: Option<PublishedDoc>,
    pub implementers: Vec<Implementer>,
    pub consumers: Vec<Consumer>,
    pub participants: Vec<Participant>,
    pub observers: Vec<Observer>,
    /// [`excluded_repositories_hint`], so an empty result can say that an
    /// excluded repository may have provided a user without the caller
    /// knowing the cache layout. Empty when nothing is excluded.
    pub excluded_hint: String,
}

/// Searches this machine's caches for the nodes that implement, consume,
/// participate in, or observe `name:tag`, with each claim's pin checked
/// against the cached documents. Each section is ordered by repository id,
/// then node name, tag and slot.
///
/// An unreadable cache is an error, as is an identity one repository
/// publishes twice: neither has an answer a launch would accept.
pub fn search_identity(
    peppy_dirs: &PeppyDirs,
    name: &str,
    tag: &str,
) -> Result<SearchReport, String> {
    let cached = load_node_cache(peppy_dirs).map_err(|e| e.to_string())?;
    let repos = listed_repositories(peppy_dirs).map_err(|e| e.to_string())?;
    let contracts = load_contract_cache(peppy_dirs).map_err(|e| e.to_string())?;
    let pairings = load_pairing_cache(peppy_dirs).map_err(|e| e.to_string())?;
    let contract = published_doc(&contracts, &repos, name, tag)?;
    let pairing = published_doc(&pairings, &repos, name, tag)?;

    let contract_pin =
        |sha256: Option<&str>| pin_status(&contracts, contract.as_ref(), &repos, name, tag, sha256);
    let pairing_pin =
        |sha256: Option<&str>| pin_status(&pairings, pairing.as_ref(), &repos, name, tag, sha256);
    let about = |claim_name: &config::runtime::Name, claim_tag: &str| {
        claim_name.as_str() == name && claim_tag == tag
    };

    let mut implementers = Vec::new();
    let mut consumers = Vec::new();
    let mut participants = Vec::new();
    let mut observers = Vec::new();
    for RepoNodes { repo, nodes } in nodes_by_repository(&repos, &cached) {
        for node in &nodes {
            let DeclaredLinks {
                implements,
                contracts,
                pairings,
                pairing_observers,
            } = &node.entry.links;
            for claim in implements.iter().filter(|c| about(&c.name, &c.tag)) {
                implementers.push(Implementer {
                    node: indexed(repo, node),
                    link_id: claim.link_id.clone(),
                    sha256: claim.sha256.clone(),
                    pin: contract_pin(claim.sha256.as_deref()),
                });
            }
            for slot in contracts.iter().filter(|s| about(&s.name, &s.tag)) {
                consumers.push(Consumer {
                    node: indexed(repo, node),
                    link_id: slot.link_id.clone(),
                    cardinality: slot.cardinality,
                    sha256: slot.sha256.clone(),
                    pin: contract_pin(slot.sha256.as_deref()),
                });
            }
            for slot in pairings.iter().filter(|s| about(&s.name, &s.tag)) {
                participants.push(Participant {
                    node: indexed(repo, node),
                    role: slot.role.clone(),
                    link_id: slot.link_id.clone(),
                    optional: slot.optional,
                    sha256: slot.sha256.clone(),
                    pin: pairing_pin(slot.sha256.as_deref()),
                });
            }
            for slot in pairing_observers.iter().filter(|s| about(&s.name, &s.tag)) {
                observers.push(Observer {
                    node: indexed(repo, node),
                    role: slot.role.clone(),
                    link_id: slot.link_id.clone(),
                    cardinality: slot.cardinality,
                    sha256: slot.sha256.clone(),
                    pin: pairing_pin(slot.sha256.as_deref()),
                });
            }
        }
    }
    implementers.sort_by_key(|hit| hit_order(&hit.node, &hit.link_id));
    consumers.sort_by_key(|hit| hit_order(&hit.node, &hit.link_id));
    participants.sort_by_key(|hit| hit_order(&hit.node, &hit.link_id));
    observers.sort_by_key(|hit| hit_order(&hit.node, &hit.link_id));

    Ok(SearchReport {
        contract,
        pairing,
        implementers,
        consumers,
        participants,
        observers,
        excluded_hint: excluded_repositories_hint(peppy_dirs),
    })
}

/// The order hits are reported in: repository priority first, then the
/// node, then the slot, so the output reads top-down as a launch resolves.
fn hit_order(node: &IndexedNode, link_id: &str) -> (u32, String, String, String) {
    (
        node.repo_id,
        node.node_name.clone(),
        node.node_tag.clone(),
        link_id.to_owned(),
    )
}

fn indexed(repo: &ListedRepo, node: &AttributedNode<'_>) -> IndexedNode {
    let entry: &NodeCacheEntry = node.entry;
    IndexedNode {
        node_name: entry.node_name.to_string(),
        node_tag: entry.node_tag.to_string(),
        repo_id: repo.id,
        repo_label: repo.label.clone(),
        source_type: entry.origin.kind(),
        path: entry.origin.path_str().to_owned(),
        shadowed_by: node.shadowed_by.map(|winner| winner.label.clone()),
    }
}

/// The label of the listed repository `repo_id`, or a phrase saying there
/// is none: a cached entry can outlive its repository's configuration.
fn label_of(repos: &[ListedRepo], repo_id: u32) -> String {
    repos
        .iter()
        .find(|repo| repo.id == repo_id)
        .map(|repo| repo.label.clone())
        .unwrap_or_else(|| "no configured repository".to_owned())
}

/// The document `name:tag` resolves to through `entries`, by the priority
/// rule a sync applies. An identity the winning repository claims twice is
/// an error, as it is for a sync.
fn published_doc<E: RepoCacheEntry>(
    entries: &[E],
    repos: &[ListedRepo],
    name: &str,
    tag: &str,
) -> Result<Option<PublishedDoc>, String> {
    let winner =
        lookup_repo_entry(entries, name, tag).map_err(|ambiguity| ambiguity.to_string())?;
    Ok(winner.map(|entry| PublishedDoc {
        repo_id: entry.repo_id(),
        repo_label: label_of(repos, entry.repo_id()),
        path: entry.origin().path_str().to_owned(),
        sha256: entry.sha256().clone(),
    }))
}

/// What `sha256` does for a claim on `name:tag` at sync time, given the
/// cache of that kind and the document a plain reference resolves to.
fn pin_status<E: RepoCacheEntry>(
    entries: &[E],
    published: Option<&PublishedDoc>,
    repos: &[ListedRepo],
    name: &str,
    tag: &str,
    sha256: Option<&str>,
) -> PinStatus {
    let Some(raw) = sha256 else {
        return PinStatus::Unpinned;
    };
    let pin = match ManifestFingerprint::parse(raw) {
        Ok(pin) => pin,
        Err(e) => {
            return PinStatus::Unusable {
                reason: e.to_string(),
            };
        }
    };
    if published.is_some_and(|doc| doc.sha256 == pin) {
        return PinStatus::Current;
    }
    match lookup_repo_entry_by_sha256(entries, name, tag, &pin) {
        Some(entry) => PinStatus::Resolvable {
            repo_id: entry.repo_id(),
            repo_label: label_of(repos, entry.repo_id()),
            path: entry.origin().path_str().to_owned(),
        },
        None => PinStatus::Unresolvable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::repo::cache::test_support::{
        consumes, contract_entry, fingerprint, implements, node_entry, observes, pairing_entry,
        participates, with_links,
    };
    use crate::services::repo::cache::{
        ContractCacheEntry, EntryOrigin, NodeCacheEntry, PairingCacheEntry, repositories_list_path,
        write_repo_cache,
    };
    use std::path::{Path, PathBuf};

    /// A Peppy home whose root is canonical, so an entry under a
    /// repository directory is attributed to it on every platform.
    struct Home {
        _tmp: tempfile::TempDir,
        dirs: PeppyDirs,
        root: PathBuf,
    }

    impl Home {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(tmp.path()).unwrap();
            let dirs = PeppyDirs::new(&root);
            std::fs::create_dir_all(dirs.conf_dir()).unwrap();
            Self {
                _tmp: tmp,
                dirs,
                root,
            }
        }

        /// A repository directory under the home, created so its
        /// configured path canonicalizes to itself.
        fn repo(&self, name: &str) -> PathBuf {
            let dir = self.root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn configure(&self, repos: &[(u32, &Path)]) {
            let entries: Vec<serde_json::Value> = repos
                .iter()
                .map(|(id, path)| {
                    serde_json::json!({ "id": id, "type": "fs", "path": path.to_string_lossy() })
                })
                .collect();
            std::fs::write(
                repositories_list_path(&self.dirs),
                serde_json::to_string(&entries).unwrap(),
            )
            .unwrap();
        }

        fn exclude(&self, path: &Path) {
            std::fs::write(
                self.dirs.conf_dir().join("excluded_repositories.json5"),
                serde_json::to_string(&serde_json::json!([
                    { "id": 1, "type": "fs", "path": path.to_string_lossy() }
                ]))
                .unwrap(),
            )
            .unwrap();
        }

        fn nodes(&self, entries: &[NodeCacheEntry]) {
            write_repo_cache(&self.dirs, entries).unwrap();
        }

        fn contracts(&self, entries: &[ContractCacheEntry]) {
            write_repo_cache(&self.dirs, entries).unwrap();
        }

        fn pairings(&self, entries: &[PairingCacheEntry]) {
            write_repo_cache(&self.dirs, entries).unwrap();
        }
    }

    fn fs(path: PathBuf) -> EntryOrigin {
        EntryOrigin::Fs { path }
    }

    fn label(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    fn only_implements(
        entry: NodeCacheEntry,
        claim: crate::services::repo::cache::ImplementsClaim,
    ) -> NodeCacheEntry {
        with_links(
            entry,
            DeclaredLinks {
                implements: vec![claim],
                ..DeclaredLinks::default()
            },
        )
    }

    /// One identity, every kind of link on it, published as both a
    /// contract and a pairing: each hit lands in its section with the
    /// slot facts the manifest declared, and both documents are reported.
    #[test]
    fn every_link_kind_on_one_identity_is_reported() {
        let home = Home::new();
        let hub = home.repo("hub");
        home.configure(&[(1, &hub)]);
        let contract = contract_entry("rgb_camera", "v1", fs(hub.join("contracts/rgb.json5")));
        let contract_sha = contract.sha256.as_str().to_owned();
        home.contracts(&[contract]);
        let pairing = pairing_entry("rgb_camera", "v1", fs(hub.join("pairings/rgb.json5")));
        let pairing_sha = pairing.sha256.as_str().to_owned();
        home.pairings(&[pairing]);
        home.nodes(&[
            only_implements(
                node_entry("cam", "v1", fs(hub.join("cam/peppy.json5"))),
                implements("rgb_camera", "v1", "camera", Some(&contract_sha)),
            ),
            with_links(
                node_entry("recorder", "v1", fs(hub.join("recorder/peppy.json5"))),
                DeclaredLinks {
                    contracts: vec![consumes(
                        "rgb_camera",
                        "v1",
                        "frames",
                        Cardinality::ZeroOrMore,
                        None,
                    )],
                    ..DeclaredLinks::default()
                },
            ),
            with_links(
                node_entry("arm", "v1", fs(hub.join("arm/peppy.json5"))),
                DeclaredLinks {
                    pairings: vec![participates("rgb_camera", "v1", "arm", "arm", true, None)],
                    ..DeclaredLinks::default()
                },
            ),
            with_links(
                node_entry("logger", "v1", fs(hub.join("logger/peppy.json5"))),
                DeclaredLinks {
                    pairing_observers: vec![observes(
                        "rgb_camera",
                        "v1",
                        "controller",
                        "watch",
                        Cardinality::OneOrMore,
                        None,
                    )],
                    ..DeclaredLinks::default()
                },
            ),
            node_entry("plain", "v1", fs(hub.join("plain/peppy.json5"))),
        ]);

        let report = search_identity(&home.dirs, "rgb_camera", "v1").expect("searches");

        let node = |name: &str| IndexedNode {
            node_name: name.to_owned(),
            node_tag: "v1".to_owned(),
            repo_id: 1,
            repo_label: label(&hub),
            source_type: RepoSourceKind::Fs,
            path: label(&hub.join(format!("{name}/peppy.json5"))),
            shadowed_by: None,
        };
        assert_eq!(
            report.contract,
            Some(PublishedDoc {
                repo_id: 1,
                repo_label: label(&hub),
                path: label(&hub.join("contracts/rgb.json5")),
                sha256: ManifestFingerprint::parse(&contract_sha).unwrap(),
            })
        );
        assert_eq!(
            report.pairing,
            Some(PublishedDoc {
                repo_id: 1,
                repo_label: label(&hub),
                path: label(&hub.join("pairings/rgb.json5")),
                sha256: ManifestFingerprint::parse(&pairing_sha).unwrap(),
            })
        );
        assert_eq!(
            report.implementers,
            vec![Implementer {
                node: node("cam"),
                link_id: "camera".to_owned(),
                sha256: Some(contract_sha),
                pin: PinStatus::Current,
            }]
        );
        assert_eq!(
            report.consumers,
            vec![Consumer {
                node: node("recorder"),
                link_id: "frames".to_owned(),
                cardinality: Cardinality::ZeroOrMore,
                sha256: None,
                pin: PinStatus::Unpinned,
            }]
        );
        assert_eq!(
            report.participants,
            vec![Participant {
                node: node("arm"),
                role: "arm".to_owned(),
                link_id: "arm".to_owned(),
                optional: true,
                sha256: None,
                pin: PinStatus::Unpinned,
            }]
        );
        assert_eq!(
            report.observers,
            vec![Observer {
                node: node("logger"),
                role: "controller".to_owned(),
                link_id: "watch".to_owned(),
                cardinality: Cardinality::OneOrMore,
                sha256: None,
                pin: PinStatus::Unpinned,
            }]
        );
        assert_eq!(report.excluded_hint, "");
    }

    /// An implementer whose `name:tag` a lower-id repository also provides
    /// is reported as shadowed: a launch resolves to the other node, which
    /// makes no such claim.
    #[test]
    fn an_implementer_shadowed_by_a_higher_priority_repository_is_marked() {
        let home = Home::new();
        let top = home.repo("top");
        let hub = home.repo("hub");
        home.configure(&[(5, &top), (1000, &hub)]);
        home.contracts(&[contract_entry(
            "rgb_camera",
            "v1",
            fs(hub.join("contracts/rgb.json5")),
        )]);
        home.nodes(&[
            only_implements(
                node_entry("cam", "v1", fs(hub.join("cam/peppy.json5"))),
                implements("rgb_camera", "v1", "camera", None),
            ),
            node_entry("cam", "v1", fs(top.join("cam/peppy.json5"))),
        ]);

        let report = search_identity(&home.dirs, "rgb_camera", "v1").expect("searches");

        assert_eq!(report.implementers.len(), 1);
        let hit = &report.implementers[0].node;
        assert_eq!(hit.repo_id, 1000);
        assert_eq!(hit.repo_label, label(&hub));
        assert_eq!(
            hit.shadowed_by,
            Some(label(&top)),
            "the lower-id repository's `cam:v1` wins"
        );
    }

    /// An excluded repository's nodes are not reported, and the hint says
    /// it may have provided some.
    #[test]
    fn nodes_of_an_excluded_repository_are_not_reported() {
        let home = Home::new();
        let hub = home.repo("hub");
        let extra = home.repo("extra");
        home.configure(&[(1, &hub), (2, &extra)]);
        home.exclude(&extra);
        home.contracts(&[contract_entry(
            "rgb_camera",
            "v1",
            fs(hub.join("contracts/rgb.json5")),
        )]);
        home.nodes(&[
            only_implements(
                node_entry("cam", "v1", fs(hub.join("cam/peppy.json5"))),
                implements("rgb_camera", "v1", "camera", None),
            ),
            only_implements(
                node_entry("other_cam", "v1", fs(extra.join("other_cam/peppy.json5"))),
                implements("rgb_camera", "v1", "camera", None),
            ),
        ]);

        let report = search_identity(&home.dirs, "rgb_camera", "v1").expect("searches");

        let names: Vec<&str> = report
            .implementers
            .iter()
            .map(|hit| hit.node.node_name.as_str())
            .collect();
        assert_eq!(names, vec!["cam"]);
        assert!(
            report.excluded_hint.contains("1 excluded repository"),
            "got: {}",
            report.excluded_hint
        );
        assert!(
            report.excluded_hint.contains(&label(&extra)),
            "got: {}",
            report.excluded_hint
        );
    }

    /// Each pin state is decided the way a sync decides it: unpinned takes
    /// the published copy, a pin equal to it is current, a pin another
    /// repository's copy carries resolves through that copy, a pin nothing
    /// carries fails, and a pin that is not a fingerprint is refused. A
    /// pairing claim is checked against the pairings cache, never the
    /// contracts one. Hits come back ordered by slot within a node.
    #[test]
    fn pin_status_follows_what_a_sync_would_do() {
        let home = Home::new();
        let hub = home.repo("hub");
        let mirror = home.repo("mirror");
        home.configure(&[(1, &hub), (2, &mirror)]);
        let current = contract_entry("rgb_camera", "v1", fs(hub.join("contracts/rgb.json5")));
        let current_sha = current.sha256.as_str().to_owned();
        let mut older = contract_entry("rgb_camera", "v1", fs(mirror.join("rgb.json5")));
        older.sha256 = fingerprint("older revision");
        let older_sha = older.sha256.as_str().to_owned();
        home.contracts(&[current, older]);
        let pairing = pairing_entry("rgb_camera", "v1", fs(hub.join("pairings/rgb.json5")));
        let pairing_sha = pairing.sha256.as_str().to_owned();
        home.pairings(&[pairing]);
        let absent = fingerprint("never published");
        home.nodes(&[with_links(
            node_entry("cam", "v1", fs(hub.join("cam/peppy.json5"))),
            DeclaredLinks {
                implements: vec![
                    implements("rgb_camera", "v1", "e_unusable", Some("abc")),
                    implements("rgb_camera", "v1", "d_unresolvable", Some(absent.as_str())),
                    implements("rgb_camera", "v1", "c_resolvable", Some(&older_sha)),
                    implements("rgb_camera", "v1", "b_current", Some(&current_sha)),
                    implements("rgb_camera", "v1", "a_unpinned", None),
                ],
                contracts: vec![consumes(
                    "rgb_camera",
                    "v1",
                    "frames",
                    Cardinality::One,
                    Some(&older_sha),
                )],
                pairings: vec![participates(
                    "rgb_camera",
                    "v1",
                    "arm",
                    "arm",
                    false,
                    Some(&pairing_sha),
                )],
                pairing_observers: vec![observes(
                    "rgb_camera",
                    "v1",
                    "controller",
                    "watch",
                    Cardinality::One,
                    Some(&current_sha),
                )],
            },
        )]);

        let report = search_identity(&home.dirs, "rgb_camera", "v1").expect("searches");

        let pins: Vec<(&str, &PinStatus)> = report
            .implementers
            .iter()
            .map(|hit| (hit.link_id.as_str(), &hit.pin))
            .collect();
        let resolvable = PinStatus::Resolvable {
            repo_id: 2,
            repo_label: label(&mirror),
            path: label(&mirror.join("rgb.json5")),
        };
        assert_eq!(pins[0], ("a_unpinned", &PinStatus::Unpinned));
        assert_eq!(pins[1], ("b_current", &PinStatus::Current));
        assert_eq!(pins[2], ("c_resolvable", &resolvable));
        assert_eq!(pins[3], ("d_unresolvable", &PinStatus::Unresolvable));
        let (link_id, PinStatus::Unusable { reason }) = pins[4] else {
            panic!("`abc` is not a fingerprint: {:?}", pins[4]);
        };
        assert_eq!(link_id, "e_unusable");
        assert!(reason.contains("64 hexadecimal"), "got: {reason}");
        assert_eq!(pins.len(), 5);

        assert_eq!(report.consumers[0].pin, resolvable);
        assert_eq!(report.participants[0].pin, PinStatus::Current);
        assert_eq!(
            report.observers[0].pin,
            PinStatus::Unresolvable,
            "a contract's fingerprint names no pairing"
        );
    }

    /// An identity nobody publishes or uses is an empty report, not an
    /// error: "nobody uses it" is an answer.
    #[test]
    fn an_unknown_identity_reports_nothing() {
        let home = Home::new();
        let hub = home.repo("hub");
        home.configure(&[(1, &hub)]);
        home.nodes(&[only_implements(
            node_entry("cam", "v1", fs(hub.join("cam/peppy.json5"))),
            implements("rgb_camera", "v1", "camera", None),
        )]);

        let report = search_identity(&home.dirs, "nobody", "v1").expect("searches");

        assert_eq!(
            report,
            SearchReport {
                contract: None,
                pairing: None,
                implementers: Vec::new(),
                consumers: Vec::new(),
                participants: Vec::new(),
                observers: Vec::new(),
                excluded_hint: String::new(),
            }
        );
    }

    /// A nodes cache that does not parse is refused with the message that
    /// names the fix, rather than searched as if empty.
    #[test]
    fn an_unparseable_nodes_cache_is_an_error() {
        let home = Home::new();
        let hub = home.repo("hub");
        home.configure(&[(1, &hub)]);
        std::fs::create_dir_all(home.dirs.cache_dir()).unwrap();
        std::fs::write(
            crate::services::repo::cache::nodes_repo_cache_path(&home.dirs),
            "[{ node_name: \"cam\" }]",
        )
        .unwrap();

        let err = search_identity(&home.dirs, "rgb_camera", "v1").expect_err("refuses");

        assert!(err.contains("does not parse"), "got: {err}");
        assert!(err.contains("peppy repo update"), "got: {err}");
    }

    /// A contract one repository publishes twice has no copy a sync would
    /// pick, so the search refuses the way a sync does.
    #[test]
    fn a_contested_published_identity_is_an_error() {
        let home = Home::new();
        let hub = home.repo("hub");
        home.configure(&[(1, &hub)]);
        home.contracts(&[
            contract_entry("rgb_camera", "v1", fs(hub.join("a/rgb.json5"))),
            contract_entry("rgb_camera", "v1", fs(hub.join("b/rgb.json5"))),
        ]);
        home.nodes(&[]);

        let err = search_identity(&home.dirs, "rgb_camera", "v1").expect_err("refuses");

        assert!(err.contains("2 contract entries"), "got: {err}");
        assert!(err.contains("`rgb_camera:v1`"), "got: {err}");
    }
}
