//! Pre-bound clock for the generated peppygen module.
//!
//! Wraps `peppylib::clock::for_node` so user code can read the
//! daemon-resolved time with a single call:
//!
//! ```ignore
//! peppygen::clock::init(&node_runner).await?;
//! let t = peppygen::clock::now_ns()?;
//! ```
//!
//! `init` must be called once before any `now_ns` call. The framework
//! treats subsequent `init` calls as no-ops, so it is safe to call from
//! both top-level setup and helper functions that may be invoked first.

use std::sync::OnceLock;

use peppylib::clock::{self, PeppyClock};
use peppylib::runtime::NodeRunner;
use peppylib::{PeppyError, PeppyResult};

static CLOCK: OnceLock<PeppyClock> = OnceLock::new();

/// Build the pre-bound clock for `node_runner`. Idempotent.
///
/// In wall mode this is a thin wrapper that does nothing observable; in
/// sim mode it opens a subscription to the `clock` topic so the first
/// `now_ns` call after a tick is delivered returns immediately.
pub async fn init(node_runner: &NodeRunner) -> PeppyResult<()> {
    if CLOCK.get().is_some() {
        return Ok(());
    }
    let resolved = clock::for_node(node_runner).await?;
    let _ = CLOCK.set(resolved);
    Ok(())
}

/// Read the current core-node-aligned time in nanoseconds since the Unix
/// epoch. Returns `Err(PeppyError::ClockNotReady)` if `init` has not run
/// or, in sim mode, if no `ClockTick` has been observed yet.
pub fn now_ns() -> PeppyResult<u64> {
    CLOCK.get().ok_or(PeppyError::ClockNotReady)?.now_ns()
}
