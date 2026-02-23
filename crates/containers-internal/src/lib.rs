mod error;

#[cfg(feature = "apptainer")]
mod apptainer;

#[cfg(feature = "apptainer")]
pub use apptainer::ApptainerFacade;

pub use error::{Error, Result};
