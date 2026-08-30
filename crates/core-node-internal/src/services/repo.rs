mod add;
pub(crate) mod cache;
mod exclude;
pub(crate) mod index;
mod init;
mod list;
mod refresh;
mod remove;
mod search;
pub(crate) mod status;

pub use add::listen_for_repo_add;
pub use exclude::listen_for_repo_exclude;
pub use init::{InitOutcome, ensure_default_repos};
pub use list::listen_for_repo_list;
pub use refresh::listen_for_repo_refresh;
pub use remove::listen_for_repo_remove;
pub use search::{
    Consumer, Implementer, IndexedNode, MatchedItem, Observer, Participant, PinStatus,
    PublishedDoc, SearchOutcome, SearchQuery, SearchReport, search_repo_items,
};

use crate::services::repo::cache::{EntryOrigin, NodeCacheEntry, UNOWNED_REPO_ID};
use crate::services::repo::exclude::ExclusionSet;
use crate::services::repo::refresh::{parse_repo_entry, read_or_create_repos};
use core_node_api::encoding::RepoSource;
use daemon_config::consts::PeppyDirs;
use serde_json::Value;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

/// Guards read-modify-write cycles on repositories.json5 and
/// excluded_repositories.json5 to prevent concurrent corruption.
pub(crate) fn repos_file_lock() -> &'static parking_lot::Mutex<()> {
    static LOCK: std::sync::OnceLock<parking_lot::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| parking_lot::Mutex::new(()))
}

/// Serializes `process_refresh` + `write_all_caches` so the user-facing repo_refresh
/// action and the post-remove refresh in repo_remove cannot race on
/// nodes.json5. The ActionState single-flight inside repo_refresh rejects
/// concurrent *user* refreshes with a friendly error; this mutex is the
/// correctness backstop that also covers the remove-triggered path.
pub(crate) fn refresh_lock() -> &'static parking_lot::Mutex<()> {
    static LOCK: std::sync::OnceLock<parking_lot::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| parking_lot::Mutex::new(()))
}

/// Serialize a `RepoSource` with an assigned id into a JSON object for
/// persisting in repositories.json5 / excluded_repositories.json5.
pub(crate) fn repo_source_to_json(id: u64, source: &RepoSource) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("id".to_string(), Value::Number(id.into()));
    match source {
        RepoSource::Fs(path) => {
            map.insert("type".to_string(), Value::String("fs".to_string()));
            map.insert(
                "path".to_string(),
                Value::String(path.to_string_lossy().into_owned()),
            );
        }
        RepoSource::Git { repo_url, repo_ref } => {
            map.insert("type".to_string(), Value::String("git".to_string()));
            map.insert("url".to_string(), Value::String(repo_url.clone()));
            if let Some(r) = repo_ref {
                map.insert("ref".to_string(), Value::String(r.to_string()));
            }
        }
    }
    Value::Object(map)
}

/// The canonical identity string for a [`RepoSource`], used for duplicate
/// detection and exclusion matching. Lives here (the daemon) rather than in
/// `core-node-api`: the `Fs` arm canonicalizes against the real filesystem,
/// which a pure wire-codec crate should not do.
///
/// - `Fs`: canonicalized (absolute, symlink-resolved) when possible, so that
///   `./repo` and `/abs/path/to/repo` produce the same identity. Falls back to
///   the raw string when the path does not exist.
/// - `Git`: `repo_url@repo_ref` when a non-empty ref is present, otherwise just
///   the url, so the same repo pinned to different refs is not collapsed.
///
/// Must stay in sync with [`json_entry_identity`], the JSON-entry equivalent.
pub(crate) fn source_identity(source: &RepoSource) -> String {
    match source {
        RepoSource::Fs(path) => std::fs::canonicalize(path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string_lossy().into_owned()),
        RepoSource::Git { repo_url, repo_ref } => match repo_ref {
            Some(r) if !r.is_empty() => format!("{repo_url}@{r}"),
            _ => repo_url.clone(),
        },
    }
}

/// Returns the canonical identity for a persisted JSON repo entry.
///
/// Must stay in sync with [`source_identity`]:
/// - `fs`: canonicalized path when possible (falls back to raw string).
/// - `git`: `url@ref` when a non-empty `ref` field is present, otherwise `url`.
pub(crate) fn json_entry_identity(entry: &Value) -> Option<String> {
    let typ = entry.get("type")?.as_str()?;
    match typ {
        "fs" => {
            let path = entry.get("path")?.as_str()?;
            let canonical = std::fs::canonicalize(path)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| path.to_string());
            Some(canonical)
        }
        "git" => {
            let url = entry.get("url")?.as_str()?;
            match entry.get("ref").and_then(|v| v.as_str()) {
                Some(r) if !r.is_empty() => Some(format!("{url}@{r}")),
                _ => Some(url.to_string()),
            }
        }
        _ => None,
    }
}

