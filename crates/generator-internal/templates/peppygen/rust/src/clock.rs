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
//! `init` must be called before any `now_ns` call. It binds the module
//! clock to the initializing node: calling it again for the same node is a
//! no-op, so it is safe to call from both top-level setup and helper
//! functions that may be invoked first. Initializing a different node
//! rebinds, which is how consecutive test-harness boots in one process
//! (wall- and sim-time alike) each read their own clock; the harness
//! serializes boots, so a rebind never races a live node.

use std::sync::{PoisonError, RwLock};

use peppylib::clock::{self, PeppyClock};
use peppylib::runtime::NodeRunner;
use peppylib::{PeppyError, PeppyResult};

/// The bound clock, keyed by the owning node's wire identity so a second
/// `init` can tell "same node again" (no-op) from "new node" (rebind).
static CLOCK: RwLock<Option<(String, PeppyClock)>> = RwLock::new(None);

fn runner_key(node_runner: &NodeRunner) -> String {
    let processor = node_runner.processor();
    format!(
        "{}/{}",
        processor.bound_core_node(),
        processor.bound_instance_id()
    )
}

/// Build the pre-bound clock for `node_runner`. Idempotent per node.
///
/// In wall mode this is a thin wrapper that does nothing observable; in
/// sim mode it opens a subscription to the `clock` topic so the first
/// `now_ns` call after a tick is delivered returns immediately.
pub async fn init(node_runner: &NodeRunner) -> PeppyResult<()> {
    let key = runner_key(node_runner);
    {
        let bound = CLOCK.read().unwrap_or_else(PoisonError::into_inner);
        if matches!(&*bound, Some((bound_key, _)) if *bound_key == key) {
            return Ok(());
        }
    }
    let resolved = clock::for_node(node_runner).await?;
    *CLOCK.write().unwrap_or_else(PoisonError::into_inner) = Some((key, resolved));
    Ok(())
}

/// Read the current core-node-aligned time in nanoseconds since the Unix
/// epoch. Returns `Err(PeppyError::ClockNotReady)` if `init` has not run
/// or, in sim mode, if no `ClockTick` has been observed yet.
pub fn now_ns() -> PeppyResult<u64> {
    match &*CLOCK.read().unwrap_or_else(PoisonError::into_inner) {
        Some((_, bound_clock)) => bound_clock.now_ns(),
        None => Err(PeppyError::ClockNotReady),
    }
}
