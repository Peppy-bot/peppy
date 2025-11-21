use bytes::Bytes;
use chrono::Local;
use colored::Colorize;
use config::consts::DEFAULT_ZENOH_PORT;
use names_generator2::get_random;
use peppylib::messaging::{ServiceRequestContext, TopicPublisher};
use peppylib::{ActionMessenger, MessengerHandle, PeppyResult};
use rand::rng;
use tokio::signal;
use tokio::sync::Mutex;
use std::sync::Arc;

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

fn payload_as_text(request: &ServiceRequestContext) -> String {
    let payload = request.message().payload();
    String::from_utf8_lossy(payload.as_bytes().as_ref()).to_string()
}

async fn handle_goal_request(
    request: ServiceRequestContext,
    feedback_publisher: &TopicPublisher,
) -> PeppyResult<Bytes> {
    let request_id = request.request_id().unwrap_or("unknown");
    let instance_id = request.message().instance_id().unwrap_or("unknown");
    let payload_text = payload_as_text(&request);

    let timestamp = current_timestamp();
    println!(
        "{}",
        format!("[GOAL] [{timestamp}] Received goal `{request_id}` from `{instance_id}` with payload `{payload_text}`")
            .bold()
            .green()
    );

    let feedback_text = format!("feedback: working on `{payload_text}`");
    feedback_publisher
        .publish(Bytes::from(feedback_text.clone()))
        .await?;

    let timestamp = current_timestamp();
    println!(
        "{}",
        format!(
            "[FEEDBACK] [{timestamp}] Published feedback `{feedback_text}` for goal `{request_id}`"
        )
        .bold()
        .yellow()
    );

    let response_text = format!("goal accepted: {payload_text}");

    let timestamp = current_timestamp();
    println!(
        "{}",
        format!("[GOAL] [{timestamp}] Responding to goal `{request_id}` with `{response_text}`")
            .bold()
            .green()
    );

    Ok(Bytes::from(response_text))
}

async fn handle_cancel_request(request: ServiceRequestContext) -> PeppyResult<Bytes> {
    let request_id = request.request_id().unwrap_or("unknown");
    let timestamp = current_timestamp();
    println!(
        "{}",
        format!("[CANCEL] [{timestamp}] Received cancel request for goal `{request_id}`")
            .bold()
            .magenta()
    );

    if !request.message().payload().is_empty() {
        let timestamp = current_timestamp();
        println!(
            "{}",
            format!(
                "[CANCEL] [{timestamp}] Cancel payload `{}` will be ignored.",
                payload_as_text(&request)
            )
            .bold()
            .magenta()
        );
    }

    let response_text = format!("cancel acknowledged for goal `{request_id}`");
    let timestamp = current_timestamp();
    println!(
        "{}",
        format!("[CANCEL] [{timestamp}] Responding to cancel request with `{response_text}`")
            .bold()
            .magenta()
    );

    Ok(Bytes::from(response_text))
}

async fn handle_result_request(request: ServiceRequestContext) -> PeppyResult<Bytes> {
    let request_id = request.request_id().unwrap_or("unknown");
    let instance_id = request.message().instance_id().unwrap_or("unknown");
    let payload_text = payload_as_text(&request);

    let timestamp = current_timestamp();
    println!(
        "{}",
        format!(
            "[RESULT] [{timestamp}] Received result request `{request_id}` from `{instance_id}` with payload `{payload_text}`"
        )
        .bold()
        .cyan()
    );

    let response_text = format!("result: `{payload_text}` -> success");
    let timestamp = current_timestamp();
    println!(
        "{}",
        format!(
            "[RESULT] [{timestamp}] Responding to result request `{request_id}` with `{response_text}`"
        )
        .bold()
        .cyan()
    );

    Ok(Bytes::from(response_text))
}

