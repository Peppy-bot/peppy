mod bindings;
mod compose;
mod composition;
mod links;
mod observations;
mod pairings;
mod parse;
mod types;

// Defines the parsing of launcher documents (`peppy_schema: "launcher/v1"`).
// The conventional filename is `peppy_launcher.json5` for standalone projects,
// but the parser is filename-agnostic; repository discovery accepts any
// `.json5` file whose body declares the launcher schema.
pub use bindings::{BindingValidationItem, validate_bindings};
pub use compose::{
    AppliedChange, ComposedJoin, ComposedLaunch, CompositionError, CompositionReport, CopyRecord,
    JoinRequest, PreparedLauncher, RunningStack, SELF_COPY_NAME_REFUSAL, SkipReason,
    SkippedAdjustment, UnitSelection, check_composition,
};
pub use composition::{ComponentAxis, FragmentPart, FragmentSpec, LauncherFragmentParser};
pub use links::{validate_link_plan, validate_link_slots, validate_sim_time_source};
pub use observations::PlannedObservation;
pub use pairings::{
    AlreadyPairedSlots, ExternallyCoveredSlots, PairingValidationItem, PlannedPairEndpoint,
    PlannedPairing, validate_pairings,
};
pub use parse::PeppyLauncherParser;
pub use types::{
    Deployment, DeploymentInstance, DeploymentSource, FrameworkOverrides, LinkTargets, LinkValue,
    PeppyLauncher, Placements, Selection, VacantReason, participant_vacancies, split_link_target,
};
