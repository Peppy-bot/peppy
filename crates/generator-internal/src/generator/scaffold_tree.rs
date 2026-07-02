//! Shared module-tree used by both the Rust and Python scaffold writers.
//!
//! `InterfaceArtifact::module_path` describes where each generated symbol
//! lives under its category directory. Both languages walk the same tree
//! shape: directories keyed by raw segment, leaves keyed by their final
//! segment. Only the per-file/per-`__init__` rendering differs.

use super::naming::unique_module_name;
use super::types::{InterfaceArtifact, ModuleCategory};
use crate::error::Result;
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
/// directory via `writer`. Sibling module names are de-duplicated independently
/// at each level (via [`unique_module_name`]) so a leaf `foo` can coexist with a
/// child directory of the same name.
pub(crate) fn write_module_tree<W: TreeWriter>(
    dir: &Path,
    tree: &ModuleTree,
    writer: &mut W,
) -> Result<()> {
    write_tree_level(dir, tree, writer, true)
}

fn write_tree_level<W: TreeWriter>(
    dir: &Path,
    tree: &ModuleTree,
    writer: &mut W,
    is_root: bool,
) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut entries: Vec<ModuleEntry> = Vec::new();

    // Leaves first, then sub-directories. `BTreeMap` already gives deterministic
    // alphabetical ordering inside each bucket.
    for (raw_leaf, artifacts) in &tree.leaves {
        let module_name = unique_module_name(raw_leaf, &mut counts, W::sanitize_module_name);
        writer.write_leaf(dir, &module_name, raw_leaf, artifacts)?;
        entries.push(ModuleEntry {
            module_name,
            raw_name: raw_leaf.clone(),
            is_leaf: true,
        });
    }

    for (raw_segment, child) in &tree.children {
        let module_name = unique_module_name(raw_segment, &mut counts, W::sanitize_module_name);
        let child_dir = dir.join(&module_name);
        write_tree_level(&child_dir, child, writer, false)?;
        entries.push(ModuleEntry {
            module_name,
            raw_name: raw_segment.clone(),
            is_leaf: false,
        });
    }

    writer.write_index(dir, &entries, is_root)
}
