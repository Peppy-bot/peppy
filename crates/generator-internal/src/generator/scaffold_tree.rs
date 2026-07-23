//! Shared module-tree used by both the Rust and Python scaffold writers.
//!
//! `InterfaceArtifact::module_path` describes where each generated symbol
//! lives under its category directory. Both languages walk the same tree
//! shape: directories keyed by raw segment, leaves keyed by their final
//! segment. Only the per-file/per-`__init__` rendering differs.

use super::types::{InterfaceArtifact, ModuleCategory};
use crate::error::{Error, Result};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// Groups a flat list of artifacts by their [`ModuleCategory`], preserving
/// per-category insertion order. Shared by both language scaffolds so the Rust
/// and Python writers bucket artifacts identically.
pub(crate) fn group_artifacts_by_category(
    artifacts: Vec<InterfaceArtifact>,
) -> BTreeMap<ModuleCategory, Vec<InterfaceArtifact>> {
    let mut grouped: BTreeMap<ModuleCategory, Vec<InterfaceArtifact>> = BTreeMap::new();
    for artifact in artifacts {
        let category = ModuleCategory::from_kind(artifact.kind);
        grouped.entry(category).or_default().push(artifact);
    }
    grouped
}

/// Recursive tree of artifacts: a directory contains nested subtrees keyed
/// by their next path segment, and at the leaf level a single segment may
/// map to multiple [`InterfaceArtifact`] entries (see [`Self::leaves`]).
#[derive(Default)]
pub(crate) struct ModuleTree {
    /// Sub-directories keyed by their (raw) segment. Sanitized on write.
    pub children: BTreeMap<String, ModuleTree>,
    /// Leaf artifacts produced at this level. Multiple artifacts at the same
    /// leaf segment are allowed (e.g. an action that registers helper symbols);
    /// they are concatenated into one rendered file.
    pub leaves: BTreeMap<String, Vec<InterfaceArtifact>>,
}

pub(crate) fn build_module_tree(artifacts: Vec<InterfaceArtifact>) -> ModuleTree {
    let mut root = ModuleTree::default();
    for artifact in artifacts {
        let path = artifact.module_path.clone();
        insert_into_tree(&mut root, &path, artifact);
    }
    root
}

fn insert_into_tree(node: &mut ModuleTree, path: &[String], artifact: InterfaceArtifact) {
    match path {
        [] => unreachable!("InterfaceArtifact::module_path must not be empty"),
        [leaf] => {
            node.leaves.entry(leaf.clone()).or_default().push(artifact);
        }
        [segment, rest @ ..] => {
            let child = node.children.entry(segment.clone()).or_default();
            insert_into_tree(child, rest, artifact);
        }
    }
}

/// One entry in a directory's module index, in deterministic write order
/// (leaves first, then sub-directories).
pub(crate) struct ModuleEntry {
    /// The sanitized, dedup-suffixed module name: the on-disk file/dir stem.
    pub module_name: String,
    /// The original (raw) segment before sanitization, for languages that emit
    /// a provenance comment when the two differ.
    pub raw_name: String,
    /// Whether this entry is a leaf module (vs a sub-directory).
    pub is_leaf: bool,
}

/// Language-specific rendering for the otherwise-identical module-tree walk.
///
/// The traversal (directory creation, per-level sibling-name de-duplication,
/// and recursion order) lives in [`write_module_tree`]; implementors only
/// decide how a single leaf file and a single directory index file are written.
pub(crate) trait TreeWriter {
    /// Sanitizes a raw path segment into a valid module name for the target
    /// language (escaping reserved keywords, etc.).
    fn sanitize_module_name(raw: &str) -> String;

    /// Writes the leaf module file for `module_name` under `dir`.
    fn write_leaf(
        &mut self,
        dir: &Path,
        module_name: &str,
        raw_name: &str,
        artifacts: &[InterfaceArtifact],
    ) -> Result<()>;

    /// Writes the directory index file (e.g. `mod.rs` / `__init__.py`) listing
    /// `entries`. `is_root` is true for the category's top directory, which
    /// some languages render to a sibling file rather than an in-directory one.
    fn write_index(&mut self, dir: &Path, entries: &[ModuleEntry], is_root: bool) -> Result<()>;
}

/// Walks `tree`, writing one leaf file per leaf and one index file per
/// directory via `writer`. Sibling module names must be unique after
/// per-language sanitization at each level: two raw segments that sanitize to
/// the same name are a hard [`Error::ModuleNameCollision`] naming both, not a
/// silent rename, so a module can never become reachable under a name the
/// author did not write.
pub(crate) fn write_module_tree<W: TreeWriter>(
    dir: &Path,
    tree: &ModuleTree,
    category: ModuleCategory,
    writer: &mut W,
) -> Result<()> {
    write_tree_level(dir, tree, category, writer, true)
}

