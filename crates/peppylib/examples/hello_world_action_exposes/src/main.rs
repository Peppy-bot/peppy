//! Concurrent action server example.
//!
//! Exposes a single action and serves many goals at once: the accept loop
//! registers each goal, replies "accepted", and hands the goal's
//! `GoalContext` to an independent worker task before going back to accept the
//! next goal. Each worker streams progress feedback on its own goal's stream
//! while watching that goal's cancel signal, then delivers a result. Because
//! every goal owns a separate context, two clients fire goals that progress in
//! parallel, and cancelling one goal never disturbs another.
//!
//! Pair this with the `hello_world_action_subscribes` example (run two
//! subscribers at once to see the concurrency) and a `zenohd` router.

use chrono::Local;
use colored::Colorize;
use config::consts::DEFAULT_MESSAGING_PORT;
use names_generator2::get_random;
use peppylib::messaging::{ActionServer, GoalContext, NonEmptyPayload, SenderTarget};
use peppylib::{ActionMessenger, MessengerHandle, Payload};
use rand::rng;
use std::time::Duration;
use tokio::signal;

const NODE_NAME: &str = "hello_node";
const ACTION_NAME: &str = "hello_action";

/// How long each simulated work step takes between feedback messages.
const STEP_DELAY: Duration = Duration::from_millis(400);

async fn connect_messenger(host: &str, port: u16) -> MessengerHandle {
    MessengerHandle::from_host_port(host, port)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "failed to create action messenger on {host}:{port}: \
                 {error:?}. Did you start a zenohd server with the `zenohd_simple` example?"
            )
        })
}

fn current_timestamp() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Per-goal worker. Streams progress feedback for one goal while racing the
/// goal's cancel signal, then delivers the result. The goal's payload is the
/// name of the resource (e.g. a device) this goal targets; the framework has
/// already stripped the wire envelope, so `request_bytes` is the user payload.
async fn run_goal(ctx: GoalContext) {
    let target = String::from_utf8_lossy(ctx.request_bytes()).into_owned();
    println!(
        "{}",
        format!(
            "[GOAL] [{}] accepted goal `{}` for `{target}`",
            current_timestamp(),
            ctx.goal_id()
        )
        .bold()
        .green()
    );

    for percent in (20..=100).step_by(20) {
        tokio::select! {
            // Idempotent and resolves immediately once cancelled, so it is safe
            // to re-arm every iteration.
            _ = ctx.cancel_signal() => {
                println!(
                    "{}",
                    format!(
                        "[CANCEL] [{}] `{target}` cancelled at {percent}%; finishing early",
                        current_timestamp()
                    )
                    .bold()
                    .magenta()
                );
                // The worker decides how to react; here it completes with a
                // cancelled result. `complete` also closes the feedback stream.
                let _ = ctx
                    .complete(Payload::from(
                        format!("`{target}` cancelled at {percent}%").into_bytes(),
                    ))
                    .await;
                return;
            }
            _ = tokio::time::sleep(STEP_DELAY) => {
                let line = format!("`{target}` progress {percent}%");
                if let Ok(feedback) = NonEmptyPayload::try_new(Payload::from(line.clone().into_bytes())) {
                    let _ = ctx.publish_feedback(feedback).await;
                    println!(
                        "{}",
                        format!("[FEEDBACK] [{}] {line}", current_timestamp()).bold().yellow()
                    );
                }
            }
        }
    }

    let _ = ctx
        .complete(Payload::from(format!("`{target}` complete").into_bytes()))
        .await;
    println!(
        "{}",
        format!("[RESULT] [{}] `{target}` complete", current_timestamp())
            .bold()
            .cyan()
    );
}

/// Accept-and-spawn loop. Accepts the next goal, registers its context (so a
/// fast follow-up cancel/result cannot miss it), replies "accepted", and spawns
/// an independent worker. The loop returns to accepting immediately, so a
/// second goal never waits behind the first.
async fn run_action_server(mut server: ActionServer) {
    loop {
        let recv = tokio::select! {
            _ = signal::ctrl_c() => {
                println!("{}", "[ACTION] Received CTRL+C, exiting.".bold().white());
                break;
            }
            recv = server.recv_next_goal() => recv,
        };

        let (request, responder) = match recv {
            Ok(Some(pair)) => pair,
            Ok(None) => {
                println!("{}", "[ACTION] Goal service closed.".bold().white());
                break;
            }
            Err(error) => {
                eprintln!(
                    "{}",
                    format!("[ERROR] Failed to accept goal: {error:?}").bold().red()
                );
                break;
            }
        };

        // Register before replying "accepted": the client only sends
        // cancel/result after it sees acceptance.
        let ctx = match server.register_goal(&request).await {
            Ok(ctx) => ctx,
            Err(error) => {
                eprintln!(
                    "{}",
                    format!("[ERROR] Failed to register goal: {error:?}").bold().red()
                );
                let _ = responder.respond_error(error.to_string()).await;
                continue;
            }
        };

        if let Err(error) = responder.respond(Payload::from_static(b"accepted")).await {
            eprintln!(
                "{}",
                format!("[ERROR] Failed to reply to goal: {error:?}").bold().red()
            );
            continue;
        }

        // Hand the context to a worker and immediately accept the next goal.
        tokio::spawn(run_goal(ctx));
    }
}

#[tokio::main]
async fn main() {
    let receiver_handle = connect_messenger("127.0.0.1", DEFAULT_MESSAGING_PORT).await;
    let core_node_name = format!("{}_core", get_random(rng()));
    let as_instance_id = format!("{}_listener", get_random(rng()));

    let server = ActionMessenger::expose(
        &receiver_handle,
        &core_node_name,
        &as_instance_id,
        SenderTarget::node(NODE_NAME, "v1").expect("valid sender target"),
        ACTION_NAME,
    )
    .await
    .expect("Should expose the action");

    println!(
        "{}",
        format!(
            "[ACTION] Serving concurrent goals as `{as_instance_id}` and core node \
             `{core_node_name}`... Press CTRL+C to stop."
        )
        .bold()
        .white()
    );

    run_action_server(server).await;

    println!(
        "{}",
        "[ACTION] Action receiver shutting down.".bold().white()
    );
}
