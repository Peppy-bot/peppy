use peppylib::MessengerHandle;
use peppylib::runtime::Processor;

#[allow(dead_code)]
pub fn run<F, Fut>(setup_fn: F) -> crate::Result<()>
where
    F: FnOnce(crate::parameters::Parameters, Arc<NodeRunner>) -> Fut,
    Fut: std::future::Future<Output = crate::Result<()>>,
{
    peppylib::runtime::run(setup_fn)?.await
}
