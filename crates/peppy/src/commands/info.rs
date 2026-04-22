use std::sync::Arc;
use std::time::Duration;

use core_node_api::encoding::InfoRequest;

use super::{CALLER_INSTANCE_ID, Command};
use crate::context::AppContext;
use crate::error::Result;
use core_node::encoding::prelude::*;

#[cfg(target_os = "linux")]
fn print_container_setup_status() {
    match containers::Apptainer::resolve_apptainer_dir() {
        Ok(dir) => {
            let status = containers::check_setup_status(&dir);
            if status.is_ok() {
                println!("Container setup: OK");
            } else {
                println!("Container setup: INCOMPLETE (run `peppy container setup`)");
            }
        }
        Err(e) => {
            println!("Container setup: ERROR ({e})");
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn print_container_setup_status() {
    println!("Container setup: OK (macOS — runs via Lima VM)");
}

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

    // Local container setup check (works without the daemon running)
    print_container_setup_status();

    // Query core node for its version
    let conn = ctx.connect_to_daemon().await?;

    let request = InfoRequest::new();
    match request
        .poll(
            conn.messenger,
            &conn.core_node_name,
            CALLER_INSTANCE_ID,
            &conn.core_node_name,
            REQUEST_TIMEOUT,
        )
        .await
    {
        Ok(response) => {
            println!();
            println!("Daemon Info");
            println!("-----------");
            println!("Version: {}", response.git_version);
            println!("Core node name: {}", response.core_node_name);
            println!("Core node instance ID: {}", response.core_node_instance_id);
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
