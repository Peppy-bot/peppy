//! `peppy fleet`: the fleet of the stack, as the daemon of its coordinator
//! serves it in `fleet:v1`. `list` gives each robot with its kind, its
//! software and its lease, and the halt; `halt` halts the stack, and
//! `clear-halt` ends the halt. The command line has no scope: it acts for the
//! person or the program that runs it.
//!
//! The command reaches the daemon of the coordinator of the launch that the
//! targeted daemon runs a slice of ([`fleet_daemon`]), so a command on any
//! machine of a federated stack reaches the same leases and the same halt.

use super::{CALLER_INSTANCE_ID, Command, GOAL_TIMEOUT};
use crate::context::{AppContext, DaemonConnection};
use crate::error::{Error, Result};
use clap::Subcommand;
use core_node_api::encoding::{StackListRequest, StackListResponse};
use daemon_config::daemon_interface::DaemonInterface;
use daemon_config::daemon_interface::fleet::{
    FleetLease, FleetListing, FleetMember, FleetRobot, RobotKind, SoftwareState,
};
use peppy_codec::consumer::{ConsumerIdentity, MemberBinding, ServiceClient};
use peppylib::core_node::transport::poll;
use peppylib::messaging::ProducerRef;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Subcommand)]
pub enum FleetCommands {
    /// List the robots of the stack: the kind, the software and the lease of
    /// each, and the halt.
    List {
        /// Emit the whole fleet listing as one line of JSON: the robots with
        /// their instances and contracts, the leases, the options of the
        /// scopes of the launch with their pinned documents, and the nodes.
        #[arg(long)]
        json: bool,
    },
    /// Halt the stack: end every lease, and refuse every move until `peppy
    /// fleet clear-halt`. Each client that asks the daemon before it moves a
    /// robot cancels its moves. The halt stays after a restart of the daemon.
    Halt,
    /// End the halt of the stack. It starts nothing again: each client takes
    /// its leases again.
    ClearHalt,
}

pub struct FleetCommand {
    pub command: FleetCommands,
}

impl Command for FleetCommand {
    fn execute(self, app_ctx: &Arc<AppContext>) -> Result<()> {
        super::block_on(async {
            let conn = app_ctx.connect_to_daemon().await?;
            let daemon = fleet_daemon(&conn).await?;
            match self.command {
                FleetCommands::List { json } => {
                    let listing = list(&conn, &daemon).await?;
                    let text = if json {
                        serde_json::to_string(&listing)
                            .map_err(|error| Error::ExecutionFailed(error.to_string()))?
                    } else {
                        render_listing(&listing)
                    };
                    println!("{text}");
                }
                FleetCommands::Halt => {
                    call_fleet(&conn, &daemon, FleetMember::Halt, &json!({})).await?;
                    println!("The stack is halted: no robot moves until `peppy fleet clear-halt`.");
                }
                FleetCommands::ClearHalt => {
                    call_fleet(&conn, &daemon, FleetMember::ClearHalt, &json!({})).await?;
                    println!(
                        "The stack is not halted. Each client takes its leases again; nothing \
                         starts again by itself."
                    );
                }
            }
            Ok(())
        })
    }
}

/// The identity of the command line on the wire, as a client of the fleet.
pub(crate) fn command_line_client(conn: &DaemonConnection<'_>) -> ConsumerIdentity {
    ConsumerIdentity {
        core_node: conn.core_node_name.clone(),
        instance_id: CALLER_INSTANCE_ID.to_owned(),
    }
}

/// The daemon that serves the fleet that a command on `conn` reaches: the
/// daemon of the coordinator of the launch that the targeted daemon runs a
/// slice of, else the targeted daemon.
pub(crate) async fn fleet_daemon(conn: &DaemonConnection<'_>) -> Result<ProducerRef> {
    let targeted = stack_listing(conn, &conn.target_core_node).await?;
    let coordinator = match &targeted.launch {
        Some(launch) if launch.coordinator_core_node != targeted.core_node => {
            launch.coordinator_core_node.clone()
        }
        _ => return Ok(ProducerRef::new(targeted.core_node, targeted.instance_id)),
    };
    let coordinating = stack_listing(conn, &coordinator).await.map_err(|error| {
        Error::ExecutionFailed(format!(
            "`{}` runs a slice of the launch that `{coordinator}` coordinates, whose daemon \
             serves the fleet: {error}",
            conn.target_core_node
        ))
    })?;
    Ok(ProducerRef::new(
        coordinating.core_node,
        coordinating.instance_id,
    ))
}

/// The stack listing of the daemon of `core_node`, which names the daemon
/// and the launch of its slice.
async fn stack_listing(conn: &DaemonConnection<'_>, core_node: &str) -> Result<StackListResponse> {
    poll(
        &StackListRequest::new(),
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        core_node,
        GOAL_TIMEOUT,
    )
    .await
    .map_err(|error| {
        Error::ExecutionFailed(format!(
            "The daemon of `{core_node}` did not list its stack: {error}"
        ))
    })
}

