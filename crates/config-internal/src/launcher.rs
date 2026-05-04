mod parse;
mod types;

// Defines the parsing of `peppy_launcher.json5` files
pub use parse::PeppyLauncherParser;
pub use types::{
    Deployment, DeploymentGitSource, DeploymentInstance, DeploymentLocalSource,
    DeploymentRepoSource, DeploymentSource, DeploymentUrlSource, FrameworkOverrides, Name,
    PeppyLauncher, PeppySchema, VariantGitSource, VariantNameSource, VariantSource,
    VariantUrlSource,
};
