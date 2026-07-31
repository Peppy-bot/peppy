mod parse;
mod types;

pub use core_node_api::encoding::RepoItemKind;

// Defines the repository index (`peppy_schema: "repository/v1"`), the
// `peppy_repository.json5` document at the root of the tree peppy scans.
// A repository states there what it publishes and where each item is
// declared, so resolution reads one file and the paths it names instead of
// discovering identities by walking a directory tree. The nesting is the
// uniqueness rule: a second claim on one identity has nowhere to go except a
// key that is already taken.
pub use parse::PeppyRepositoryIndexParser;
pub use types::{
    DeclaredItem, DeclaredPaths, IndexedItem, ItemName, ItemTag, RepoPathError, RepoRelativePath,
    RepositoryIndex, TaggedSection, UniqueMap,
};
