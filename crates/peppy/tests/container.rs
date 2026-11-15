use std::sync::Arc;

use peppy::commands::Command;
use peppy::commands::container::{ContainerCommand, ContainerCommands};
use peppy::context::AppContext;

/// `peppy container status` should succeed without a running daemon.
/// On the dev machine the user namespace setup may or may not be complete, so we
/// accept both Ok (all checks pass) and Err (some checks fail); the
/// important thing is that the command does not panic or crash.
#[test]
fn container_status_runs_without_daemon() {
    let ctx = Arc::new(AppContext::from_current_dir().expect("current dir is readable"));

    // status may return Ok (all checks pass) or Err (some checks fail);
    // either is a valid outcome.  We just verify it doesn't panic.
    let _result = ContainerCommand {
        command: ContainerCommands::Status { packages: false },
    }
    .execute(&ctx);
}

/// `peppy container status --packages` reads `PATH` alone, so it runs
/// without a daemon. It returns Ok when the machine has each package that
/// the daemon needs and Err when it does not; either is a valid outcome.
#[test]
fn container_packages_status_runs_without_daemon() {
    let ctx = Arc::new(AppContext::from_current_dir().expect("current dir is readable"));

    let _result = ContainerCommand {
        command: ContainerCommands::Status { packages: true },
    }
    .execute(&ctx);
}

/// `peppy container setup` should succeed without a running daemon.
/// In CI/test environments where stdin is not a terminal and AppArmor is
/// already configured (or not required), the command should either report
/// "nothing to do" (Ok) or fail gracefully (Err with a message), never panic.
#[test]
fn container_setup_runs_without_daemon() {
    let ctx = Arc::new(AppContext::from_current_dir().expect("current dir is readable"));

    let _result = ContainerCommand {
        command: ContainerCommands::Setup,
    }
    .execute(&ctx);
}