/// The configured repositories, resolved once so that attributing a whole
/// cache to them costs one pass over the list per entry rather than one
/// `canonicalize` per (entry, repository) pair.
///
/// The one definition of "this entry belongs to that repository", so
/// `repo_id` tagging at load time, retention of a failed repository's
/// previous entries, and `repo list` grouping cannot drift apart. Every
/// caller asks the same question of the same handful of repositories for
/// every entry it holds, which is why resolving them is worth doing up
/// front: `repo refresh` alone asks it four times per repository.
pub(crate) struct RepoOwners(Vec<(Option<u64>, RepoOwner)>);

/// One configured repository, in the form attribution needs it.
enum RepoOwner {
    /// A tree on this machine, rooted at a canonicalized path.
    ///
    /// Cache entries store their paths canonicalized, so on macOS an entry
    /// under a `/var/...` temp or symlinked directory is spelled
    /// `/private/var/...`. The root arrives from `repositories.json5`
    /// exactly as the user wrote it, so it is canonicalized here before any
    /// containment test; without that the two never share a prefix on such
    /// a directory and every entry looks unowned. A root that cannot be
    /// resolved (removed, or momentarily unreachable) keeps its written
    /// spelling, which still matches on platforms that do not put the tree
    /// behind a symlink.
    Fs { root: PathBuf },
    /// A remote, optionally pinned to a ref.
    Git {
        url: String,
        /// `None` for a repository that follows whatever branch it is
        /// given, which therefore matches any ref on its url.
        pinned_ref: Option<String>,
    },
}

impl RepoOwners {
    /// Resolves the `repositories.json5` entries in `repos`. Entries that
    /// describe no repository this machine can match against — an
    /// unrecognized `type`, an `fs` without a `path` — are dropped, since
    /// nothing could ever attribute to them.
    pub(crate) fn new(repos: &[Value]) -> Self {
        let field = |repo: &Value, key: &str| {
            repo.get(key)
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned)
        };
        Self(
            repos
                .iter()
                .filter_map(|repo| {
                    let owner = match repo.get("type").and_then(|v| v.as_str())? {
                        "fs" => RepoOwner::Fs {
                            root: canonical_root(&field(repo, "path")?),
                        },
                        "git" => RepoOwner::Git {
                            url: field(repo, "url")?,
                            pinned_ref: field(repo, "ref").filter(|r| !r.is_empty()),
                        },
                        _ => return None,
                    };
                    Some((repo.get("id").and_then(|v| v.as_u64()), owner))
                })
                .collect(),
        )
    }

    /// Id of the repository that owns this cache entry: the first match in
    /// id order, which (since `repos` arrives sorted by id) is the
    /// highest-priority repository that could have produced it. `None` when
    /// no configured repository matches.
    ///
    /// Nested fs repositories can both contain an entry; first-match makes
    /// exactly one of them its owner, so retention and `repo_id` tagging
    /// never double-count.
    pub(crate) fn owner_of(&self, origin: &EntryOrigin) -> Option<u64> {
        self.0
            .iter()
            .find(|(_, owner)| owner.owns(origin))
            .and_then(|(id, _)| *id)
    }
}

impl RepoOwner {
    fn owns(&self, origin: &EntryOrigin) -> bool {
        match (self, origin) {
            (RepoOwner::Fs { root }, EntryOrigin::Fs { path }) => path.starts_with(root),
            (
                RepoOwner::Git { url, pinned_ref },
                EntryOrigin::Git {
                    repo_url, repo_ref, ..
                },
            ) => {
                // The ref check matters: without it, two entries for one url
                // on different refs both attribute to the lower id and read
                // as one repository claiming an identity twice.
                url == repo_url
                    && pinned_ref
                        .as_deref()
                        .is_none_or(|pinned| repo_ref.as_deref() == Some(pinned))
            }
            _ => false,
        }
    }
}

fn canonical_root(configured: &str) -> PathBuf {
    std::fs::canonicalize(configured).unwrap_or_else(|_| PathBuf::from(configured))
}

