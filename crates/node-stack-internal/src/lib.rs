mod deployment;
mod error;

pub use error::Error as NodeStackError;
pub(crate) use error::Error;

// Class that creates a map from the `deployments` to the actual nodes expected inputs/output messages
pub use deployment::LocalNodesMapper;
