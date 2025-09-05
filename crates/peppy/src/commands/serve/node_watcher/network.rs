use super::types::{NetworkEvent, NodeDetectionEvent};
use crate::Result;
use std::time::Duration;
use tokio::sync::mpsc;

// Async function to simulate network events, sending string messages over channel
// TODO replace this by a watcher that monitors new nodes added on the network
pub async fn network_events(tx: mpsc::Sender<NodeDetectionEvent>) -> Result<()> {
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if tx
            .send(NodeDetectionEvent::NetworkEvent(
                NetworkEvent::ExternalNodeDetected {
                    uri: String::from("192.168.0.1:7654/a_node"),
                },
            ))
            .await
            .is_err()
        {
            break;
        }
    }
    Ok(())
}
