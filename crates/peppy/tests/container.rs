use std::sync::Arc;

use peppy::commands::Command;
use peppy::commands::container::{ContainerCommand, ContainerCommands};
use peppy::context::AppContext;

/// `peppy container status` should succeed without a running daemon.
/// On the dev machine the setuid setup may or may not be complete, so we
/// accept both Ok (all checks pass) and Err (some checks fail) — the
/// important thing is that the command does not panic or crash.
#[test]
fn container_status_runs_without_daemon() {
    let ctx = Arc::new(AppContext::default());

    // status may return Ok (all checks pass) or Err (some checks fail) —
    // either is a valid outcome.  We just verify it doesn't panic.
    let _result = ContainerCommand {
        command: ContainerCommands::Status,
    }
    .execute(&ctx);
}

/// `peppy container setup` should succeed without a running daemon.
/// In CI/test environments where stdin is not a terminal and setuid is
/// already configured, the command should either report "nothing to do" (Ok)
/// or fail gracefully (Err with a message) — never panic.
#[test]
fn container_setup_runs_without_daemon() {
    let ctx = Arc::new(AppContext::default());

    let _result = ContainerCommand {
        command: ContainerCommands::Setup { force: false },
    }
    .execute(&ctx);
}
