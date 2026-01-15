pub use peppylib::runtime::runner::NodeRunner;
use peppylib::runtime::runner::run as peppylib_run;
use std::sync::Arc;

/// Runs the node and exits with a human-readable error message if it fails.
#[allow(dead_code)]
pub fn run_or_exit<F, Fut>(setup_fn: F)
where
    F: FnOnce(crate::parameters::Parameters, Arc<NodeRunner>) -> Fut,
    Fut: std::future::Future<Output = peppylib::PeppyResult<()>>,
{
    if let Err(e) = peppylib_run(setup_fn) {
        peppylib::report_error_and_exit(e);
    }
}
