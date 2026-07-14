use super::interfaces::action_message_from_exposed;
use crate::services::node::cache as node_cache;
use crate::services::repo::cache as repo_cache;
use core_node_api::encoding::RepoResolvedEntry;
use daemon_config::consts::PeppyDirs;
use generator::ConsumedActionMessage;
use node_stack::NodeStack;
use std::collections::{HashMap, HashSet};

/// Walks `manifest.depends_on.nodes` and BFS-fetches every dependency
/// missing from `node_stack` through the repository cache. Returns
/// `(name, tag) -> NodeConfig` for every materialized dep, a provenance
/// vec for repo-resolved entries, and a `name:tag` list of every dep
/// the BFS found in the node stack (direct or transitive via a
/// repo-cache-materialized parent) so the response can surface them
/// under "Synchronized from node stack:".
///
/// A dep that resolves through `node_stack` is recorded as a stack hit
/// and BFS expansion stops there (the existing resolver tier handles
/// transitive walking via stack entries). A dep that resolves through
/// neither the stack nor any configured repository is a hard failure:
/// the returned `Err` becomes the request's `NodeSyncResponse::failure(...)`
/// payload.
pub(super) async fn materialize_repo_deps(
    manifest: &config::node::Manifest,
    node_stack: &NodeStack,
    peppy_dirs: &PeppyDirs,
) -> std::result::Result<
    (
        HashMap<(String, String), config::node::NodeConfig>,
        Vec<RepoResolvedEntry>,
        Vec<String>,
    ),
    String,
> {
    let mut resolved: HashMap<(String, String), config::node::NodeConfig> = HashMap::new();
    let mut provenance: Vec<RepoResolvedEntry> = Vec::new();
    let mut stack_hits: Vec<String> = Vec::new();

    let Some(deps) = manifest.depends_on.as_ref() else {
        return Ok((resolved, provenance, stack_hits));
    };
    if deps.nodes.is_empty() {
        return Ok((resolved, provenance, stack_hits));
    }

    // Lazy-load the nodes cache: defer the read until the first stack
    // miss so a manifest fully covered by the NodeStack never touches
    // nodes.json5 (and a malformed cache can't fail a sync that
    // wouldn't have used it). Loaded once for the whole BFS so the
    // `mtime`-keyed memo + checkout dedup amortize across deps.
    let mut cache: Option<(
        Vec<repo_cache::NodeCacheEntry>,
        Option<std::time::SystemTime>,
    )> = None;

    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut pending: Vec<(String, String)> = deps
        .nodes
        .iter()
        .map(|n| (n.name.as_str().to_owned(), n.tag.clone()))
        .collect();

    while let Some((name, tag)) = pending.pop() {
        if !seen.insert((name.clone(), tag.clone())) {
            continue;
        }
        // Stack tier wins; record the hit (de-duped via `seen` above)
        // and skip materialization for anything already pushed onto the
        // persistent NodeStack.
        if node_stack.find(&name, &tag).is_some() {
            stack_hits.push(format!("{}:{}", name, tag));
            continue;
        }
        if cache.is_none() {
            cache = Some(
                repo_cache::load_with_generation(peppy_dirs)
                    .map_err(|e| format!("failed to load nodes cache: {e}"))?,
            );
        }
        let (entries, cache_generation) = cache.as_ref().expect("cache loaded above");
        let Some(entry) = repo_cache::lookup(entries, &name, &tag) else {
            return Err(format!(
                "dep `{name}:{tag}` not found in node stack or repository cache; \
                 run `peppy repo refresh`"
            ));
        };
        let entry = entry.clone();
        let source_kind = entry.source_type;
        let (_root_dir, parsed) = node_cache::materialize_entry(
            &entry,
            peppy_dirs,
            *cache_generation,
            node_cache::silent_feedback(),
        )
        .await
        .map_err(|e| format!("failed to materialize {name}:{tag} from repo cache: {e}"))?;

        // Push transitive deps onto the BFS queue, skipping anything we
        // already plan to visit. Stack-tier shadowing happens at pop time.
        if let Some(child_deps) = parsed.manifest.depends_on.as_ref() {
            for child in &child_deps.nodes {
                let key = (child.name.as_str().to_owned(), child.tag.clone());
                if !seen.contains(&key) {
                    pending.push(key);
                }
            }
        }

        resolved.insert((name.clone(), tag.clone()), parsed);
        provenance.push(RepoResolvedEntry {
            name,
            tag,
            source_kind,
        });
    }

    Ok((resolved, provenance, stack_hits))
}

