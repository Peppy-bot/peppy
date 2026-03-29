mod git;
mod local;
mod planner;
mod url;

use std::path::PathBuf;

use config::node::RawNodeConfig;

#[derive(Debug, Clone)]
pub(crate) struct ResolvedNode {
    pub config: RawNodeConfig,
    pub root_path: PathBuf,
}

pub mod types;
pub use planner::LaunchPlan;
pub use types::NodeStack;
