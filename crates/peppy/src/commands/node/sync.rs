use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use config::consts::NODE_CONFIG_FILE;
use config::node::NodeConfigParser;
use core_node_api::encoding::NodeSyncRequest;
use node_stack::VirtualDeptree;
use tracing::{info, warn};
use walkdir::WalkDir;

use super::source::resolve_node_root_dir;
use crate::commands::CALLER_INSTANCE_ID;
use crate::context::AppContext;
use crate::error::{Error, Result};

use peppylib::core_node::transport::poll_node_sync;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Directory names that should never be descended into while searching for
/// root `peppy.json5` files.
const PRUNED_DIR_NAMES: &[&str] = &[
    ".git",
    ".peppy",
    "target",
    "node_modules",
    ".venv",
    "__pycache__",
];

pub fn sync_node(ctx: &Arc<AppContext>, path: Option<PathBuf>) -> Result<()> {
    crate::commands::block_on(sync_node_async(ctx, path))
}

pub fn sync_all_nodes(ctx: &Arc<AppContext>, path: Option<PathBuf>) -> Result<()> {
    crate::commands::block_on(sync_all_nodes_async(ctx, path))
}

pub(super) async fn sync_node_async(ctx: &Arc<AppContext>, path: Option<PathBuf>) -> Result<()> {
    // If the current directory doesn't contain a valid root config (e.g. we're
    // inside a variant subdirectory), walk up to find the root node directory.
    let base_dir = match path {
        Some(p) => ctx.root_dir.join(p),
        None => ctx.root_dir.clone(),
    };
    let node_root_dir = resolve_node_root_dir(&base_dir)?;
    sync_resolved_node(ctx, &node_root_dir, &[]).await
}

async fn sync_all_nodes_async(ctx: &Arc<AppContext>, path: Option<PathBuf>) -> Result<()> {
    let base_dir = match path {
        Some(p) => ctx.root_dir.join(p),
        None => ctx.root_dir.clone(),
    };

    info!("Searching for nodes under {}...", base_dir.display());
    let roots = find_root_node_dirs(&base_dir);
    if roots.is_empty() {
        return Err(Error::ExecutionFailed(format!(
            "No {} root configs found under '{}'",
            NODE_CONFIG_FILE,
            base_dir.display()
        )));
    }

    // Parse every discovered `peppy.json5` so we can build a virtual dependency
    // tree below. We use `into_resolved_or_default` so that variant-only roots
    // (where execution lives in a child variant config) still parse — only
    // their manifest is needed for dependency analysis.
    let mut parsed: Vec<(PathBuf, config::node::NodeConfig)> = Vec::with_capacity(roots.len());
    for root in &roots {
        let cfg_path = root.join(NODE_CONFIG_FILE);
        let parsed_cfg = NodeConfigParser::from_path(&cfg_path).map_err(|e| {
            Error::ExecutionFailed(format!("Failed to parse {}: {}", cfg_path.display(), e))
        })?;
        parsed.push((root.clone(), parsed_cfg.into_resolved_or_default()));
    }

    // Build the in-memory dependency tree. This is the only place ordering is
    // decided; the daemon never sees the tree itself, only the flat peer list.
    // The tree is dropped at the end of this function.
    let tree = VirtualDeptree::build(parsed).map_err(|e| Error::ExecutionFailed(e.to_string()))?;

    let peer_paths = tree.peer_root_dirs();
    let ordered = tree.topological_order();
    info!(
        "Found {} node(s) to sync (resolved dep order)",
        ordered.len()
    );

    let mut failures: Vec<(PathBuf, Error)> = Vec::new();
    for info in &ordered {
        info!("Syncing node at {}...", info.root_dir.display());
        if let Err(err) = sync_resolved_node(ctx, &info.root_dir, &peer_paths).await {
            warn!(
                "Failed to sync node at {}: {}",
                info.root_dir.display(),
                err
            );
            failures.push((info.root_dir.clone(), err));
        }
    }

    // The `tree` is purely scoped to this function — `ordered` borrows from
    // it, so it stays alive until the function returns and is then dropped.
    let synced = ordered.len() - failures.len();
    info!("Synced {} node(s)", synced);

    if failures.is_empty() {
        Ok(())
    } else {
        let details = failures
            .iter()
            .map(|(dir, err)| format!("  - {}: {}", dir.display(), err))
            .collect::<Vec<_>>()
            .join("\n");
        Err(Error::ExecutionFailed(format!(
            "Failed to sync {} of {} node(s):\n{}",
            failures.len(),
            ordered.len(),
            details
        )))
    }
}

