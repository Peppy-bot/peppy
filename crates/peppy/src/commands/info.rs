use std::sync::Arc;
use std::time::Duration;

use core_node_api::encoding::InfoRequest;
use daemon_config::consts::PEPPY_VERSION;

use super::{CALLER_INSTANCE_ID, Command};
use crate::context::AppContext;
use crate::error::{Error, Result};
use peppylib::core_node::transport::poll;

#[cfg(target_os = "linux")]
fn print_container_setup_status() {
    match containers::Apptainer::resolve_apptainer_dir() {
        Ok(dir) => {
            let status = containers::check_setup_status(&dir);
            println!(
                "Container setup: {}",
                container_setup_state(&status.verdict)
            );
        }
        Err(e) => {
            println!("Container setup: ERROR ({e})");
        }
    }
}

/// The state of the container setup that `peppy info` shows: the command
/// that makes the fixes when the setup has one to make, and the command that
/// shows the cause of a failure that has none. A setup whose one failure is
/// a warning is not incomplete: the nodes that peppy builds build and run.
#[cfg(target_os = "linux")]
fn container_setup_state(verdict: &containers::SetupVerdict) -> &'static str {
    use containers::SetupVerdict::{Fixable, Ready, Unfixable};

    match verdict {
        Ready => "OK",
        Fixable { .. } | Unfixable { .. } if verdict.fix_repairs_a_failure() => {
            "INCOMPLETE (run `peppy container setup`)"
        }
        Fixable { .. } => "OK, with a warning (run `peppy container setup`)",
        Unfixable { .. } => "INCOMPLETE (see `peppy container status`)",
    }
}

#[cfg(not(target_os = "linux"))]
fn print_container_setup_status() {
    println!("Container setup: OK (macOS, runs via Lima VM)");
}

// No need for an excessive timeout here
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct InfoCommand;

impl Command for InfoCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        crate::commands::block_on(info_async(ctx))
    }
}

async fn info_async(ctx: &Arc<AppContext>) -> Result<()> {
    println!("Peppy client info");
    println!("----------");
    println!("Version: {PEPPY_VERSION}");

    // Local container setup check (works without the daemon running)
    print_container_setup_status();

    // Query core node for its version
    let conn = ctx.connect_to_daemon().await?;

    let request = InfoRequest::new();
    let response = poll(
        &request,
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        &conn.target_core_node,
        REQUEST_TIMEOUT,
    )
    .await
    .map_err(|e| Error::ExecutionFailed(format!("Failed to get daemon info: {e}")))?;

    println!();
    println!("Daemon Info");
    println!("-----------");
    println!("Version: {}", response.git_version);
    println!("Core node name: {}", response.core_node_name);
    println!("Core node instance ID: {}", response.core_node_instance_id);
    println!("Host name: {}", response.host_name);
    println!("Messaging port: {}", response.messaging_port);
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

    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::commands::container::test_fixtures::fix_of;
    use containers::{FailureSeverity, SetupVerdict};

    #[test]
    fn the_container_setup_line_names_the_command_to_run_next() {
        let refusal = || "The kernel refuses a user namespace.".to_string();
        let cases = [
            (SetupVerdict::Ready, "OK"),
            (
                SetupVerdict::Fixable {
                    fix: fix_of(FailureSeverity::StopsTheDaemon),
                },
                "INCOMPLETE (run `peppy container setup`)",
            ),
            (
                SetupVerdict::Fixable {
                    fix: fix_of(FailureSeverity::StopsEachBuild),
                },
                "INCOMPLETE (run `peppy container setup`)",
            ),
            (
                SetupVerdict::Fixable {
                    fix: fix_of(FailureSeverity::Warning),
                },
                "OK, with a warning (run `peppy container setup`)",
            ),
            (
                SetupVerdict::Unfixable {
                    refusal: refusal(),
                    fix: Some(fix_of(FailureSeverity::StopsTheDaemon)),
                },
                "INCOMPLETE (run `peppy container setup`)",
            ),
            (
                SetupVerdict::Unfixable {
                    refusal: refusal(),
                    fix: Some(fix_of(FailureSeverity::Warning)),
                },
                "INCOMPLETE (see `peppy container status`)",
            ),
            (
                SetupVerdict::Unfixable {
                    refusal: refusal(),
                    fix: None,
                },
                "INCOMPLETE (see `peppy container status`)",
            ),
        ];
        for (verdict, state) in cases {
            assert_eq!(container_setup_state(&verdict), state, "{verdict:?}");
        }
    }
}
