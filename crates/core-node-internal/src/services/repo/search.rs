//! What a search query names across this machine's caches: every indexed
//! item whose identity matches, and who uses it once the query settles on
//! one identity.
//!
//! Every answer comes from the caches `repo refresh` wrote, so a search
//! never reads a manifest or materializes a checkout: it answers for what
//! the last refresh saw, which is also what a launch resolves. Usage hits
//! come from the links recorded on each node entry ([`DeclaredLinks`]);
//! nodes are attributed to repositories, and shadowed, exactly as
//! `repo list` shows them, through [`nodes_by_repository`].

use crate::services::repo::cache::{
    ContractCacheEntry, DeclaredLinks, LauncherCacheEntry, McpExposureCacheEntry, NodeCacheEntry,
    PairingCacheEntry, RepoCacheEntry, excluded_repositories_hint, load_contract_cache,
    load_node_cache, load_pairing_cache, load_repo_cache, lookup_repo_entry,
    lookup_repo_entry_by_sha256,
};
use crate::services::repo::{
    AttributedNode, ListedRepo, RepoNodes, listed_repositories, nodes_by_repository,
};
use config::node::Cardinality;
use core_node_api::encoding::{RepoItemKind, RepoSourceKind};
use daemon_config::consts::PeppyDirs;
use daemon_config::repository::ManifestFingerprint;
use std::collections::BTreeSet;

/// A parsed `<name-regex>[:<tag-regex>][@<sha256>]` search query.
///
/// Each part is an unanchored [`regex::Regex`], so `camera` finds
/// `rgb_camera` and `^camera$` finds only `camera`. A missing tag part
/// matches every tag, the empty one of untagged kinds included; a digest
/// keeps only the copies carrying exactly those bytes.
#[derive(Debug, Clone)]
pub struct SearchQuery {
    raw: String,
    name: regex::Regex,
    tag: Option<regex::Regex>,
    digest: Option<ManifestFingerprint>,
}

impl SearchQuery {
    /// Parses `raw`: the digest is split off at the last `@` and must be a
    /// sha256 fingerprint, the name and tag parts are split at the one
    /// allowed `:`, and each part must be a non-empty, valid regular
    /// expression. A query that does not fit the grammar is refused with
    /// the rule it broke; an item name never contains `:` or `@`, so the
    /// splits never take a matchable pattern apart.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(
                "the search query is empty; write `<name-regex>[:<tag-regex>][@<sha256>]`"
                    .to_owned(),
            );
        }
        let (patterns, digest) = match raw.rsplit_once('@') {
            Some((patterns, suffix)) => {
                let digest = ManifestFingerprint::parse(suffix)
                    .map_err(|e| format!("search digest `{suffix}`: {e}"))?;
                (patterns, Some(digest))
            }
            None => (raw, None),
        };
        let (name, tag) = match patterns.split_once(':') {
            Some((name, tag)) => (name, Some(tag)),
            None => (patterns, None),
        };
        if let Some(tag) = tag
            && tag.contains(':')
        {
            return Err(format!("search query `{raw}` must contain at most one `:`"));
        }
        if name.is_empty() {
            return Err(format!(
                "search query `{raw}` has an empty name pattern; write `.*` to match every name"
            ));
        }
        if let Some(tag) = tag
            && tag.is_empty()
        {
            return Err(format!(
                "search query `{raw}` has an empty tag pattern; drop the `:` to match every tag"
            ));
        }
        Ok(Self {
            raw: raw.to_owned(),
            name: compile_pattern(name)?,
            tag: tag.map(compile_pattern).transpose()?,
            digest,
        })
    }

    /// The query as typed (trimmed), for echoing back in output.
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// The name part as typed, for machine-readable output.
    pub fn name_pattern(&self) -> &str {
        self.name.as_str()
    }

    /// The tag part as typed, when the query has one.
    pub fn tag_pattern(&self) -> Option<&str> {
        self.tag.as_ref().map(|pattern| pattern.as_str())
    }

    /// The digest, when the query pins one.
    pub fn digest(&self) -> Option<&ManifestFingerprint> {
        self.digest.as_ref()
    }

    /// Whether the patterns match `(name, tag)`. The digest is a filter on
    /// copies, not identities, so it plays no part here.
    fn matches(&self, name: &str, tag: &str) -> bool {
        self.name.is_match(name)
            && self
                .tag
                .as_ref()
                .is_none_or(|pattern| pattern.is_match(tag))
    }

    /// Whether the name pattern matches the whole name: such a hit ranks
    /// ahead of one the pattern merely brushes. The leftmost match must
    /// cover the name from start to end, so the pattern itself needs no
    /// second, anchored compilation.
    fn names_outright(&self, name: &str) -> bool {
        self.name
            .find(name)
            .is_some_and(|hit| hit.start() == 0 && hit.end() == name.len())
    }
}

