use std::sync::Arc;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use super::{BACKOFF_INIT, BACKOFF_MAX};
use crate::transport::RawTransport;

pub async fn run_os_to_sim<T, Runner, Msg, RecvFn>(
    transport: Arc<T>,
    runner: Arc<Runner>,
    token: CancellationToken,
    topic: Arc<str>,
    recv_fn: RecvFn,
) where
    T: RawTransport,
    Runner: Send + Sync + 'static,
    Msg: Serialize + Send + 'static,
    RecvFn: Fn(Arc<Runner>) -> super::BoxFuture<std::result::Result<(String, Msg), String>>
        + Send
        + 'static,
{
    let instance_id = format!("sim_bridge_{topic}_pub");
    let mut backoff = BACKOFF_INIT;

    loop {
        tokio::select! {
            _ = token.cancelled() => break,
            result = recv_fn(runner.clone()) => {
                let (_sender, msg) = match result {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!("os_to_sim({topic}): receive — {e}, retry in {backoff:?}");
                        tokio::select! {
                            _ = token.cancelled() => break,
                            _ = tokio::time::sleep(backoff) => {}
                        }
                        backoff = (backoff * 2).min(BACKOFF_MAX);
                        continue;
                    }
                };

                let payload = match serde_json::to_vec(&msg) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!("os_to_sim({topic}): serialize — {e}");
                        continue;
                    }
                };

                match transport.emit(&instance_id, &topic, payload).await {
                    Ok(()) => backoff = BACKOFF_INIT,
                    Err(e) => {
                        tracing::warn!("os_to_sim({topic}): emit — {e}, retry in {backoff:?}");
                        tokio::select! {
                            _ = token.cancelled() => break,
                            _ = tokio::time::sleep(backoff) => {}
                        }
                        backoff = (backoff * 2).min(BACKOFF_MAX);
                    }
                }
            },
        }
    }
}