/// Runs the daemon-side `node_generate` service for a node directory that has
/// already been resolved to its canonical root.
///
/// `local_peers` carries sibling node root directories that the daemon should
/// consider when resolving dependencies for this request — used by `node sync
/// -a` to make freshly-discovered peers visible without touching the daemon's
/// persistent node stack. For plain `node sync`, pass `&[]`.
async fn sync_resolved_node(
    ctx: &Arc<AppContext>,
    node_root_dir: &Path,
    local_peers: &[PathBuf],
) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    info!(
        "Syncing node from {} via daemon '{}'...",
        node_root_dir.display(),
        conn.core_node_name
    );

    let request = NodeSyncRequest::new(
        node_root_dir.to_path_buf(),
        conn.git_hash,
        local_peers.to_vec(),
    );
    let response = poll_node_sync(
        &request,
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        &conn.core_node_name,
        REQUEST_TIMEOUT,
    )
    .await
    .map_err(|e| Error::ExecutionFailed(format!("Failed to call node_generate service: {}", e)))?;

    if !response.success {
        let msg = if response.error_message.trim().is_empty() {
            "node_generate failed with no error message".to_string()
        } else {
            response.error_message
        };
        return Err(Error::ExecutionFailed(msg));
    }

    info!("Synced node interfaces at {}", node_root_dir.display());
    Ok(())
}

/// Recursively finds every root node directory under `base`.
///
/// A "root node directory" is any directory whose `peppy.json5` is classified
/// as a root by [`resolve_node_root_dir`]. Variant subdirectory configs are
/// deduped back to their owning root. Malformed configs are skipped with a
/// warning so that a single bad file does not abort the whole search.
fn find_root_node_dirs(base: &Path) -> Vec<PathBuf> {
    let mut roots: BTreeSet<PathBuf> = BTreeSet::new();

    let walker = WalkDir::new(base)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if !entry.file_type().is_dir() {
                return true;
            }
            // Always include the starting directory.
            if entry.depth() == 0 {
                return true;
            }
            let name = entry.file_name().to_string_lossy();
            !PRUNED_DIR_NAMES.iter().any(|pruned| name == *pruned)
        });

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                warn!(
                    "Skipping directory entry while searching for nodes: {}",
                    err
                );
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.file_name() != NODE_CONFIG_FILE {
            continue;
        }
        let Some(parent) = entry.path().parent() else {
            continue;
        };
        match resolve_node_root_dir(parent) {
            Ok(root) => {
                roots.insert(root);
            }
            Err(err) => {
                warn!(
                    "Skipping {} at {}: {}",
                    NODE_CONFIG_FILE,
                    parent.display(),
                    err
                );
            }
        }
    }

    roots.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::consts::NODE_CONFIG_FILE;

    fn write_root_config(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(NODE_CONFIG_FILE),
            format!(
                r#"{{
                    schema_version: 1,
                    manifest: {{
                        name: "{name}",
                        tag: "0.1.0",
                    }},
                    execution: {{ language: "rust", run_cmd: ["./bin"] }}
                }}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn find_root_node_dirs_finds_nested_nodes() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();

        write_root_config(&base.join("node_a"), "node_a");
        write_root_config(&base.join("subdir").join("node_b"), "node_b");
        write_root_config(&base.join("deep").join("nested").join("node_c"), "node_c");

        let roots = find_root_node_dirs(base);
        assert_eq!(roots.len(), 3, "expected 3 roots, got {:?}", roots);
        assert!(roots.contains(&base.join("node_a")));
        assert!(roots.contains(&base.join("subdir").join("node_b")));
        assert!(roots.contains(&base.join("deep").join("nested").join("node_c")));
    }

    #[test]
    fn find_root_node_dirs_skips_pruned_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();

        write_root_config(&base.join("node_a"), "node_a");
        // These should be ignored by the walker pruning.
        write_root_config(&base.join("target").join("ghost"), "ghost");
        write_root_config(&base.join(".git").join("ghost"), "ghost");
        write_root_config(&base.join(".peppy").join("ghost"), "ghost");

        let roots = find_root_node_dirs(base);
        assert_eq!(roots, vec![base.join("node_a")]);
    }

    #[test]
    fn find_root_node_dirs_dedupes_variant_to_root() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let root = base.join("my_node");
        let variant = root.join("variants").join("linux");
        std::fs::create_dir_all(&variant).unwrap();

        // Root with variants
        std::fs::write(
            root.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                manifest: {
                    name: "my_node",
                    tag: "0.1.0",
                    variants: [
                        { name: "default", source: { local: "./variants/linux" } }
                    ]
                },
                interfaces: {}
            }"#,
        )
        .unwrap();

        // Variant config (no manifest)
        std::fs::write(
            variant.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                execution: { language: "rust", run_cmd: ["sleep", "1"] }
            }"#,
        )
        .unwrap();

        let roots = find_root_node_dirs(base);
        assert_eq!(roots, vec![root]);
    }
}
