mod origin;
mod parse;
mod pins;
mod types;

pub use core_node_api::encoding::RepoItemKind;

// Where an item's bytes live and which revision they were read at, shared by
// the repo cache entries and the launch pins so the two cannot drift.
pub use origin::EntryOrigin;
// Defines the repository index (`peppy_schema: "repository/v1"`), the
// `peppy_repository.json5` document at the root of the tree peppy scans.
// A repository states there what it publishes and where each item is
// declared, and `peppy repo index` writes that statement and checks it back
// against the tree. The nesting is the uniqueness rule: a second claim on one
// identity has nowhere to go except a key that is already taken.
pub use parse::PeppyRepositoryIndexParser;
// The pin model a launch coordinator ships to every daemon in a launch: the
// fingerprint of the bytes to run plus the origin any machine can read them
// from. Decoding a pin is the receiving side's structural validation.
pub use pins::{DeploymentPins, PinKind, PinnedItem};
pub use types::{
    DeclaredItem, DeclaredPaths, GitCommit, GitCommitError, IndexedItem, ItemName, ItemTag,
    ManifestFingerprint, ManifestFingerprintError, RepoPathError, RepoRelativePath,
    RepositoryIndex, TaggedSection, UniqueMap,
};
