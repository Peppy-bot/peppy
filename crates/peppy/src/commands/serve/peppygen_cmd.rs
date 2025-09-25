use std::path::PathBuf;

use super::{ServeAsyncCommand, ServeFuture};
use crate::{AppContext, AppEvent, Result};
use config::Interfaces;
use node_stack::DeploymentMappingBuilder;
use tokio::sync::broadcast;

pub struct InterfacesGenerator {
    events: broadcast::Sender<AppEvent>,
    root_dir: PathBuf,
    interfaces: Interfaces,
}

impl InterfacesGenerator {
    pub fn new(app_context: &AppContext, interfaces: Interfaces) -> Result<Self> {
        Ok(Self {
            events: app_context.event_sender(),
            root_dir: app_context.root_dir.clone(),
            interfaces,
        })
    }
}

impl ServeAsyncCommand for InterfacesGenerator {
    fn run(&self) -> ServeFuture {
        let mut app_events = self.events.subscribe();
        let nodes_cache_dir = self.root_dir.join(".peppy").join("nodes");
        let _interfaces = self.interfaces.clone();
        // FIXME: Is it really what we need?
        Box::pin(async move {
            loop {
                // We will only be able to take an events mut in here...
                match app_events.recv().await {
                    Ok(AppEvent::NodeConfigChanged(node_config_state)) => {
                        let nodes: Vec<_> = node_config_state
                            .values()
                            .filter_map(|entry| entry.as_ref().ok().cloned())
                            .collect();

                        let deployments: Vec<_> = nodes
                            .iter()
                            .flat_map(|node| {
                                node.deployments
                                    .as_ref()
                                    .into_iter()
                                    .flat_map(|items| items.iter().cloned())
                            })
                            .collect();

                        // TODO: Only the root node can have deployments

                        if deployments.is_empty() {
                            continue;
                        }

                        let _validated =
                            DeploymentMappingBuilder::new(&nodes_cache_dir, &deployments, &nodes)
                                .resolve_nodes()?
                                .validate_messages()?;

                        todo!("Finish, invoke functions in the generator crate");
                        //let _gen = generate_interfaces_code(_interfaces.as_ref(), &_for_language);
                    }
                    Ok(AppEvent::Shutdown) => break,
                    Ok(AppEvent::Custom { .. }) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            Ok(())
        })
    }
}
