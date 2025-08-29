//use super::types::{CommandContext, ServeAsyncCommand};

use crate::Result;
use tokio::task::JoinHandle;

pub struct NodeWatcherCommand {}

impl NodeWatcherCommand {
    // Use the following design pattern in this module:
    // Observer Pattern for Node Watching
    //
    // The node_watcher.rs could be enhanced with:
    // - A proper event system where components can subscribe to node configuration changes
    // - This would decouple the watcher from its consumers
    // - Makes it easier to add new reactions to configuration changes

    // The goal of this module is to observe the change in `peppy.star` configuration files the `peppy.star` root configuration is pointing to.
    // If a file has changed, the watcher should notify all subscribers about the change.
    // Beyond configuration file change, a node can also communicate with other nodes via pubsub even tho this node is not part of the file configuration of the current project.
    // The node_watcher should specify what type event has been detected, for example if it's an internal event (a file belonging to this project has changed) or an external event (a node outside this project has joined the network of nodes).
    // The main subscriber to this node_watcher is the python dependency or the Rust crate that is automatically generated inside the .pixi virtualenv (and added to pixi.toml) when a file configuration changes.
    fn watch_node_configuration_files_changes() {
        todo!(
            "Run a separate thread that watches changes on peppy.star and updates the pixi envs accordingly"
        );
    }
}

impl super::ServeAsyncCommand for NodeWatcherCommand {
    fn execute_async(&self) -> Result<JoinHandle<Result<()>>> {
        let handle = tokio::spawn(async move {
            NodeWatcherCommand::watch_node_configuration_files_changes();
            Ok(())
        });

        Ok(handle)
    }
}
