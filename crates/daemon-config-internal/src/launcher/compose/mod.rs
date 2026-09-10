//! Composition: turning a composed launcher plus a selection into the
//! ordinary flat launcher the rest of the pipeline consumes.
//!
//! Composition is a pure function of the launcher, its fragments, and the
//! selection; the only I/O is reading fragment files, all of them resolved
//! relative to the declaring document's directory so a repository launcher
//! and a filesystem launcher compose identically.
//!
//! A launch is the stack, the launcher's own axes and the axes of the
//! options they select, plus every copy the launcher's `deployments` list.
//! A join is one more copy over the stack as it runs; a removal is the
//! stack without one.

mod check;
mod constraints;
mod copy;
mod error;
mod expand;
mod load;
mod prepared;
mod report;
mod select;

pub use check::check_composition;
pub use copy::CopyRecord;
pub use error::CompositionError;
pub use prepared::{ComposedJoin, ComposedLaunch, JoinRequest, PreparedLauncher, RunningStack};
pub use report::{
    AppliedAdjustment, AppliedChange, CompositionReport, SkipReason, SkippedAdjustment,
};
pub use select::{SelectionEntry, SelectionSource, UnitSelection};
