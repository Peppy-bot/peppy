mod bindings;
mod composition;
mod flatten;
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
pub use composition::{
    Adjustment, ComponentAxis, Fragment, FragmentPart, FragmentSpec, LauncherFragment,
    LauncherFragmentParser,
};
pub use flatten::{
    AppliedAdjustment, AppliedChange, ComponentSelection, CompositionError, FlattenReport,
    SelectionEntry, SelectionSource, SkipReason, SkippedAdjustment, compose, enumerate_selections,
    flatten, load_composition, resolve_selection, selection_space_size,
};
pub use links::{ValidatedLinkPlan, validate_link_plan, validate_link_slots};
pub use observations::{PlannedObservation, ValidatedObservations, validate_observations};
pub use pairings::{
    AlreadyPairedSlots, ExternallyCoveredSlots, PairingValidationItem, PlannedPairEndpoint,
    PlannedPairing, ValidatedPairings, validate_pairings,
};
pub use parse::PeppyLauncherParser;
pub use types::{
    Deployment, DeploymentInstance, DeploymentSource, DuplicateLinkTarget, EmptyVacantReason,
    FrameworkOverrides, LinkTargets, LinkValue, PeppyLauncher, Placements, Selection, VacantReason,
    participant_vacancies, split_link_target,
};
