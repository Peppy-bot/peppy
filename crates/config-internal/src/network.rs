// TODO: deprecated module, probably needs removal
use super::types::{NetworkEvent, NodeDetectionEvent};
use crate::Result;
use std::{path::PathBuf, time::Duration};
use tokio::sync::mpsc;
use tracing::warn;

/// Get the current network automatically by detecting the default network interface
pub fn get_current_network() -> Option<ipnet::IpNet> {
    match netdev::get_default_interface() {
        Ok(interface) => {
            // Get the first IPv4 address with its network from the default interface
            for addr in interface.ipv4 {
                return Some(ipnet::IpNet::V4(addr));
            }
            // Fallback to IPv6 if no IPv4 found
            for addr in interface.ipv6 {
                return Some(ipnet::IpNet::V6(addr));
            }
            warn!("Default interface has no IP addresses");
            None
        }
        Err(e) => {
            warn!("Failed to get default network interface: {}", e);
            None
        }
    }
}

/// Finds all the other root nodes on the same network. Those root nodes will expose their own nodes
/// Only root nodes are exposed to the network. They broadcast their status on the network for other root nodes to find them.
/// If the current root node finds another one on the network, it connects to its /nodes service to pull its list of nodes.
pub fn find_root_nodes_on_network(netmask: Option<ipnet::IpNet>) -> Vec<PathBuf> {
    let network = match netmask {
        Some(net) => net,
        None => match get_current_network() {
            Some(net) => net,
            None => {
                warn!("Could not detect current network, returning empty list");
                return Vec::new();
            }
        },
    };

    // TODO: Use the network to find peppy nodes
    tracing::debug!("Searching for peppy nodes on network: {}", network);
    let peppy_files = Vec::new();
    peppy_files
}

// Async function to simulate network events, sending string messages over channel
// TODO replace the business logic of this function by a watcher that monitors new nodes added on the network
pub async fn watch_network_nodes(tx: mpsc::Sender<NodeDetectionEvent>) -> Result<()> {
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