/// Calls the service `member` of `fleet:v1` on `daemon` with `request`.
async fn call_fleet(
    conn: &DaemonConnection<'_>,
    daemon: &ProducerRef,
    member: FleetMember,
    request: &Value,
) -> Result<Value> {
    let document = DaemonInterface::Fleet.document();
    let name = member.name();
    let failed = |error: String| Error::ExecutionFailed(format!("`fleet.{name}` failed: {error}"));
    let client =
        ServiceClient::of_contract(document, name).map_err(|error| failed(error.to_string()))?;
    let binding =
        MemberBinding::of_contract(document, name).map_err(|error| failed(error.to_string()))?;
    client
        .call(
            conn.messenger,
            &command_line_client(conn),
            &binding,
            daemon,
            request,
            GOAL_TIMEOUT,
        )
        .await
        .map_err(|error| failed(error.to_string()))
}

/// The fleet listing of `daemon`.
async fn list(conn: &DaemonConnection<'_>, daemon: &ProducerRef) -> Result<FleetListing> {
    let answer = call_fleet(conn, daemon, FleetMember::List, &json!({})).await?;
    serde_json::from_value(answer).map_err(|error| {
        Error::ExecutionFailed(format!("The fleet listing does not parse: {error}"))
    })
}

/// The listing as a person reads it: the launcher and the halt, then one
/// line per robot.
fn render_listing(listing: &FleetListing) -> String {
    let mut lines = vec![format!(
        "Fleet of `{}` (launcher `{}`, peppy {})",
        listing.core_node,
        if listing.launcher.is_empty() {
            "none"
        } else {
            &listing.launcher
        },
        listing.version
    )];
    if listing.halted {
        lines.push(format!(
            "HALTED by {}: no robot moves until `peppy fleet clear-halt`",
            listing.halted_by
        ));
    }
    if listing.robots.is_empty() {
        lines.push("No robot is on the stack.".to_owned());
    }
    for robot in &listing.robots {
        lines.push(robot_line(
            robot,
            listing
                .leases
                .iter()
                .find(|lease| lease.robot == robot.name),
        ));
    }
    lines.join("\n")
}

/// One robot: its name, option, machine, kind and software, its robot
/// commander, and its lease.
fn robot_line(robot: &FleetRobot, lease: Option<&FleetLease>) -> String {
    let kind = match robot.kind {
        RobotKind::Simulated => "simulated",
        RobotKind::Physical => "physical",
    };
    let software = match robot.software {
        SoftwareState::Running => "running",
        SoftwareState::Stopped => "stopped",
    };
    let commander = if robot.robot_commander.is_empty() {
        String::new()
    } else {
        format!(", robot commander `{}`", robot.robot_commander)
    };
    let lease = match lease {
        None => "no lease".to_owned(),
        Some(lease) => {
            let open = if lease.open { ", open" } else { "" };
            format!(
                "leased by `{}` on `{}` for `{}`{open}",
                lease.holder, lease.holder_core_node, lease.name
            )
        }
    };
    format!(
        "  {} ({} on `{}`, {kind}, software {software}{commander}): {lease}",
        robot.name, robot.option, robot.core_node
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn robot(name: &str, software: SoftwareState, commander: &str) -> FleetRobot {
        FleetRobot {
            name: name.to_owned(),
            option: "openarm_sim".to_owned(),
            core_node: "cn-desk".to_owned(),
            kind: RobotKind::Simulated,
            clock: "sim".to_owned(),
            software,
            robot_commander: commander.to_owned(),
            instances: Vec::new(),
            contracts: Vec::new(),
        }
    }

    #[test]
    fn the_listing_gives_the_halt_and_one_line_per_robot_with_its_lease() {
        let listing = FleetListing {
            version: "0.35.0".to_owned(),
            core_node: "cn-desk".to_owned(),
            launcher: "simulation_mcp".to_owned(),
            halted: true,
            halted_by: "`peppy-cli` on `cn-desk`".to_owned(),
            robots: vec![
                robot("alpha", SoftwareState::Running, "web_commander"),
                robot("bravo", SoftwareState::Stopped, ""),
            ],
            leases: vec![FleetLease {
                robot: "alpha".to_owned(),
                holder: "director_inst".to_owned(),
                holder_core_node: "cn-desk".to_owned(),
                name: "tidy".to_owned(),
                open: true,
                giver: String::new(),
                giver_core_node: String::new(),
                receiver: String::new(),
                receiver_core_node: String::new(),
            }],
            ..FleetListing::default()
        };
        assert_eq!(
            render_listing(&listing),
            "Fleet of `cn-desk` (launcher `simulation_mcp`, peppy 0.35.0)\n\
             HALTED by `peppy-cli` on `cn-desk`: no robot moves until `peppy fleet clear-halt`\n  \
             alpha (openarm_sim on `cn-desk`, simulated, software running, robot commander \
             `web_commander`): leased by `director_inst` on `cn-desk` for `tidy`, open\n  \
             bravo (openarm_sim on `cn-desk`, simulated, software stopped): no lease"
        );
    }

    #[test]
    fn an_empty_listing_says_that_no_robot_is_on_the_stack() {
        let listing = FleetListing {
            version: "unknown".to_owned(),
            core_node: "cn-desk".to_owned(),
            ..FleetListing::default()
        };
        assert_eq!(
            render_listing(&listing),
            "Fleet of `cn-desk` (launcher `none`, peppy unknown)\nNo robot is on the stack."
        );
    }
}
