use chrono::Local;
use colored::Colorize;
use config::consts::DEFAULT_MESSAGING_PORT;
use names_generator2::get_random;
use peppylib::messaging::{ConcurrentAction, GoalContext, NonEmptyPayload, SenderTarget};
use peppylib::{MessengerHandle, Payload};
use rand::rng;
use std::time::Duration;
use tokio::signal;

const NODE_NAME: &str = "hello_node";
const ACTION_NAME: &str = "hello_action";

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

fn log_ctrl_c() {
    println!("{}", "[ACTION] Received CTRL+C, exiting.".bold().white());
}

fn log_listener_closed() {
    println!(
        "{}",
        "[ACTION] Goal listener closed by client.".bold().white()
    );
}

fn log_handle_error(context: &str, error: &impl std::fmt::Debug) {
    eprintln!(
        "{}",
        format!("[ERROR] Failed to {context}: {error:?}")
            .bold()
            .red()
    );
}

/// Drives a single accepted goal to completion. Spawned once per goal so many
/// goals make progress concurrently — each owns its own `GoalContext`, so its
/// feedback, cancel signal, and result never cross another goal's streams.
async fn drive_goal(ctx: GoalContext) {
    let request_text = String::from_utf8_lossy(ctx.request_bytes()).to_string();
    let goal_id = ctx.goal_id().to_string();
    println!(
        "{}",
        format!(
            "[GOAL] [{}] Accepted goal `{goal_id}` for `{request_text}`",
            current_timestamp()
        )
        .bold()
        .green()
    );

    // Feedback goes through this goal's context, not a shared slot.
    let feedback_text = format!("working on `{request_text}`");
    if let Ok(payload) = NonEmptyPayload::try_new(Payload::from(feedback_text.clone().into_bytes()))
    {
        if let Err(error) = ctx.publish_feedback(payload).await {
            log_handle_error("publish feedback", &error);
        } else {
            println!(
                "{}",
                format!(
                    "[FEEDBACK] [{}] Published `{feedback_text}` for goal `{goal_id}`",
                    current_timestamp()
                )
                .bold()
                .yellow()
            );
        }
    }

    // Simulate long-running work that can be cancelled mid-flight.
    tokio::select! {
        _ = ctx.cancel_signal() => {
            println!(
                "{}",
                format!("[CANCEL] [{}] Goal `{goal_id}` cancelled", current_timestamp())
                    .bold()
                    .magenta()
            );
            let _ = ctx.complete_cancelled(Payload::from_static(b"CANCELLED")).await;
        }
        _ = tokio::time::sleep(Duration::from_secs(2)) => {
            let result_text = format!("SUCCESS: {request_text}");
            if let Err(error) = ctx.complete(Payload::from(result_text.into_bytes())).await {
                log_handle_error("deliver result", &error);
            } else {
                println!(
                    "{}",
                    format!("[RESULT] [{}] Goal `{goal_id}` completed", current_timestamp())
                        .bold()
                        .cyan()
                );
            }
        }
    }
}

/// Accepts goals forever, spawning a worker per goal. The loop only waits for
/// the next goal — cancel and result requests are routed to the right goal by
/// the engine, so a slow goal never blocks accepting new ones.
async fn run_action_loop(mut action: ConcurrentAction) {
    loop {
        let pending = tokio::select! {
            _ = signal::ctrl_c() => {
                log_ctrl_c();
                return;
            }
            recv = action.recv_next_goal() => recv,
        };

        match pending {
            Ok(Some(pending)) => match pending.accept(Payload::from_static(b"goal accepted")).await
            {
                Ok(ctx) => {
                    tokio::spawn(drive_goal(ctx));
                }
                Err(error) => log_handle_error("accept goal", &error),
            },
            Ok(None) => {
                log_listener_closed();
                return;
            }
            Err(error) => {
                log_handle_error("receive goal", &error);
                return;
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let receiver_handle = connect_messenger("127.0.0.1", DEFAULT_MESSAGING_PORT).await;
    let core_node_name = format!("{}_core", get_random(rng()));
    let as_instance_id = format!("{}_listener", get_random(rng()));

    let action = ConcurrentAction::expose(
        &receiver_handle,
        &core_node_name,
        &as_instance_id,
        SenderTarget::node(NODE_NAME, "v1").expect("test target"),
        ACTION_NAME,
        true, // this action publishes feedback
    )
    .await
    .expect("Should expose the action");

    println!(
        "{}",
        format!("[ACTION] Waiting for action goals as `{as_instance_id}` and core node `{core_node_name}`... Press CTRL+C to stop.")
            .bold()
            .white()
    );

    run_action_loop(action).await;

    println!(
        "{}",
        "[ACTION] Action receiver shutting down.".bold().white()
    );
}