/// One configured repository as `repo list` and a search show it.
pub(crate) struct ListedRepo {
    pub id: u32,
    /// [`RepoSource::display_label`]: the path of an fs repository, the url
    /// and ref of a git one.
    pub label: String,
    pub source: RepoSource,
}

/// The configured repositories in id order, minus the excluded ones and
/// any whose id no cache entry can be attributed to (above the wire's
/// `u32`, or the id reserved for unowned entries). What `repo list` groups
/// nodes under and what a search attributes hits to, so the two cannot
/// disagree about which repositories exist.
pub(crate) fn listed_repositories(peppy_dirs: &PeppyDirs) -> crate::Result<Vec<ListedRepo>> {
    let repos = read_or_create_repos(peppy_dirs)?;
    let exclusions = ExclusionSet::load(peppy_dirs);
    let mut listed = Vec::new();
    for entry in &repos {
        let Some(source) = parse_repo_entry(entry) else {
            warn!("Skipping unrecognized repository entry: {:?}", entry);
            continue;
        };

        let identity = source_identity(&source);
        if exclusions.is_excluded(&identity) {
            debug!("Excluding repository from list: {}", identity);
            continue;
        }

        let repo_id_u64 = entry.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        let id = match u32::try_from(repo_id_u64) {
            Ok(id) if id != UNOWNED_REPO_ID => id,
            _ => {
                warn!(
                    "Skipping repository entry with id {} (not a repository id cache entries \
                     can be attributed to)",
                    repo_id_u64
                );
                continue;
            }
        };

        listed.push(ListedRepo {
            id,
            label: source.display_label(),
            source,
        });
    }
    Ok(listed)
}

/// A cached node under the repository that owns it.
pub(crate) struct AttributedNode<'a> {
    pub entry: &'a NodeCacheEntry,
    /// The lower-id repository that already provides this `name:tag`, so a
    /// plain lookup resolves there rather than here.
    pub shadowed_by: Option<&'a ListedRepo>,
}

/// One listed repository with the cached nodes it owns.
pub(crate) struct RepoNodes<'a> {
    pub repo: &'a ListedRepo,
    pub nodes: Vec<AttributedNode<'a>>,
}

/// Each listed repository with the cached nodes it owns, in the order
/// `repos` lists them (id order), each node in cache order. `shadowed_by`
/// names the earlier repository that already provides a node's `name:tag`.
///
/// The single definition of what `repo list` shows and what a search
/// filters. Ownership is the `repo_id` the cache load stamped on each
/// entry, so an entry no listed repository owns (its repository was removed
/// or excluded since the cache was written) appears under none.
pub(crate) fn nodes_by_repository<'a>(
    repos: &'a [ListedRepo],
    cached: &'a [NodeCacheEntry],
) -> Vec<RepoNodes<'a>> {
    let mut winners: HashMap<(&str, &str), &ListedRepo> = HashMap::new();
    repos
        .iter()
        .map(|repo| {
            let nodes = cached
                .iter()
                .filter(|node| node.repo_id == repo.id)
                .map(|entry| {
                    let key = (entry.node_name.as_str(), entry.node_tag.as_str());
                    let shadowed_by = match winners.entry(key) {
                        Entry::Occupied(winner) => Some(*winner.get()),
                        Entry::Vacant(vacant) => {
                            vacant.insert(repo);
                            None
                        }
                    };
                    AttributedNode { entry, shadowed_by }
                })
                .collect();
            RepoNodes { repo, nodes }
        })
        .collect()
}

