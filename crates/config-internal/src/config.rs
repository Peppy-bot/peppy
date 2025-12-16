mod create;
mod parse;
mod types;

// Defines the parsing of `peppy_launcher.json5` files
pub use parse::PeppyLauncherParser;
pub use types::{
    BuildSystem, CURRENT_SCHEMA_VERSION, Deployment, DeploymentInstance, DeploymentNodeSource,
    GitRemoteSpec, HttpRemoteSpec, Name, PeppyLauncher, SchemaVersion,
};