/// Discriminator carried by [`DependencyLookupEntry`] so the resolver knows
/// whether a `link_id` resolves to a `depends_on.nodes` entry (load
/// offerings from the producer node config) or a `depends_on.contracts`
/// entry (load the contract directly from the contract cache).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DependencyKind {
    Node,
    Contract,
}

/// Resolved `(name, tag, kind, cardinality)` for a single dependency
/// referenced by a consumer's `interfaces.topics.consumes`,
/// `interfaces.services.consumes`, or `interfaces.actions.consumes`.
/// `cardinality` rides along so codegen can document each slot's bound-set
/// size; interface shape resolution itself is cardinality-independent.
#[derive(Debug, Clone)]
pub(crate) struct DependencyLookupEntry {
    pub name: String,
    pub tag: String,
    pub sha256: Option<String>,
    pub kind: DependencyKind,
    pub cardinality: config::node::Cardinality,
}

/// Builds a lookup from `link_id` → [`DependencyLookupEntry`] using the
/// node's `depends_on.nodes` and `depends_on.contracts`. Parse-time
/// validation has already guaranteed uniqueness of `link_id` across both
/// lists, so insertion-order is preserved.
pub(super) fn build_dependency_lookup(
    manifest: &config::node::Manifest,
) -> std::collections::HashMap<String, DependencyLookupEntry> {
    let Some(depends_on) = manifest.depends_on.as_ref() else {
        return std::collections::HashMap::new();
    };
    let mut out = std::collections::HashMap::new();
    for node in &depends_on.nodes {
        out.insert(
            node.link_id.clone(),
            DependencyLookupEntry {
                name: node.name.as_str().to_string(),
                tag: node.tag.clone(),
                sha256: None,
                kind: DependencyKind::Node,
                cardinality: node.cardinality,
            },
        );
    }
    for contract in &depends_on.contracts {
        out.insert(
            contract.link_id.clone(),
            DependencyLookupEntry {
                name: contract.name.as_str().to_string(),
                tag: contract.tag.clone(),
                sha256: contract.sha256.clone(),
                kind: DependencyKind::Contract,
                cardinality: contract.cardinality,
            },
        );
    }
    out
}

/// What a single node dependency can provide to consumers: its NATIVE
/// emits/exposes only. Contract-backed entries never enter these tables;
/// the two namespaces cannot overlap, so no precedence rule exists.
/// Contract-backed interfaces are consumable solely through
/// `depends_on.contracts`. Keys are trimmed names, matching the consumer
/// side's `.trim()` comparisons.
pub(super) struct DependencyOfferings {
    pub(super) topics: HashMap<String, config::node::MessageFormat>,
    pub(super) services:
        HashMap<String, (config::node::MessageFormat, config::node::MessageFormat)>,
    pub(super) actions: HashMap<String, ConsumedActionMessage>,
}

pub(super) fn build_dependency_offerings(
    dep_config: &config::node::NodeConfig,
) -> DependencyOfferings {
    let mut topics: HashMap<String, config::node::MessageFormat> = HashMap::new();
    let mut services: HashMap<String, (config::node::MessageFormat, config::node::MessageFormat)> =
        HashMap::new();
    let mut actions: HashMap<String, ConsumedActionMessage> = HashMap::new();

    for emitted in dep_config.interfaces.native_emits() {
        if let Some(fmt) = &emitted.message_format {
            topics
                .entry(emitted.name.trim().to_string())
                .or_insert_with(|| fmt.clone());
        }
    }
    for exposed in dep_config.interfaces.native_service_exposes() {
        services
            .entry(exposed.name.trim().to_string())
            .or_insert_with(|| {
                (
                    exposed.request_message_format.clone().unwrap_or_default(),
                    exposed.response_message_format.clone().unwrap_or_default(),
                )
            });
    }
    for exposed in dep_config.interfaces.native_action_exposes() {
        actions
            .entry(exposed.name.trim().to_string())
            .or_insert_with(|| action_message_from_exposed(exposed));
    }

    DependencyOfferings {
        topics,
        services,
        actions,
    }
}
