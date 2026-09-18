use std::sync::Arc;

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::{BACKOFF_INIT, BACKOFF_MAX};
use crate::transport::{RawSubscription, RawTransport};

pub async fn run_sim_to_os<T, Runner, Msg, EmitFn>(
    transport: Arc<T>,
    runner: Arc<Runner>,
    token: CancellationToken,
    sim_node: Arc<str>,
    topic: Arc<str>,
    emit_fn: EmitFn,
) where
    T: RawTransport,
    Runner: Send + Sync + 'static,
    Msg: for<'de> Deserialize<'de> + Send + 'static,
    EmitFn: Fn(Arc<Runner>, Msg) -> super::BoxFuture<Result<(), String>> + Send + 'static,
{
    let instance_id = format!("sim_bridge_{topic}");
    let mut backoff = BACKOFF_INIT;

    'retry: loop {
        let mut sub = tokio::select! {
            _ = token.cancelled() => break,
            result = transport.subscribe(&instance_id, &sim_node, &topic) => {
                match result {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("sim_to_os({topic}): subscribe — {e}, retry in {backoff:?}");
                        tokio::select! {
                            _ = token.cancelled() => break 'retry,
                            _ = tokio::time::sleep(backoff) => {}
                        }
                        backoff = (backoff * 2).min(BACKOFF_MAX);
                        continue 'retry;
                    }
                }
            }
        };

        tracing::info!("sim_to_os: subscribed to {sim_node}/{topic}");
        backoff = BACKOFF_INIT;

        loop {
            tokio::select! {
                _ = token.cancelled() => break 'retry,
                msg = sub.next() => match msg {
                    Some(payload) => {
                        match serde_json::from_slice::<Msg>(&payload) {
                            Ok(m) => {
                                if let Err(e) = emit_fn(runner.clone(), m).await {
                                    tracing::warn!("sim_to_os({topic}): emit — {e}");
                                }
                            }
                            Err(e) => tracing::warn!("sim_to_os({topic}): deserialize — {e}"),
                        }
                    }
                    None => {
                        tracing::info!("sim_to_os({topic}): subscription closed — re-subscribing");
                        break;
                    }
                },
            }
        }
    }
}