/// Compiles one query part into its unanchored matcher.
fn compile_pattern(part: &str) -> Result<regex::Regex, String> {
    regex::Regex::new(part)
        .map_err(|e| format!("search pattern `{part}` is not a valid regular expression: {e}"))
}

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

/// The document a reference resolves to: the lowest-id repository's copy,
/// or, for a digest query, the copy carrying that digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedDoc {
    pub repo_id: u32,
    /// The repository's label, or "no configured repository" for a cached
    /// copy whose repository is no longer listed.
    pub repo_label: String,
    pub path: String,
    pub sha256: ManifestFingerprint,
}

/// One identity the query matches, with the copy the answer points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedItem {
    pub kind: RepoItemKind,
    pub name: String,
    /// Empty for untagged kinds (launchers).
    pub tag: String,
    /// The name pattern matches the whole name, not just a part of it.
    pub exact: bool,
    pub published: PublishedDoc,
}

/// Everything a search says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOutcome {
    /// Every identity the query matches, ranked: names the pattern spells
    /// out first, then by name, tag and kind.
    pub matches: Vec<MatchedItem>,
    /// Who uses the identity the query settles on: the only matched
    /// `name:tag`, or the only one the name pattern spells out in full
    /// ([`settled_identity`]). An identity published as both a contract
    /// and a pairing has two entries in `matches` and one report covering
    /// both.
    pub detail: Option<SearchReport>,
    /// One report per matched identity in match order, filled only when
    /// the search is asked for full reports (`--full`); empty otherwise.
    pub details: Vec<SearchReport>,
    /// [`excluded_repositories_hint`], so an empty result can say that an
    /// excluded repository may have provided the item without the caller
    /// knowing the cache layout. Empty when nothing is excluded.
    pub excluded_hint: String,
}

/// Who uses one identity: the nodes that implement, consume, participate
/// in, or observe it, read from the links `repo refresh` recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchReport {
    pub name: String,
    pub tag: String,
    pub implementers: Vec<Implementer>,
    pub consumers: Vec<Consumer>,
    pub participants: Vec<Participant>,
    pub observers: Vec<Observer>,
}

/// Searches this machine's caches for every indexed item the query
/// matches, across all five kinds. Once the query settles on one
/// `name:tag` ([`settled_identity`]), the outcome also reports who uses
/// that identity, with each claim's pin checked against the cached
/// documents. `full` asks for that report on every matched identity
/// instead, the way `apt search --full` prints every record whole.
///
/// An unreadable cache is an error. An identity one repository publishes
/// twice is an error only when the query settles on it or a full search
/// demands its report, since it has no answer a launch would accept; a
/// plain match list keeps its row so the other matches still answer.
pub fn search_repo_items(
    peppy_dirs: &PeppyDirs,
    query: &SearchQuery,
    full: bool,
) -> Result<SearchOutcome, String> {
    let nodes = load_node_cache(peppy_dirs).map_err(|e| e.to_string())?;
    let launchers: Vec<LauncherCacheEntry> =
        load_repo_cache(peppy_dirs).map_err(|e| e.to_string())?;
    let contracts = load_contract_cache(peppy_dirs).map_err(|e| e.to_string())?;
    let pairings = load_pairing_cache(peppy_dirs).map_err(|e| e.to_string())?;
    let exposures: Vec<McpExposureCacheEntry> =
        load_repo_cache(peppy_dirs).map_err(|e| e.to_string())?;
    let repos = listed_repositories(peppy_dirs).map_err(|e| e.to_string())?;

    let mut contests = Vec::new();
    let mut matches = matched_identities(&nodes, &repos, query, &mut contests);
    matches.extend(matched_identities(&launchers, &repos, query, &mut contests));
    matches.extend(matched_identities(&contracts, &repos, query, &mut contests));
    matches.extend(matched_identities(&pairings, &repos, query, &mut contests));
    matches.extend(matched_identities(&exposures, &repos, query, &mut contests));
    matches.sort_by(|a, b| match_order(a).cmp(&match_order(b)));

    let settled = settled_identity(&matches);
    if let Some((name, tag)) = &settled
        && let Some(contest) = contests
            .iter()
            .find(|contest| &contest.name == name && &contest.tag == tag)
    {
        return Err(contest.error.clone());
    }
    let detail = match settled {
        Some((name, tag)) => Some(usage_report(
            &nodes, &contracts, &pairings, &repos, &name, &tag,
        )?),
        None => None,
    };

    let mut details = Vec::new();
    if full {
        let mut reported = BTreeSet::new();
        for item in &matches {
            if !reported.insert((item.name.clone(), item.tag.clone())) {
                continue;
            }
            if let Some(contest) = contests
                .iter()
                .find(|contest| contest.name == item.name && contest.tag == item.tag)
            {
                return Err(contest.error.clone());
            }
            details.push(usage_report(
                &nodes, &contracts, &pairings, &repos, &item.name, &item.tag,
            )?);
        }
    }

    Ok(SearchOutcome {
        matches,
        detail,
        details,
        excluded_hint: excluded_repositories_hint(peppy_dirs),
    })
}