fn write_tree_level<W: TreeWriter>(
    dir: &Path,
    tree: &ModuleTree,
    category: ModuleCategory,
    writer: &mut W,
    is_root: bool,
) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    // Sanitized module name -> the raw segment that first claimed it, shared
    // across leaves and sub-directories at this level so a leaf and a sibling
    // directory that sanitize alike also collide.
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut entries: Vec<ModuleEntry> = Vec::new();

    // Leaves first, then sub-directories. `BTreeMap` already gives deterministic
    // alphabetical ordering inside each bucket.
    for (raw_leaf, artifacts) in &tree.leaves {
        let module_name =
            claim_module_name(raw_leaf, category, &mut seen, W::sanitize_module_name)?;
        writer.write_leaf(dir, &module_name, raw_leaf, artifacts)?;
        entries.push(ModuleEntry {
            module_name,
            raw_name: raw_leaf.clone(),
            is_leaf: true,
        });
    }

    for (raw_segment, child) in &tree.children {
        let module_name =
            claim_module_name(raw_segment, category, &mut seen, W::sanitize_module_name)?;
        let child_dir = dir.join(&module_name);
        write_tree_level(&child_dir, child, category, writer, false)?;
        entries.push(ModuleEntry {
            module_name,
            raw_name: raw_segment.clone(),
            is_leaf: false,
        });
    }

    writer.write_index(dir, &entries, is_root)
}

/// Sanitizes `raw` for the target language and records the result in `seen`.
/// Returns a hard [`Error::ModuleNameCollision`] if a different raw segment at
/// this level already claimed the same sanitized name.
fn claim_module_name(
    raw: &str,
    category: ModuleCategory,
    seen: &mut HashMap<String, String>,
    sanitize_fn: fn(&str) -> String,
) -> Result<String> {
    let sanitized = sanitize_fn(raw);
    if let Some(first) = seen.get(&sanitized) {
        return Err(Error::ModuleNameCollision {
            category: category.dir_name().to_string(),
            first: first.clone(),
            second: raw.to_string(),
            sanitized,
        });
    }
    seen.insert(sanitized.clone(), raw.to_string());
    Ok(sanitized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::naming::sanitize_component;
    use crate::generator::types::InterfaceKind;
    use tempfile::TempDir;

    /// Minimal writer: sanitizes like the real backends and writes nothing, so
    /// the collision check in `claim_module_name` is exercised in isolation.
    struct NoopWriter;
    impl TreeWriter for NoopWriter {
        fn sanitize_module_name(raw: &str) -> String {
            sanitize_component(raw)
        }
        fn write_leaf(
            &mut self,
            _dir: &Path,
            _module_name: &str,
            _raw_name: &str,
            _artifacts: &[InterfaceArtifact],
        ) -> Result<()> {
            Ok(())
        }
        fn write_index(
            &mut self,
            _dir: &Path,
            _entries: &[ModuleEntry],
            _is_root: bool,
        ) -> Result<()> {
            Ok(())
        }
    }

    fn leaf(path: &[&str]) -> InterfaceArtifact {
        InterfaceArtifact {
            module_path: path.iter().map(|s| s.to_string()).collect(),
            kind: InterfaceKind::ConsumedTopic,
            code_output: "x\n".to_string(),
        }
    }

    #[test]
    fn colliding_slot_directories_are_a_hard_error() {
        // Two slots whose link_ids differ only by separator (`arm-1` vs `arm_1`)
        // sanitize to the same directory name; nesting makes that
        // unrepresentable, so it must be a hard error, not a silent rename.
        let tree = build_module_tree(vec![leaf(&["arm-1", "states"]), leaf(&["arm_1", "states"])]);
        let temp = TempDir::new().unwrap();
        let err = write_module_tree(
            temp.path(),
            &tree,
            ModuleCategory::ConsumedTopics,
            &mut NoopWriter,
        )
        .expect_err("colliding link_ids must be rejected");
        assert!(
            matches!(err, Error::ModuleNameCollision { .. }),
            "expected ModuleNameCollision, got {err:?}"
        );
    }

    #[test]
    fn native_leaf_colliding_with_slot_directory_is_a_hard_error() {
        // A native produced topic named `arm` collides with a slot directory
        // `arm`; the leaf and the sibling directory share one namespace.
        let tree = build_module_tree(vec![leaf(&["arm"]), leaf(&["arm", "states"])]);
        let temp = TempDir::new().unwrap();
        let err = write_module_tree(
            temp.path(),
            &tree,
            ModuleCategory::EmittedTopics,
            &mut NoopWriter,
        )
        .expect_err("a native leaf colliding with a slot directory must be rejected");
        assert!(
            matches!(err, Error::ModuleNameCollision { .. }),
            "expected ModuleNameCollision, got {err:?}"
        );
    }

    #[test]
    fn distinct_slots_and_leaves_write_cleanly() {
        // The happy path: distinct link_ids and a distinct native leaf coexist.
        let tree = build_module_tree(vec![
            leaf(&["video_stream"]),
            leaf(&["front_cam", "frames"]),
            leaf(&["rear_cam", "frames"]),
        ]);
        let temp = TempDir::new().unwrap();
        write_module_tree(
            temp.path(),
            &tree,
            ModuleCategory::EmittedTopics,
            &mut NoopWriter,
        )
        .expect("distinct names must write without collision");
    }
}
