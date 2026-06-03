mod bindings;
mod parse;
mod types;

// Defines the parsing of launcher documents (`peppy_schema: "launcher_v1"`).
// The conventional filename is `peppy_launcher.json5` for standalone projects,
// but the parser is filename-agnostic — repository discovery accepts any
// `.json5` file whose body declares the launcher schema.
pub use bindings::{
    BindingValidationItem, ValidatedBindings, producer_satisfies_slot, slot_meta_for_link_id,
    validate_bindings,
};
pub use parse::PeppyLauncherParser;
pub use types::{
    Deployment, DeploymentGitSource, DeploymentInstance, DeploymentLocalSource,
    DeploymentRepoSource, DeploymentSource, DeploymentUrlSource, FrameworkOverrides, Name,
    PeppyLauncher,
};