#[tokio::main]
async fn main() {
    let receiver_handle = connect_messenger("127.0.0.1", DEFAULT_ZENOH_PORT).await;
    let as_instance_id = format!("{}_listener", get_random(rng()));

    let mut action =
        ActionMessenger::listen(&receiver_handle, NODE_NAME, ACTION_NAME, &as_instance_id)
            .await
            .expect("Should expose the action");

    println!(
        "{}",
        format!("[ACTION] Waiting for action goals as {as_instance_id}... Press CTRL+C to stop.")
            .bold()
            .white()
    );

    let active_caller_instance = Arc::new(Mutex::new(None::<String>));
    let mut awaiting_followup = false;
    let mut shutting_down = false;
    while !shutting_down {
        if !awaiting_followup {
            let feedback_publisher = &action.feedback_publisher;
            let goal_outcome = tokio::select! {
                _ = signal::ctrl_c() => {
                    println!(
                        "{}",
                        "[ACTION] Received CTRL+C, exiting."
                            .bold()
                            .white()
                    );
                    shutting_down = true;
                    continue;
                }
                result = action.goal_service.handle_next_request({
                    let feedback_publisher = feedback_publisher;
                    let active_caller_instance = Arc::clone(&active_caller_instance);
                    move |request| {
                        let feedback_publisher = feedback_publisher;
                        let active_caller_instance = Arc::clone(&active_caller_instance);
                        async move {
                            {
                                let mut active_instance = active_caller_instance.lock().await;
                                *active_instance =
                                    request.message().instance_id().map(str::to_string);
                            }
                            handle_goal_request(request, feedback_publisher).await
                        }
                    }
                }) => result,
            };

            match goal_outcome {
                Ok(true) => awaiting_followup = true,
                Ok(false) => {
                    println!(
                        "{}",
                        "[ACTION] Goal listener closed by client.".bold().white()
                    );
                    shutting_down = true;
                }
                Err(error) => {
                    eprintln!(
                        "{}",
                        format!("[ERROR] Failed to handle goal request: {error:?}")
                            .bold()
                            .red()
                    );
                    shutting_down = true;
                }
            }
        }

        while awaiting_followup && !shutting_down {
            tokio::select! {
                _ = signal::ctrl_c() => {
                    println!(
                        "{}",
                        "[ACTION] Received CTRL+C, exiting."
                            .bold()
                            .white()
                    );
                    shutting_down = true;
                }
                cancel_result = action
                    .cancel_service
                    .handle_next_request({
                        let active_caller_instance = Arc::clone(&active_caller_instance);
                        move |request| {
                            let active_caller_instance = Arc::clone(&active_caller_instance);
                            async move {
                                let caller_instance =
                                    request.message().instance_id().map(str::to_string);
                                let matches_active_goal = {
                                    let active_instance = active_caller_instance.lock().await;
                                    match (&*active_instance, &caller_instance) {
                                        (Some(active), Some(caller)) => active == caller,
                                        _ => false,
                                    }
                                };

                                if !matches_active_goal {
                                    println!(
                                        "{}",
                                        "[CANCEL] Ignoring cancel request for inactive goal."
                                            .bold()
                                            .magenta()
                                    );
                                    return Ok(Bytes::from_static(
                                        b"cancel ignored: no active goal for caller",
                                    ));
                                }

                                let response = handle_cancel_request(request).await?;
                                let mut active_instance = active_caller_instance.lock().await;
                                *active_instance = None;
                                Ok(response)
                            }
                        }
                    }) => {
                        match cancel_result {
                            Ok(true) => {
                                let still_waiting = active_caller_instance.lock().await.is_some();
                                awaiting_followup = still_waiting;
                            }
                            Ok(false) => {
                                println!(
                                    "{}",
                                    "[ACTION] Cancel listener closed by client."
                                        .bold()
                                        .white()
                                );
                                shutting_down = true;
                            }
                            Err(error) => {
                                eprintln!(
                                    "{}",
                                    format!("[ERROR] Failed to handle cancel request: {error:?}")
                                        .bold()
                                        .red()
                                );
                                shutting_down = true;
                            }
                        }
                    }
                result_result = action
                    .result_service
                    .handle_next_request({
                        let active_caller_instance = Arc::clone(&active_caller_instance);
                        move |request| {
                            let active_caller_instance = Arc::clone(&active_caller_instance);
                            async move {
                                let caller_instance =
                                    request.message().instance_id().map(str::to_string);
                                let matches_active_goal = {
                                    let active_instance = active_caller_instance.lock().await;
                                    match (&*active_instance, &caller_instance) {
                                        (Some(active), Some(caller)) => active == caller,
                                        _ => false,
                                    }
                                };

                                if !matches_active_goal {
                                    println!(
                                        "{}",
                                        "[RESULT] Ignoring result request for inactive goal."
                                            .bold()
                                            .cyan()
                                    );
                                    return Ok(Bytes::from_static(
                                        b"result ignored: no active goal for caller",
                                    ));
                                }

                                let response = handle_result_request(request).await?;
                                let mut active_instance = active_caller_instance.lock().await;
                                *active_instance = None;
                                Ok(response)
                            }
                        }
                    }) => {
                        match result_result {
                            Ok(true) => {
                                let still_waiting = active_caller_instance.lock().await.is_some();
                                awaiting_followup = still_waiting;
                            }
                            Ok(false) => {
                                println!(
                                    "{}",
                                    "[ACTION] Result listener closed by client."
                                        .bold()
                                        .white()
                                );
                                shutting_down = true;
                            }
                            Err(error) => {
                                eprintln!(
                                    "{}",
                                    format!("[ERROR] Failed to handle result request: {error:?}")
                                        .bold()
                                        .red()
                                );
                                shutting_down = true;
                            }
                        }
                    }
            };
        }
    }

    println!(
        "{}",
        "[ACTION] Action receiver shutting down.".bold().white()
    );
}
