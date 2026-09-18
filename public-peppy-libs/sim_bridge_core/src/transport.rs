// Raw-topic transport contract. peppylib is generated per node by `peppy node
// sync` and ships with peppy nodes, so this shared lib never links it —
// the consuming node implements these traits with its own generated peppylib
// and hands the implementation to SimBridge / call_sim.

use std::future::Future;
use std::pin::Pin;

pub type TransportFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait RawSubscription: Send + 'static {
    /// Next raw payload. `None` means the subscription closed and the caller
    /// should resubscribe.
    fn next(&mut self) -> TransportFuture<'_, Option<Vec<u8>>>;
}

pub trait RawTransport: Send + Sync + 'static {
    type Subscription: RawSubscription;

    /// Subscribe to `topic` produced by `source_node` (interface tag v1) with
    /// telemetry (latest-wins) delivery. The implementation owns connection
    /// setup; an Err here is retried by the pipeline with exponential backoff.
    fn subscribe<'a>(
        &'a self,
        instance_id: &'a str,
        source_node: &'a str,
        topic: &'a str,
    ) -> TransportFuture<'a, std::result::Result<Self::Subscription, String>>;

    /// Publish `payload` on `topic` under the sim-bridge identity with
    /// reliable delivery. The implementation owns connection reuse and
    /// teardown on failure.
    fn emit<'a>(
        &'a self,
        instance_id: &'a str,
        topic: &'a str,
        payload: Vec<u8>,
    ) -> TransportFuture<'a, std::result::Result<(), String>>;
}