/// The identity the query settles on: the one every match shares, or,
/// failing that, the one every outright name match shares. A pattern that
/// spells a name out in full settles on that identity even while it also
/// brushes longer names, so `rgb_camera:v1` keeps its report when
/// `sim_rgb_camera:v1` matches too.
fn settled_identity(matches: &[MatchedItem]) -> Option<(String, String)> {
    single_identity(matches.iter()).or_else(|| single_identity(matches.iter().filter(|m| m.exact)))
}

/// The one `(name, tag)` every item shares, if they all do.
fn single_identity<'a>(
    mut items: impl Iterator<Item = &'a MatchedItem>,
) -> Option<(String, String)> {
    let first = items.next()?;
    items
        .all(|m| m.name == first.name && m.tag == first.tag)
        .then(|| (first.name.clone(), first.tag.clone()))
}

/// An identity its winning repository publishes twice: the search refuses
/// it only once the query settles on it alone.
struct ContestedIdentity {
    name: String,
    tag: String,
    error: String,
}

/// Every distinct identity in `entries` the query matches, each with the
/// copy the answer points at: the one a plain reference resolves to, or,
/// for a digest query, the highest-priority copy carrying that digest.
/// An identity none of whose copies carries the digest is not a match.
/// An identity its winning repository publishes twice is listed by that
/// repository's first copy in path order and recorded in `contests`, so
/// the caller decides whether the contest matters.
fn matched_identities<E: RepoCacheEntry>(
    entries: &[E],
    repos: &[ListedRepo],
    query: &SearchQuery,
    contests: &mut Vec<ContestedIdentity>,
) -> Vec<MatchedItem> {
    let identities: BTreeSet<(&str, &str)> = entries
        .iter()
        .filter(|e| query.matches(e.name(), e.tag()))
        .map(|e| (e.name(), e.tag()))
        .collect();

    let mut matches = Vec::new();
    for (name, tag) in identities {
        let published = match query.digest() {
            None => match published_doc(entries, repos, name, tag) {
                Ok(doc) => doc.expect("the identity came from these entries"),
                Err(error) => {
                    contests.push(ContestedIdentity {
                        name: name.to_owned(),
                        tag: tag.to_owned(),
                        error,
                    });
                    let copy = entries
                        .iter()
                        .filter(|e| e.name() == name && e.tag() == tag)
                        .min_by_key(|e| (e.repo_id(), e.origin().path_str().to_owned()))
                        .expect("the identity came from these entries");
                    PublishedDoc {
                        repo_id: copy.repo_id(),
                        repo_label: label_of(repos, copy.repo_id()),
                        path: copy.origin().path_str().to_owned(),
                        sha256: copy.sha256().clone(),
                    }
                }
            },
            Some(digest) => {
                let carriers = entries
                    .iter()
                    .filter(|e| e.name() == name && e.tag() == tag && e.sha256() == digest);
                let Some(entry) = carriers.min_by_key(|e| e.repo_id()) else {
                    continue;
                };
                PublishedDoc {
                    repo_id: entry.repo_id(),
                    repo_label: label_of(repos, entry.repo_id()),
                    path: entry.origin().path_str().to_owned(),
                    sha256: entry.sha256().clone(),
                }
            }
        };
        matches.push(MatchedItem {
            kind: E::ITEM_KIND,
            name: name.to_owned(),
            tag: tag.to_owned(),
            exact: query.names_outright(name),
            published,
        });
    }
    matches
}

/// The order matches are listed in: a name the pattern spells out ranks
/// first, then name, tag and kind.
fn match_order(item: &MatchedItem) -> (bool, &str, &str, u8) {
    (!item.exact, &item.name, &item.tag, kind_rank(item.kind))
}

/// Kinds in the order `repo refresh` discovers them.
fn kind_rank(kind: RepoItemKind) -> u8 {
    match kind {
        RepoItemKind::Node => 0,
        RepoItemKind::Launcher => 1,
        RepoItemKind::Contract => 2,
        RepoItemKind::Pairing => 3,
        RepoItemKind::McpExposure => 4,
    }
}