/// Normalize a list of repo JSON entries: auto-assign missing `id` fields,
/// detect duplicate ids, sort by id, and write back if any ids were assigned.
pub(crate) fn normalize_repo_entries(
    repos: &mut Vec<Value>,
    file_path: &Path,
    desc: &str,
) -> crate::Result<()> {
    let mut max_id: u64 = repos
        .iter()
        .filter_map(|e| e.get("id").and_then(|v| v.as_u64()))
        .max()
        .unwrap_or(0);

    let mut needs_write = false;
    for entry in repos.iter_mut() {
        if entry.get("id").and_then(|v| v.as_u64()).is_none() {
            max_id += 1;
            if let Some(obj) = entry.as_object_mut() {
                obj.insert("id".to_string(), Value::Number(max_id.into()));
                needs_write = true;
            }
        }
    }

    // Detect duplicate ids; a user may manually edit the file and introduce collisions.
    let mut seen_ids = HashSet::new();
    for entry in repos.iter() {
        if let Some(id) = entry.get("id").and_then(|v| v.as_u64())
            && !seen_ids.insert(id)
        {
            let file = file_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| file_path.to_string_lossy().into_owned());
            return Err(crate::Error::DuplicateRepoId { id, file });
        }
    }

    let ids_before: Vec<u64> = repos
        .iter()
        .map(|e| e.get("id").and_then(|v| v.as_u64()).unwrap_or(0))
        .collect();
    repos.sort_by_key(|e| e.get("id").and_then(|v| v.as_u64()).unwrap_or(0));
    let ids_after: Vec<u64> = repos
        .iter()
        .map(|e| e.get("id").and_then(|v| v.as_u64()).unwrap_or(0))
        .collect();
    if ids_before != ids_after {
        needs_write = true;
    }

    if needs_write {
        let content = json5_pretty::to_string_pretty(repos).map_err(|e| {
            core_node_api::Error::Encoding(format!("failed to serialize {desc}: {e}"))
        })?;
        std::fs::write(file_path, content)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    // Coverage for `source_identity` (relocated here from `core-node-api`
    // alongside the function, which moved out of the pure wire-codec crate
    // because its `Fs` arm canonicalizes against the real filesystem).
    use super::{
        ListedRepo, RepoOwners, listed_repositories, nodes_by_repository, source_identity,
    };
    use crate::services::repo::cache::test_support::{node_entry, owned_by};
    use crate::services::repo::cache::{EntryOrigin, UNOWNED_REPO_ID, repositories_list_path};
    use core_node_api::encoding::RepoSource;
    use daemon_config::consts::PeppyDirs;
    use serde_json::Value;
    use std::path::PathBuf;

    fn listed(id: u32, path: &str) -> ListedRepo {
        let source = RepoSource::Fs(PathBuf::from(path));
        ListedRepo {
            id,
            label: source.display_label(),
            source,
        }
    }

    /// The `(repo id, node name, shadowing repo id)` triples
    /// `nodes_by_repository` yields, in order, which is the whole of what a
    /// listing shows.
    fn attribution(
        repos: &[ListedRepo],
        cached: &[crate::services::repo::cache::NodeCacheEntry],
    ) -> Vec<(u32, String, Option<u32>)> {
        nodes_by_repository(repos, cached)
            .iter()
            .flat_map(|group| {
                group.nodes.iter().map(|node| {
                    (
                        group.repo.id,
                        node.entry.node_name.to_string(),
                        node.shadowed_by.map(|winner| winner.id),
                    )
                })
            })
            .collect()
    }

    /// An identity two repositories provide is shadowed in the higher-id
    /// one, whatever order the cache lists the entries in.
    #[test]
    fn a_higher_id_repository_is_shadowed_on_a_shared_identity() {
        let repos = [listed(1, "/top"), listed(2, "/hub")];
        let cached = [
            owned_by(
                node_entry(
                    "cam",
                    "v1",
                    EntryOrigin::Fs {
                        path: "/hub/cam/peppy.json5".into(),
                    },
                ),
                2,
            ),
            owned_by(
                node_entry(
                    "cam",
                    "v1",
                    EntryOrigin::Fs {
                        path: "/top/cam/peppy.json5".into(),
                    },
                ),
                1,
            ),
            owned_by(
                node_entry(
                    "lidar",
                    "v1",
                    EntryOrigin::Fs {
                        path: "/hub/lidar/peppy.json5".into(),
                    },
                ),
                2,
            ),
        ];

        assert_eq!(
            attribution(&repos, &cached),
            vec![
                (1, "cam".to_owned(), None),
                (2, "cam".to_owned(), Some(1)),
                (2, "lidar".to_owned(), None),
            ]
        );
    }

    /// An entry no listed repository owns is shown under none: its
    /// repository was removed or excluded after the cache was written.
    #[test]
    fn an_unowned_entry_appears_under_no_repository() {
        let repos = [listed(1, "/hub")];
        let cached = [owned_by(
            node_entry(
                "ghost",
                "v1",
                EntryOrigin::Fs {
                    path: "/gone/ghost/peppy.json5".into(),
                },
            ),
            UNOWNED_REPO_ID,
        )];

        assert!(attribution(&repos, &cached).is_empty());
    }

    /// A repository that owns nothing is still listed: an empty group is
    /// the answer to "what does this repository provide".
    #[test]
    fn a_repository_with_no_nodes_is_still_listed() {
        let repos = [listed(1, "/empty")];
        let groups = nodes_by_repository(&repos, &[]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].repo.id, 1);
        assert!(groups[0].nodes.is_empty());
    }

    /// The configured repositories come back in id order, without the
    /// excluded ones and without an id cache entries cannot carry.
    #[test]
    fn listed_repositories_drop_excluded_and_unattributable_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let kept = tmp.path().join("kept");
        let excluded = tmp.path().join("excluded");
        std::fs::create_dir_all(&kept).unwrap();
        std::fs::create_dir_all(&excluded).unwrap();
        std::fs::create_dir_all(peppy_dirs.conf_dir()).unwrap();
        std::fs::write(
            repositories_list_path(&peppy_dirs),
            serde_json::to_string(&serde_json::json!([
                { "id": 7, "type": "fs", "path": kept.to_string_lossy() },
                { "id": 3, "type": "fs", "path": excluded.to_string_lossy() },
                { "id": u64::from(UNOWNED_REPO_ID), "type": "git", "url": "https://example.com/unowned.git" },
                { "id": u64::from(u32::MAX) + 1, "type": "git", "url": "https://example.com/wide.git" },
                { "id": 9, "type": "mystery" },
            ]))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            peppy_dirs.conf_dir().join("excluded_repositories.json5"),
            serde_json::to_string(&serde_json::json!([
                { "id": 1, "type": "fs", "path": excluded.to_string_lossy() },
            ]))
            .unwrap(),
        )
        .unwrap();

        let listed = listed_repositories(&peppy_dirs).expect("the configuration is readable");

        let ids: Vec<u32> = listed.iter().map(|repo| repo.id).collect();
        assert_eq!(ids, vec![7]);
        assert_eq!(listed[0].label, kept.to_string_lossy());
        assert!(matches!(listed[0].source, RepoSource::Fs(_)));
    }

    #[test]
    fn identity_git_distinguishes_refs() {
        let a = RepoSource::Git {
            repo_url: "https://github.com/org/repo".to_string(),
            repo_ref: Some("main".to_string()),
        };
        let b = RepoSource::Git {
            repo_url: "https://github.com/org/repo".to_string(),
            repo_ref: Some("dev".to_string()),
        };
        assert_ne!(source_identity(&a), source_identity(&b));
        assert!(source_identity(&a).contains("main"));
        assert!(source_identity(&b).contains("dev"));
    }

    #[test]
    fn identity_git_without_ref_matches_url() {
        let src = RepoSource::Git {
            repo_url: "https://github.com/org/repo".to_string(),
            repo_ref: None,
        };
        assert_eq!(source_identity(&src), "https://github.com/org/repo");
    }

    #[test]
    fn identity_git_empty_ref_matches_url() {
        // Treat empty ref as "no ref" so it matches legacy entries without a ref.
        let src = RepoSource::Git {
            repo_url: "https://github.com/org/repo".to_string(),
            repo_ref: Some(String::new()),
        };
        assert_eq!(source_identity(&src), "https://github.com/org/repo");
    }

    #[test]
    fn identity_fs_canonicalizes_existing_path() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(tmp.path()).unwrap();

        // Build a non-canonical spelling: `<canonical>/../<basename>`.
        let parent = canonical.parent().unwrap();
        let name = canonical.file_name().unwrap();
        let roundabout = parent
            .join("..")
            .join(parent.file_name().unwrap())
            .join(name);

        let raw = RepoSource::Fs(roundabout);
        let canon = RepoSource::Fs(canonical);
        assert_eq!(
            source_identity(&raw),
            source_identity(&canon),
            "canonicalization must collapse equivalent paths"
        );
    }

    #[test]
    fn identity_fs_nonexistent_falls_back_to_raw() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp
            .path()
            .join("definitely")
            .join("does-not-exist")
            .join("xyz");
        let src = RepoSource::Fs(missing.clone());
        assert_eq!(
            source_identity(&src),
            missing.to_string_lossy().into_owned()
        );
    }

    /// A git origin on the shared test url at `repo_ref`.
    fn git_origin(repo_ref: &str) -> EntryOrigin {
        EntryOrigin::Git {
            repo_url: "https://example.com/hub.git".to_owned(),
            repo_ref: Some(repo_ref.to_owned()),
            commit: daemon_config::repository::GitCommit::parse(&"a".repeat(40)).unwrap(),
            path: daemon_config::repository::RepoRelativePath::parse("node/peppy.json5").unwrap(),
        }
    }

    fn fs_origin(path: impl Into<std::path::PathBuf>) -> EntryOrigin {
        EntryOrigin::Fs { path: path.into() }
    }

    fn git_repo(url: &str, git_ref: Option<&str>) -> Value {
        let mut map = serde_json::Map::new();
        map.insert("type".into(), Value::String("git".into()));
        map.insert("url".into(), Value::String(url.into()));
        if let Some(r) = git_ref {
            map.insert("ref".into(), Value::String(r.into()));
        }
        Value::Object(map)
    }

    /// Whether `repo` alone owns `origin`, which is what every attribution
    /// rule below is really asking. The id is supplied here because
    /// `normalize_repo_entries` guarantees one by the time attribution runs.
    fn owns(repo: &Value, origin: &EntryOrigin) -> bool {
        let mut repo = repo.clone();
        repo["id"] = Value::from(1);
        RepoOwners::new(std::slice::from_ref(&repo)).owner_of(origin) == Some(1)
    }

    /// Two repositories on one url pinned to different refs are different
    /// repositories. Attributing by url alone would give both the lower
    /// id, which makes their entries look like one repository claiming an
    /// identity twice.
    #[test]
    fn git_attribution_distinguishes_pinned_refs() {
        let main = git_repo("https://example.com/hub.git", Some("main"));
        let dev = git_repo("https://example.com/hub.git", Some("dev"));

        assert!(owns(&main, &git_origin("main")));
        assert!(!owns(&main, &git_origin("dev")));
        assert!(owns(&dev, &git_origin("dev")));
    }

    /// An unpinned repository takes whatever branch was checked out, so
    /// it matches any resolved ref on its url.
    #[test]
    fn git_attribution_unpinned_repo_matches_any_ref() {
        let unpinned = git_repo("https://example.com/hub.git", None);

        assert!(owns(&unpinned, &git_origin("some-branch")));

        let EntryOrigin::Git {
            repo_ref,
            commit,
            path,
            ..
        } = git_origin("main")
        else {
            unreachable!("git_origin builds a git origin")
        };
        let other = EntryOrigin::Git {
            repo_url: "https://example.com/other.git".to_owned(),
            repo_ref,
            commit,
            path,
        };
        assert!(
            !owns(&unpinned, &other),
            "a different url is a different repository"
        );
    }

    /// An fs entry belongs to the repository whose directory contains it,
    /// and to no other.
    #[test]
    fn fs_attribution_matches_by_containment() {
        let repo = serde_json::json!({ "type": "fs", "path": "/home/user/workspace" });

        assert!(owns(
            &repo,
            &fs_origin("/home/user/workspace/arm/peppy.json5")
        ));
        assert!(!owns(
            &repo,
            &fs_origin("/home/user/elsewhere/arm/peppy.json5")
        ));
    }

    /// A repository configured through a symlinked root still owns the entries
    /// discovered under it. The walk stores canonicalized entry paths, so the
    /// configured root has to be canonicalized before the containment test or
    /// the two never share a prefix. macOS hits this because a `/var` temp dir
    /// is really `/private/var`; the test creates its own symlink so the case
    /// is exercised deterministically on every platform, not just where the
    /// host's tmpdir happens to be symlinked.
    #[cfg(unix)]
    #[test]
    fn fs_attribution_matches_through_a_symlinked_root() {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let real_root = base.join("real");
        std::fs::create_dir_all(real_root.join("arm")).unwrap();

        let link_root = base.join("link");
        std::os::unix::fs::symlink(&real_root, &link_root).unwrap();

        // The entry path is canonical (what the walk emits); the configured
        // root is the symlink spelling (what the user wrote).
        let entry = real_root.join("arm/peppy.json5");
        let repo = serde_json::json!({ "type": "fs", "path": link_root.to_str().unwrap() });

        assert!(
            owns(&repo, &fs_origin(entry)),
            "an entry under the symlink target belongs to the repo configured by the symlink path"
        );
    }

    /// Source kinds never cross-attribute, even when the other fields
    /// would otherwise line up.
    #[test]
    fn attribution_requires_matching_source_kind() {
        let git = git_repo("https://example.com/hub.git", None);
        assert!(!owns(
            &git,
            &fs_origin("/home/user/workspace/arm/peppy.json5")
        ));

        let fs = serde_json::json!({ "type": "fs", "path": "/home/user/workspace" });
        assert!(!owns(&fs, &git_origin("main")));
    }
}
