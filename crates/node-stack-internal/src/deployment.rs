mod git;
mod local;
mod planner;
mod url;
mod variant;

use std::path::PathBuf;

use config::node::NodeConfig;

#[derive(Debug, Clone)]
pub(crate) struct ResolvedNode {
    pub config: NodeConfig,
    pub root_path: PathBuf,
}

pub mod types;
pub use planner::LaunchPlan;
pub use types::NodeStack;