/// Who uses `name:tag`: the nodes whose cached links implement, consume,
/// participate in, or observe it, with each claim's pin checked. Each
/// section is ordered by repository id, then node name, tag and slot.
fn usage_report(
    nodes: &[NodeCacheEntry],
    contracts: &[ContractCacheEntry],
    pairings: &[PairingCacheEntry],
    repos: &[ListedRepo],
    name: &str,
    tag: &str,
) -> Result<SearchReport, String> {
    let contract = published_doc(contracts, repos, name, tag)?;
    let pairing = published_doc(pairings, repos, name, tag)?;

    let contract_pin =
        |sha256: Option<&str>| pin_status(contracts, contract.as_ref(), repos, name, tag, sha256);
    let pairing_pin =
        |sha256: Option<&str>| pin_status(pairings, pairing.as_ref(), repos, name, tag, sha256);
    let about = |claim_name: &config::runtime::Name, claim_tag: &str| {
        claim_name.as_str() == name && claim_tag == tag
    };

    let mut implementers = Vec::new();
    let mut consumers = Vec::new();
    let mut participants = Vec::new();
    let mut observers = Vec::new();
    for RepoNodes { repo, nodes } in nodes_by_repository(repos, nodes) {
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
        name: name.to_owned(),
        tag: tag.to_owned(),
        implementers,
        consumers,
        participants,
        observers,
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
        consumes, contract_entry, fingerprint, implements, launcher_entry, mcp_exposure_entry,
        node_entry, observes, pairing_entry, participates, with_links,
    };
    use crate::services::repo::cache::{EntryOrigin, repositories_list_path, write_repo_cache};
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

        /// Writes the cache of `E`'s kind; the entries name the kind.
        fn cache<E: RepoCacheEntry>(&self, entries: &[E]) {
            write_repo_cache(&self.dirs, entries).unwrap();
        }
    }

    fn search(home: &Home, raw: &str) -> Result<SearchOutcome, String> {
        search_repo_items(
            &home.dirs,
            &SearchQuery::parse(raw).expect("a valid query"),
            false,
        )
    }

    fn search_full(home: &Home, raw: &str) -> Result<SearchOutcome, String> {
        search_repo_items(
            &home.dirs,
            &SearchQuery::parse(raw).expect("a valid query"),
            true,
        )
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

    #[test]
    fn parse_accepts_a_bare_name_pattern() {
        let query = SearchQuery::parse(" openarm ").expect("parses");
        assert_eq!(query.raw(), "openarm");
        assert_eq!(query.name_pattern(), "openarm");
        assert_eq!(query.tag_pattern(), None);
        assert_eq!(query.digest(), None);
    }

    #[test]
    fn parse_splits_name_tag_and_digest() {
        let digest = "A".repeat(64);
        let query = SearchQuery::parse(&format!("openarm_v2:v1@{digest}")).expect("parses");
        assert_eq!(query.name_pattern(), "openarm_v2");
        assert_eq!(query.tag_pattern(), Some("v1"));
        assert_eq!(
            query.digest().map(|d| d.as_str().to_owned()),
            Some("a".repeat(64)),
            "the digest is normalized to lowercase"
        );
    }

    #[test]
    fn parse_refuses_a_digest_that_is_not_a_fingerprint() {
        let err = SearchQuery::parse("cam@abc").expect_err("refuses");
        assert!(err.contains("search digest `abc`"), "got: {err}");
        assert!(err.contains("64 hexadecimal"), "got: {err}");
        let err = SearchQuery::parse("cam@").expect_err("refuses");
        assert!(err.contains("empty"), "got: {err}");
    }

    #[test]
    fn parse_refuses_an_empty_part() {
        let err = SearchQuery::parse("  ").expect_err("refuses");
        assert!(err.contains("search query is empty"), "got: {err}");
        let err = SearchQuery::parse("cam:").expect_err("refuses");
        assert!(err.contains("empty tag pattern"), "got: {err}");
        let err = SearchQuery::parse(":v1").expect_err("refuses");
        assert!(err.contains("empty name pattern"), "got: {err}");
    }

    #[test]
    fn parse_refuses_a_second_colon() {
        let err = SearchQuery::parse("a:b:c").expect_err("refuses");
        assert!(err.contains("at most one `:`"), "got: {err}");
    }

    #[test]
    fn parse_refuses_an_invalid_regex() {
        let err = SearchQuery::parse("cam[").expect_err("refuses");
        assert!(err.contains("`cam[`"), "got: {err}");
        assert!(err.contains("not a valid regular expression"), "got: {err}");
    }

    /// Matching is unanchored, as `apt search` matches; ranking a hit as
    /// exact takes the whole name, and `^…$` makes exactness explicit.
    #[test]
    fn matching_is_unanchored_and_exactness_needs_the_whole_name() {
        let query = SearchQuery::parse("cam").expect("parses");
        assert!(query.matches("uvc_camera", "v1"));
        assert!(!query.names_outright("uvc_camera"));
        assert!(query.names_outright("cam"));

        let anchored = SearchQuery::parse("^cam$").expect("parses");
        assert!(anchored.matches("cam", "v1"));
        assert!(!anchored.matches("uvc_camera", "v1"));
    }

    /// A verbose-mode pattern whose comment runs to the end of the query
    /// is a valid query, and exactness still takes the whole name.
    #[test]
    fn a_verbose_pattern_with_a_trailing_comment_is_accepted() {
        let query = SearchQuery::parse("(?x)cam # the short name").expect("parses");
        assert!(query.matches("cam", "v1"));
        assert!(query.names_outright("cam"));
        assert!(!query.names_outright("uvc_camera"));
    }

    /// One identity, every kind of link on it, published as both a
    /// contract and a pairing: both documents are matches, one identity
    /// means the usage report comes along, and each hit lands in its
    /// section with the slot facts the manifest declared.
    #[test]
    fn every_link_kind_on_one_identity_is_reported() {
        let home = Home::new();
        let hub = home.repo("hub");
        home.configure(&[(1, &hub)]);
        let contract = contract_entry("rgb_camera", "v1", fs(hub.join("contracts/rgb.json5")));
        let contract_sha = contract.sha256.as_str().to_owned();
        home.cache(&[contract]);
        let pairing = pairing_entry("rgb_camera", "v1", fs(hub.join("pairings/rgb.json5")));
        let pairing_sha = pairing.sha256.as_str().to_owned();
        home.cache(&[pairing]);
        home.cache(&[
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

        let outcome = search(&home, "rgb_camera:v1").expect("searches");

        assert_eq!(
            outcome.matches,
            vec![
                MatchedItem {
                    kind: RepoItemKind::Contract,
                    name: "rgb_camera".to_owned(),
                    tag: "v1".to_owned(),
                    exact: true,
                    published: PublishedDoc {
                        repo_id: 1,
                        repo_label: label(&hub),
                        path: label(&hub.join("contracts/rgb.json5")),
                        sha256: ManifestFingerprint::parse(&contract_sha).unwrap(),
                    },
                },
                MatchedItem {
                    kind: RepoItemKind::Pairing,
                    name: "rgb_camera".to_owned(),
                    tag: "v1".to_owned(),
                    exact: true,
                    published: PublishedDoc {
                        repo_id: 1,
                        repo_label: label(&hub),
                        path: label(&hub.join("pairings/rgb.json5")),
                        sha256: ManifestFingerprint::parse(&pairing_sha).unwrap(),
                    },
                },
            ]
        );
        assert_eq!(outcome.excluded_hint, "");

        let report = outcome.detail.expect("one identity carries its report");
        assert_eq!(report.name, "rgb_camera");
        assert_eq!(report.tag, "v1");
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
    }

    /// A partial name lists every matching identity across the five kinds
    /// with no usage report, ordered by name whatever the kind.
    #[test]
    fn a_partial_name_lists_every_matching_identity() {
        let home = Home::new();
        let hub = home.repo("hub");
        home.configure(&[(1, &hub)]);
        home.cache(&[node_entry(
            "uvc_camera",
            "v1",
            fs(hub.join("uvc_camera/peppy.json5")),
        )]);
        home.cache(&[launcher_entry(
            "camera_boot",
            fs(hub.join("camera_boot.json5")),
        )]);
        home.cache(&[contract_entry(
            "rgb_camera",
            "v1",
            fs(hub.join("rgb.json5")),
        )]);
        home.cache(&[mcp_exposure_entry(
            "camera_surface",
            "v1",
            fs(hub.join("camera_surface.json5")),
        )]);

        let outcome = search(&home, "camera").expect("searches");

        let identities: Vec<(RepoItemKind, &str, &str)> = outcome
            .matches
            .iter()
            .map(|m| (m.kind, m.name.as_str(), m.tag.as_str()))
            .collect();
        assert_eq!(
            identities,
            vec![
                (RepoItemKind::Launcher, "camera_boot", ""),
                (RepoItemKind::McpExposure, "camera_surface", "v1"),
                (RepoItemKind::Contract, "rgb_camera", "v1"),
                (RepoItemKind::Node, "uvc_camera", "v1"),
            ]
        );
        assert!(outcome.matches.iter().all(|m| !m.exact));
        assert_eq!(outcome.detail, None);
    }

    /// A name the pattern spells out ranks ahead of one it merely brushes,
    /// whatever the kinds involved.
    #[test]
    fn an_outright_name_match_ranks_before_partial_ones() {
        let home = Home::new();
        let hub = home.repo("hub");
        home.configure(&[(1, &hub)]);
        home.cache(&[contract_entry("cam", "v1", fs(hub.join("cam.json5")))]);
        home.cache(&[node_entry(
            "uvc_camera",
            "v1",
            fs(hub.join("uvc_camera/peppy.json5")),
        )]);

        let outcome = search(&home, "cam").expect("searches");

        let names: Vec<(&str, bool)> = outcome
            .matches
            .iter()
            .map(|m| (m.name.as_str(), m.exact))
            .collect();
        assert_eq!(names, vec![("cam", true), ("uvc_camera", false)]);
        let report = outcome
            .detail
            .expect("the pattern spells `cam` out in full");
        assert_eq!((report.name.as_str(), report.tag.as_str()), ("cam", "v1"));
    }

    /// A pattern that spells exactly one matched name out in full settles
    /// on that identity: its usage report comes along even while the
    /// pattern also brushes longer names, which stay listed.
    #[test]
    fn an_outright_name_settles_among_partial_matches() {
        let home = Home::new();
        let hub = home.repo("hub");
        home.configure(&[(1, &hub)]);
        home.cache(&[contract_entry(
            "rgb_camera",
            "v1",
            fs(hub.join("rgb.json5")),
        )]);
        home.cache(&[pairing_entry(
            "sim_rgb_camera_link",
            "v1",
            fs(hub.join("sim_link.json5")),
        )]);
        home.cache(&[
            node_entry("sim_rgb_camera", "v1", fs(hub.join("sim/peppy.json5"))),
            with_links(
                node_entry("cam", "v1", fs(hub.join("cam/peppy.json5"))),
                DeclaredLinks {
                    implements: vec![implements("rgb_camera", "v1", "camera", None)],
                    ..DeclaredLinks::default()
                },
            ),
        ]);

        let outcome = search(&home, "rgb_camera:v1").expect("searches");

        let identities: Vec<(&str, bool)> = outcome
            .matches
            .iter()
            .map(|m| (m.name.as_str(), m.exact))
            .collect();
        assert_eq!(
            identities,
            vec![
                ("rgb_camera", true),
                ("sim_rgb_camera", false),
                ("sim_rgb_camera_link", false),
            ]
        );
        let report = outcome
            .detail
            .expect("the query settles on `rgb_camera:v1`");
        assert_eq!(
            (report.name.as_str(), report.tag.as_str()),
            ("rgb_camera", "v1")
        );
        assert_eq!(report.implementers.len(), 1);
    }

    /// A query without a tag part matches an untagged launcher; a tag
    /// pattern is matched against the launcher's empty tag, so `v1` never
    /// finds it and `.*` does.
    #[test]
    fn a_missing_tag_part_matches_untagged_launchers() {
        let home = Home::new();
        let hub = home.repo("hub");
        home.configure(&[(1, &hub)]);
        home.cache(&[launcher_entry(
            "camera_boot",
            fs(hub.join("camera_boot.json5")),
        )]);

        let outcome = search(&home, "camera_boot").expect("searches");
        assert_eq!(outcome.matches.len(), 1);
        assert_eq!(outcome.matches[0].kind, RepoItemKind::Launcher);
        assert_eq!(outcome.matches[0].tag, "");
        let report = outcome.detail.expect("one identity carries its report");
        assert_eq!(
            (report.name.as_str(), report.tag.as_str()),
            ("camera_boot", "")
        );
        assert_eq!(report.implementers, Vec::new());

        let outcome = search(&home, "camera_boot:v1").expect("searches");
        assert_eq!(outcome.matches, Vec::new());

        let outcome = search(&home, "camera_boot:.*").expect("searches");
        assert_eq!(outcome.matches.len(), 1);
    }

    /// A regex tag part spans every tag it matches; more than one identity
    /// means no usage report.
    #[test]
    fn a_regex_tag_part_spans_every_matching_tag() {
        let home = Home::new();
        let hub = home.repo("hub");
        home.configure(&[(1, &hub)]);
        home.cache(&[
            contract_entry("rgb_camera", "v1", fs(hub.join("rgb_v1.json5"))),
            contract_entry("rgb_camera", "v2", fs(hub.join("rgb_v2.json5"))),
        ]);

        let outcome = search(&home, "rgb_camera:v[12]").expect("searches");

        let tags: Vec<&str> = outcome.matches.iter().map(|m| m.tag.as_str()).collect();
        assert_eq!(tags, vec!["v1", "v2"]);
        assert_eq!(outcome.detail, None);
    }

    /// A digest query points at the copy carrying those bytes, wherever it
    /// is published, and matches nothing when no copy carries them.
    #[test]
    fn a_digest_query_finds_the_copy_carrying_it() {
        let home = Home::new();
        let hub = home.repo("hub");
        let mirror = home.repo("mirror");
        home.configure(&[(1, &hub), (2, &mirror)]);
        let current = contract_entry("rgb_camera", "v1", fs(hub.join("contracts/rgb.json5")));
        let mut older = contract_entry("rgb_camera", "v1", fs(mirror.join("rgb.json5")));
        older.sha256 = fingerprint("older revision");
        let older_sha = older.sha256.as_str().to_owned();
        home.cache(&[current, older]);

        let outcome = search(&home, &format!("rgb_camera:v1@{older_sha}")).expect("searches");
        assert_eq!(outcome.matches.len(), 1);
        assert_eq!(
            outcome.matches[0].published,
            PublishedDoc {
                repo_id: 2,
                repo_label: label(&mirror),
                path: label(&mirror.join("rgb.json5")),
                sha256: ManifestFingerprint::parse(&older_sha).unwrap(),
            }
        );
        assert!(outcome.detail.is_some(), "one identity carries its report");

        let absent = fingerprint("never published");
        let outcome = search(&home, &format!("rgb_camera:v1@{absent}")).expect("searches");
        assert_eq!(outcome.matches, Vec::new());
        assert_eq!(outcome.detail, None);
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
        home.cache(&[contract_entry(
            "rgb_camera",
            "v1",
            fs(hub.join("contracts/rgb.json5")),
        )]);
        home.cache(&[
            only_implements(
                node_entry("cam", "v1", fs(hub.join("cam/peppy.json5"))),
                implements("rgb_camera", "v1", "camera", None),
            ),
            node_entry("cam", "v1", fs(top.join("cam/peppy.json5"))),
        ]);

        let outcome = search(&home, "rgb_camera:v1").expect("searches");
        let report = outcome.detail.expect("one identity carries its report");

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
        home.cache(&[contract_entry(
            "rgb_camera",
            "v1",
            fs(hub.join("contracts/rgb.json5")),
        )]);
        home.cache(&[
            only_implements(
                node_entry("cam", "v1", fs(hub.join("cam/peppy.json5"))),
                implements("rgb_camera", "v1", "camera", None),
            ),
            only_implements(
                node_entry("other_cam", "v1", fs(extra.join("other_cam/peppy.json5"))),
                implements("rgb_camera", "v1", "camera", None),
            ),
        ]);

        let outcome = search(&home, "rgb_camera:v1").expect("searches");

        assert!(
            outcome.excluded_hint.contains("1 excluded repository"),
            "got: {}",
            outcome.excluded_hint
        );
        assert!(
            outcome.excluded_hint.contains(&label(&extra)),
            "got: {}",
            outcome.excluded_hint
        );
        let report = outcome.detail.expect("one identity carries its report");
        let names: Vec<&str> = report
            .implementers
            .iter()
            .map(|hit| hit.node.node_name.as_str())
            .collect();
        assert_eq!(names, vec!["cam"]);
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
        home.cache(&[current, older]);
        let pairing = pairing_entry("rgb_camera", "v1", fs(hub.join("pairings/rgb.json5")));
        let pairing_sha = pairing.sha256.as_str().to_owned();
        home.cache(&[pairing]);
        let absent = fingerprint("never published");
        home.cache(&[with_links(
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

        let outcome = search(&home, "rgb_camera:v1").expect("searches");
        let report = outcome.detail.expect("one identity carries its report");

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

    /// An identity nobody publishes or uses is an empty outcome, not an
    /// error: "nothing matches" is an answer.
    #[test]
    fn an_unknown_identity_reports_nothing() {
        let home = Home::new();
        let hub = home.repo("hub");
        home.configure(&[(1, &hub)]);
        home.cache(&[only_implements(
            node_entry("cam", "v1", fs(hub.join("cam/peppy.json5"))),
            implements("rgb_camera", "v1", "camera", None),
        )]);

        let outcome = search(&home, "nobody:v1").expect("searches");

        assert_eq!(
            outcome,
            SearchOutcome {
                matches: Vec::new(),
                detail: None,
                details: Vec::new(),
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

        let err = search(&home, "rgb_camera:v1").expect_err("refuses");

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
        home.cache(&[
            contract_entry("rgb_camera", "v1", fs(hub.join("a/rgb.json5"))),
            contract_entry("rgb_camera", "v1", fs(hub.join("b/rgb.json5"))),
        ]);

        let err = search(&home, "rgb_camera:v1").expect_err("refuses");

        assert!(err.contains("2 contract entries"), "got: {err}");
        assert!(err.contains("`rgb_camera:v1`"), "got: {err}");
    }

    /// A broad pattern that also matches a contested identity keeps the
    /// other matches: the contested row points at the winning repository's
    /// first copy in path order, and only a query settling on that
    /// identity alone refuses.
    #[test]
    fn a_contested_identity_among_other_matches_is_still_listed() {
        let home = Home::new();
        let hub = home.repo("hub");
        home.configure(&[(1, &hub)]);
        home.cache(&[
            contract_entry("rgb_camera", "v1", fs(hub.join("a/rgb.json5"))),
            contract_entry("rgb_camera", "v1", fs(hub.join("b/rgb.json5"))),
        ]);
        home.cache(&[node_entry(
            "uvc_camera",
            "v1",
            fs(hub.join("uvc_camera/peppy.json5")),
        )]);

        let outcome = search(&home, ".*").expect("searches");

        let identities: Vec<(RepoItemKind, &str)> = outcome
            .matches
            .iter()
            .map(|m| (m.kind, m.name.as_str()))
            .collect();
        assert_eq!(
            identities,
            vec![
                (RepoItemKind::Contract, "rgb_camera"),
                (RepoItemKind::Node, "uvc_camera"),
            ]
        );
        assert_eq!(
            outcome.matches[0].published.path,
            label(&hub.join("a/rgb.json5"))
        );
        assert_eq!(outcome.detail, None);
    }

    /// A pattern that spells a contested identity's name out in full
    /// settles on it, so the search refuses even while the pattern also
    /// brushes other names.
    #[test]
    fn a_query_settling_on_a_contested_identity_refuses() {
        let home = Home::new();
        let hub = home.repo("hub");
        home.configure(&[(1, &hub)]);
        home.cache(&[
            contract_entry("rgb_camera", "v1", fs(hub.join("a/rgb.json5"))),
            contract_entry("rgb_camera", "v1", fs(hub.join("b/rgb.json5"))),
        ]);
        home.cache(&[node_entry(
            "sim_rgb_camera",
            "v1",
            fs(hub.join("sim/peppy.json5")),
        )]);

        let err = search(&home, "rgb_camera:v1").expect_err("refuses");

        assert!(err.contains("2 contract entries"), "got: {err}");
    }

    /// A full search reports every matched identity in match order, the
    /// way `apt search --full` prints every record whole; the settled
    /// report is unaffected.
    #[test]
    fn a_full_search_reports_every_matched_identity() {
        let home = Home::new();
        let hub = home.repo("hub");
        home.configure(&[(1, &hub)]);
        home.cache(&[
            contract_entry("rgb_camera", "v1", fs(hub.join("rgb_v1.json5"))),
            contract_entry("rgb_camera", "v2", fs(hub.join("rgb_v2.json5"))),
        ]);
        home.cache(&[only_implements(
            node_entry("cam", "v1", fs(hub.join("cam/peppy.json5"))),
            implements("rgb_camera", "v1", "camera", None),
        )]);

        let outcome = search_full(&home, "rgb_camera:v[12]").expect("searches");

        let identities: Vec<(&str, &str)> = outcome
            .details
            .iter()
            .map(|report| (report.name.as_str(), report.tag.as_str()))
            .collect();
        assert_eq!(identities, vec![("rgb_camera", "v1"), ("rgb_camera", "v2")]);
        assert_eq!(outcome.details[0].implementers.len(), 1);
        assert_eq!(outcome.details[1].implementers, Vec::new());
        assert_eq!(outcome.detail, None, "two identities never settle");

        let outcome = search(&home, "rgb_camera:v[12]").expect("searches");
        assert_eq!(
            outcome.details,
            Vec::new(),
            "a plain search computes no per-identity reports"
        );
    }

    /// A full search demands a report for every matched identity, so a
    /// contested one refuses even though a plain search would list it.
    #[test]
    fn a_full_search_refuses_a_contested_identity_among_matches() {
        let home = Home::new();
        let hub = home.repo("hub");
        home.configure(&[(1, &hub)]);
        home.cache(&[
            contract_entry("rgb_camera", "v1", fs(hub.join("a/rgb.json5"))),
            contract_entry("rgb_camera", "v1", fs(hub.join("b/rgb.json5"))),
        ]);
        home.cache(&[node_entry(
            "uvc_camera",
            "v1",
            fs(hub.join("uvc_camera/peppy.json5")),
        )]);

        assert!(search(&home, ".*").is_ok(), "a plain search lists it");
        let err = search_full(&home, ".*").expect_err("refuses");
        assert!(err.contains("2 contract entries"), "got: {err}");
    }
}
