#![forbid(unsafe_code)]
//! The core node is a special kind of node that has access to the whole context of the peppy daemon and runs as part of the same process

mod error;
mod services;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use error::{Error, Result};
pub use services::mcp::{
    ExposureFinding, PeppyExecutable, check_repository_exposures, derive_exposure_catalog,
    resolve_exposure_plan, resolve_peppy_executable,
};
pub use services::repo::cache::{
    ContractSlot, DeclaredLinks, EntryOrigin, ImplementsClaim, NodeCacheEntry, ObserverSlot,
    PairingSlot, contracts_repo_cache_path, launchers_repo_cache_path, load_node_cache, lookup,
    mcp_exposures_repo_cache_path, nodes_repo_cache_path, pairings_repo_cache_path,
    repositories_list_path, resolve_repo_launcher_path,
};
pub use services::repo::index::{
    IndexDrift, IndexError, RepoConflict, check_repository_index, generate_repository_index,
    publish_repository_index, read_repository_index, write_repository_index,
};
pub use services::repo::{
    Consumer, Implementer, IndexedNode, InitOutcome, MatchedItem, Observer, Participant, PinStatus,
    PublishedDoc, SearchOutcome, SearchQuery, SearchReport, ensure_default_repos,
    search_repo_items,
};
pub use services::{
    CoreNode, CoreNodeArguments, CoreNodeConfig, NAME_CLAIM_LINKED_SETTLE, TEARDOWN_REAP_BUDGET,
    check_runtime_prerequisites, force_kill_deadline, idle_timeout_flag, slow_connection_hint,
    teardown_all_instances,
};
