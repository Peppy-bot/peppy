/// The core node is a special kind of node that has access to the whole context of the peppy daemon and runs as part of the same process
pub mod names;

mod error;
mod services;

pub use error::{Error, Result};
pub use services::repo::cache::{
    launchers_repo_cache_path, nodes_repo_cache_path, repositories_list_path,
};
pub use services::repo::{InitOutcome, ensure_default_repos};
pub use services::{CoreNode, CoreNodeArguments, FORBIDDEN_ENV_KEYS};
