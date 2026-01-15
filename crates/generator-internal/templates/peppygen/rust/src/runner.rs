pub use peppylib::runtime::runner::NodeRunner;
pub use peppylib::StandaloneConfig;
use peppylib::runtime::runner::run as peppylib_run;
use peppylib::run_standalone as peppylib_run_standalone;
use std::sync::Arc;

/// Runs the node in daemon mode (started by peppy service).
///
/// This expects the `PEPPY_RUNTIME_CONFIG` environment variable to be set
/// with the path to the runtime configuration file.
#[allow(dead_code)]
pub fn run<F, Fut>(setup_fn: F) -> peppylib::PeppyResult<()>
where
    F: FnOnce(crate::parameters::Parameters, Arc<NodeRunner>) -> Fut,
    Fut: std::future::Future<Output = peppylib::PeppyResult<()>>,
{
    peppylib_run(setup_fn)
}

/// Runs the node in standalone mode (started directly with cargo run).
///
/// This allows running the node without the peppy daemon, using a local
/// Zenoh router for messaging.
///
/// # Example
/// ```ignore
/// use peppygen::{run_standalone, StandaloneConfig, parameters::Parameters};
///
/// fn main() -> peppylib::PeppyResult<()> {
///     let params = Parameters {
///         // your parameters here
///     };
///
///     let config = StandaloneConfig::new("127.0.0.1", 7448, "my_node", "instance_1", params);
///
///     run_standalone(config, |params, node_runner| async move {
///         // Use params and node_runner.messenger() for topics/services
///         Ok(())
///     })
/// }
/// ```
#[allow(dead_code)]
pub fn run_standalone<F, Fut>(
    config: StandaloneConfig<crate::parameters::Parameters>,
    setup_fn: F,
) -> peppylib::PeppyResult<()>
where
    F: FnOnce(crate::parameters::Parameters, Arc<NodeRunner>) -> Fut,
    Fut: std::future::Future<Output = peppylib::PeppyResult<()>>,
{
    peppylib_run_standalone(config, setup_fn)
}
