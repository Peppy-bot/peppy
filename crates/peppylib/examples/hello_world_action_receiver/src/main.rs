use bytes::Bytes;
use chrono::Local;
use colored::Colorize;
use config::consts::DEFAULT_ZENOH_PORT;
use names_generator2::get_random;
use peppylib::messaging::{ActionCreation, ServiceRequestContext, TopicPublisher};
use peppylib::{ActionMessenger, MessengerHandle, PeppyResult};
use rand::rng;
use std::sync::Arc;
use tokio::signal;
use tokio::sync::Mutex;

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
    let request_id = request.request_id();
    let instance_id = request.message().instance_id();
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
    let request_id = request.request_id();
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
    let request_id = request.request_id();
    let instance_id = request.message().instance_id();
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

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum LoopState {
    WaitForGoal,
    WaitForFollowups,
    Shutdown,
}

fn log_ctrl_c() {
    println!("{}", "[ACTION] Received CTRL+C, exiting.".bold().white());
}

fn log_listener_closed(listener: &str) {
    println!(
        "{}",
        format!("[ACTION] {listener} listener closed by client.")
            .bold()
            .white()
    );
}

fn log_handle_error(listener: &str, error: &impl std::fmt::Debug) {
    eprintln!(
        "{}",
        format!("[ERROR] Failed to handle {listener} request: {error:?}")
            .bold()
            .red()
    );
}

async fn set_active_goal(
    active_caller_instance: &Arc<Mutex<Option<String>>>,
    caller_instance: &str,
) {
    let mut active_instance = active_caller_instance.lock().await;
    *active_instance = Some(caller_instance.to_string());
}

async fn clear_active_goal(active_caller_instance: &Arc<Mutex<Option<String>>>) {
    let mut active_instance = active_caller_instance.lock().await;
    *active_instance = None;
}

async fn has_active_goal(active_caller_instance: &Arc<Mutex<Option<String>>>) -> bool {
    active_caller_instance.lock().await.is_some()
}

async fn matches_active_goal(
    active_caller_instance: &Arc<Mutex<Option<String>>>,
    caller_instance: &str,
) -> bool {
    let active_instance = active_caller_instance.lock().await;
    match &*active_instance {
        Some(active) => active == caller_instance,
        None => false,
    }
}

async fn wait_for_goal(
    action: &mut ActionCreation,
    active_caller_instance: &Arc<Mutex<Option<String>>>,
) -> LoopState {
    let feedback_publisher = &action.feedback_publisher;
    let goal_outcome = tokio::select! {
        _ = signal::ctrl_c() => {
            log_ctrl_c();
            return LoopState::Shutdown;
        }
        result = action.goal_service.handle_next_request({
            let feedback_publisher = feedback_publisher;
            let active_caller_instance = Arc::clone(active_caller_instance);
            move |request| {
                let feedback_publisher = feedback_publisher;
                let active_caller_instance = Arc::clone(&active_caller_instance);
                async move {
                    set_active_goal(&active_caller_instance, request.message().instance_id()).await;
                    handle_goal_request(request, feedback_publisher).await
                }
            }
        }) => result,
    };

    match goal_outcome {
        Ok(true) => LoopState::WaitForFollowups,
        Ok(false) => {
            log_listener_closed("Goal");
            LoopState::Shutdown
        }
        Err(error) => {
            log_handle_error("goal", &error);
            LoopState::Shutdown
        }
    }
}

async fn handle_followups(
    action: &mut ActionCreation,
    active_caller_instance: &Arc<Mutex<Option<String>>>,
) -> LoopState {
    loop {
        tokio::select! {
            _ = signal::ctrl_c() => {
                log_ctrl_c();
                return LoopState::Shutdown;
            }
            cancel_result = action
                .cancel_service
                .handle_next_request({
                    let active_caller_instance = Arc::clone(active_caller_instance);
                    move |request| {
                        let active_caller_instance = Arc::clone(&active_caller_instance);
                        async move {
                            let caller_instance = request.message().instance_id();
                            if !matches_active_goal(&active_caller_instance, caller_instance).await {
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
                            clear_active_goal(&active_caller_instance).await;
                            Ok(response)
                        }
                    }
                }) => {
                    match cancel_result {
                        Ok(true) => {}
                        Ok(false) => {
                            log_listener_closed("Cancel");
                            return LoopState::Shutdown;
                        }
                        Err(error) => {
                            log_handle_error("cancel", &error);
                            return LoopState::Shutdown;
                        }
                    }
                }
            result_result = action
                .result_service
                .handle_next_request({
                    let active_caller_instance = Arc::clone(active_caller_instance);
                    move |request| {
                        let active_caller_instance = Arc::clone(&active_caller_instance);
                        async move {
                            let caller_instance = request.message().instance_id();
                            if !matches_active_goal(&active_caller_instance, caller_instance).await {
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
                            clear_active_goal(&active_caller_instance).await;
                            Ok(response)
                        }
                    }
                }) => {
                    match result_result {
                        Ok(true) => {}
                        Ok(false) => {
                            log_listener_closed("Result");
                            return LoopState::Shutdown;
                        }
                        Err(error) => {
                            log_handle_error("result", &error);
                            return LoopState::Shutdown;
                        }
                    }
                }
        };

        if !has_active_goal(active_caller_instance).await {
            return LoopState::WaitForGoal;
        }
    }
}

async fn run_action_loop(mut action: ActionCreation) {
    let active_caller_instance = Arc::new(Mutex::new(None::<String>));
    let mut state = LoopState::WaitForGoal;

    while state != LoopState::Shutdown {
        state = match state {
            LoopState::WaitForGoal => wait_for_goal(&mut action, &active_caller_instance).await,
            LoopState::WaitForFollowups => {
                handle_followups(&mut action, &active_caller_instance).await
            }
            LoopState::Shutdown => LoopState::Shutdown,
        };
    }
}

#[tokio::main]
async fn main() {
    let receiver_handle = connect_messenger("127.0.0.1", DEFAULT_ZENOH_PORT).await;
    let as_instance_id = format!("{}_listener", get_random(rng()));

    let action = ActionMessenger::listen(&receiver_handle, NODE_NAME, ACTION_NAME, &as_instance_id)
        .await
        .expect("Should expose the action");

    println!(
        "{}",
        format!("[ACTION] Waiting for action goals as {as_instance_id}... Press CTRL+C to stop.")
            .bold()
            .white()
    );

    run_action_loop(action).await;

    println!(
        "{}",
        "[ACTION] Action receiver shutting down.".bold().white()
    );
}
