#![forbid(unsafe_code)]
//! The core node is a special kind of node that has access to the whole context of the peppy daemon and runs as part of the same process

mod error;
mod services;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use error::{Error, Result};
pub use services::repo::cache::{
    contracts_repo_cache_path, launchers_repo_cache_path, nodes_repo_cache_path,
    pairings_repo_cache_path, repositories_list_path,
};
pub use services::repo::{InitOutcome, ensure_default_repos};
pub use services::{
    CoreNode, CoreNodeArguments, CoreNodeConfig, NAME_CLAIM_LINKED_SETTLE, TEARDOWN_REAP_BUDGET,
    check_runtime_prerequisites, force_kill_deadline, teardown_all_instances,
};
