use std::time::Duration;

use serde_json::Value;

use crate::transport::{RawSubscription, RawTransport};

const SIM_CTRL_TIMEOUT: Duration = Duration::from_secs(5);
const SIM_CTRL_REQ_SUFFIX: &str = "_req";
const SIM_CTRL_RES_SUFFIX: &str = "_res";

pub async fn call_sim<T: RawTransport>(
    transport: &T,
    sim_node: &str,
    service: &str,
    payload: Value,
) -> std::result::Result<Value, String> {
    let req_topic = format!("sim_ctrl_{service}{SIM_CTRL_REQ_SUFFIX}");
    let res_topic = format!("sim_ctrl_{service}{SIM_CTRL_RES_SUFFIX}");

    // Subscribe to the response topic before emitting the request so the
    // reply can't race past us.
    let mut sub = transport
        .subscribe(
            &format!("sim_bridge_{service}_res_sub"),
            sim_node,
            &res_topic,
        )
        .await
        .map_err(|e| format!("subscribe {res_topic}: {e}"))?;

    let body = serde_json::to_vec(&payload).map_err(|e| format!("serialize: {e}"))?;
    transport
        .emit(&format!("sim_bridge_{service}_req_pub"), &req_topic, body)
        .await
        .map_err(|e| format!("emit {req_topic}: {e}"))?;

    let msg = tokio::time::timeout(SIM_CTRL_TIMEOUT, sub.next())
        .await
        .map_err(|_| format!("timeout waiting for {res_topic}"))?
        .ok_or_else(|| format!("subscription closed for {res_topic}"))?;

    serde_json::from_slice::<Value>(&msg).map_err(|e| format!("deserialize response: {e}"))
}

/// Requires a multi-threaded Tokio runtime (`flavor = "multi_thread"`). Panics on current_thread.
pub fn call_sim_sync<T: RawTransport>(
    transport: &T,
    sim_node: &str,
    service: &str,
    payload: Value,
) -> std::result::Result<Value, String> {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(call_sim(transport, sim_node, service, payload))
    })
}
