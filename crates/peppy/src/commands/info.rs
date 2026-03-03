use std::sync::Arc;
use std::time::Duration;

use daemon_node::encoding::InfoRequest;

use super::Command;
use crate::context::AppContext;
use crate::error::{Error, Result};

const CALLER_INSTANCE_ID: &str = "peppy-cli";
// No need for an excessive timeout here
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

pub struct InfoCommand;

impl Command for InfoCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        crate::commands::block_on(info_async(ctx))
    }
}

async fn info_async(ctx: &Arc<AppContext>) -> Result<()> {
    let client_version = option_env!("PEPPY_GIT_TAG").unwrap_or("unknown");

    println!("Peppy client info");
    println!("----------");
    println!("Version: {}", client_version);

    // Query daemon node for its version
    let daemon_state = ctx.read_daemon_state().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to read daemon state. Is the peppy daemon running? Error: {}",
            e
        ))
    })?;
    let daemon_node_name = daemon_state.daemon_node_name.clone();

    ctx.connect()
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to connect to daemon: {}", e)))?;
    let messenger = ctx
        .messenger_handle()
        .expect("messenger should be initialized after connect");

    let request = InfoRequest::new();
    match request
        .poll(
            messenger,
            &daemon_node_name,
            CALLER_INSTANCE_ID,
            &daemon_node_name,
            REQUEST_TIMEOUT,
        )
        .await
    {
        Ok(response) => {
            println!();
            println!("Daemon Info");
            println!("-----------");
            println!("Version: {}", response.git_version);
            println!("Daemon node name: {}", response.daemon_node_name);
            println!(
                "Daemon node instance ID: {}",
                response.daemon_node_instance_id
            );
            println!("Host name: {}", response.host_name);
            println!("Uptime: {}s", response.uptime_secs);
            println!("Node count: {}", response.node_count);
            println!();
            println!("Container Info");
            println!("--------------");
            println!(
                "Apptainer version: {}",
                response.container_info.apptainer_version
            );
            println!("Lima version: {}", response.container_info.lima_version);
        }
        Err(e) => {
            eprintln!();
            eprintln!("Failed to get daemon info: {}", e);
        }
    }

    Ok(())
}
