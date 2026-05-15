//! Shared module-tree used by both the Rust and Python scaffold writers.
//!
//! `InterfaceArtifact::module_path` describes where each generated symbol
//! lives under its category directory. Both languages walk the same tree
//! shape — directories keyed by raw segment, leaves keyed by their final
//! segment. Only the per-file/per-`__init__` rendering differs.

use super::types::InterfaceArtifact;
use std::collections::BTreeMap;

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
