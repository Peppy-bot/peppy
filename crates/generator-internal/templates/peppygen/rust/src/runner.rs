pub use peppylib::runtime::runner::NodeRunner;
use peppylib::runtime::runner::run as peppylib_run;
use std::sync::Arc;

#[allow(dead_code)]
pub fn run<F, Fut>(setup_fn: F) -> peppylib::PeppyResult<()>
where
    F: FnOnce(crate::parameters::Parameters, Arc<NodeRunner>) -> Fut,
    Fut: std::future::Future<Output = peppylib::PeppyResult<()>>,
{
    peppylib_run(setup_fn)
}
