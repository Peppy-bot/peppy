pub mod publisher;
pub mod subscriber;

pub use publisher::run_os_to_sim;
pub use subscriber::run_sim_to_os;

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

pub(crate) const BACKOFF_INIT: Duration = Duration::from_secs(1);
pub(crate) const BACKOFF_MAX: Duration = Duration::from_secs(30);

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
