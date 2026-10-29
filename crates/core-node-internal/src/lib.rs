#![forbid(unsafe_code)]
//! The core node is a special kind of node that has access to the whole context of the peppy daemon and runs as part of the same process

mod error;
mod services;
mod source_url;
mod ssh_config;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use error::{Error, Result};
#[cfg(any(test, feature = "test-support"))]
pub use services::InstanceKeys;
#[cfg(any(test, feature = "test-support"))]
pub use services::federation::{ReserveOutcome, RestartHold, SliceOwnership};
pub use services::mcp::{
    ExposureDocumentsError, ExposureFinding, check_repository_exposures, derive_exposure_catalog,
    resolve_exposure_documents, resolve_exposure_plan,
};
pub use services::repo::cache::{
    ContractSlot, DeclaredLinks, EntryOrigin, ImplementsClaim, NodeCacheEntry, ObserverSlot,
    PairingSlot, contracts_repo_cache_path, launchers_repo_cache_path, load_node_cache, lookup,
    nodes_repo_cache_path, pairings_repo_cache_path, repositories_list_path,
    resolve_repo_launcher_path,
};
pub use services::repo::descriptions::{CachedDescriptions, FieldDescription, MemberDescriptions};
pub use services::repo::index::{
    IndexDrift, IndexError, RepoConflict, check_repository_index, publish_repository_index,
};
pub use services::repo::{
    Consumer, Implementer, IndexedNode, InitOutcome, MatchedItem, Observer, Participant, PinStatus,
    PublishedDoc, SearchOutcome, SearchQuery, SearchReport, ShowOutcome, SlotPeers,
    ensure_default_repos, search_repo_items, show_repo_items,
};
pub use services::{
    CoreNode, CoreNodeArguments, CoreNodeConfig, DAEMON_SENDER_GRACE, ExportDiscardLog,
    HealthMonitorPolicy, HostAddressSource, ManualTime, NAME_CLAIM_LINKED_SETTLE, ReplyBounds,
    Report, RestartBackoff, TEARDOWN_REAP_BUDGET, TimeSource, check_runtime_prerequisites,
    checkout_dir_for, copy_removal_budget, current_host_name, force_kill_deadline,
    idle_timeout_flag, materialized_checkout, node_remove_bound, node_stop_bound,
    slow_connection_hint, stack_reset_timeout, teardown_all_instances, test_host_addresses,
};
pub use ssh_config::{IdentityAgent, SshHostConfig, SshTarget, resolve_host_config};
