mod bindings;
mod links;
mod observations;
mod pairings;
mod parse;
mod types;

// Defines the parsing of launcher documents (`peppy_schema: "launcher/v1"`).
// The conventional filename is `peppy_launcher.json5` for standalone projects,
// but the parser is filename-agnostic; repository discovery accepts any
// `.json5` file whose body declares the launcher schema.
pub use bindings::{BindingValidationItem, ValidatedBindings, validate_bindings};
pub use links::{ValidatedLinkPlan, validate_link_plan, validate_link_slots};
pub use observations::{PlannedObservation, ValidatedObservations, validate_observations};
pub use pairings::{
    AlreadyPairedSlots, PairingValidationItem, PlannedPairEndpoint, PlannedPairing,
    ValidatedPairings, validate_pairings,
};
pub use parse::PeppyLauncherParser;
pub use types::{
    Deployment, DeploymentGitSource, DeploymentInstance, DeploymentLocalSource,
    DeploymentRepoSource, DeploymentSource, DeploymentUrlSource, DuplicateLinkTarget,
    FrameworkOverrides, LinkTargets, LinkValue, PeppyLauncher, split_link_target,
};
