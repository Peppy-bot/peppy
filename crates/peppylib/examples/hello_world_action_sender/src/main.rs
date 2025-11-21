use bytes::Bytes;
use colored::Colorize;
use config::{consts::DEFAULT_ZENOH_PORT, node::QoSProfile};
use peppylib::messaging::ActionGoalHandle;
use peppylib::{ActionMessenger, MessengerHandle, PeppyError};
use std::time::Duration;
use tokio::time::{sleep, timeout};

const ACTION_NAME: &str = "hello_action";
const NAMESPACE: &str = "/hello_ns";
const ACTION_INSTANCE_ID: &str = "hello_action_server";
const FEEDBACK_TIMEOUT: Duration = Duration::from_secs(5);
const GOAL_TIMEOUT: Duration = Duration::from_secs(3);
const CANCEL_TIMEOUT: Duration = Duration::from_secs(3);

async fn connect_messenger(host: &str, port: u16) -> MessengerHandle {
    MessengerHandle::from_host_port(host, port)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "failed to create action messenger on {host}:{port}: \
                 {error:?}.\n Did you start a zenohd server with the `zenohd_simple` example?"
            )
        })
}

async fn receive_feedback(handle: &mut ActionGoalHandle, goal_label: &str) {
    let feedback_result = {
        let subscription = handle.feedback_mut();
        timeout(FEEDBACK_TIMEOUT, subscription.rx.recv()).await
    };

    match feedback_result {
        Ok(Some(message)) => {
            let feedback_text = String::from_utf8_lossy(message.payload.as_ref());
            println!(
                "{}",
                format!("[FEEDBACK] Received feedback for {goal_label}: `{feedback_text}`")
                    .bold()
                    .yellow()
            );
        }
        Ok(None) => {
            println!(
                "{}",
                format!("[FEEDBACK] Feedback channel closed early for {goal_label}")
                    .bold()
                    .yellow()
            );
        }
        Err(_) => {
            println!(
                "{}",
                format!("[FEEDBACK] Timed out waiting for feedback for {goal_label}")
                    .bold()
                    .yellow()
            );
        }
    }
}

#[tokio::main]
async fn main() {
    let sender_handle = connect_messenger("127.0.0.1", DEFAULT_ZENOH_PORT).await;
    let as_instance_id = "action_client";

    println!(
        "{}",
        format!("[GOAL] Sending goal to `{ACTION_NAME}` action...")
            .bold()
            .green()
    );
    let mut goal_handle = ActionMessenger::send_goal(
        &sender_handle,
        as_instance_id,
        NAMESPACE,
        ACTION_NAME,
        Some(ACTION_INSTANCE_ID),
        Bytes::from_static(b"Hello from the action client"),
        QoSProfile::Reliable,
        GOAL_TIMEOUT,
    )
        .await
        .expect("Action goal should succeed");

    let goal_response_text = String::from_utf8_lossy(goal_handle.goal_response().as_ref());
    println!(
        "{}",
        format!("[GOAL] Received goal response: `{goal_response_text}`")
            .bold()
            .green()
    );

    receive_feedback(&mut goal_handle, "initial goal").await;

    let result_payload = ActionMessenger::poll_result(
        &sender_handle,
        as_instance_id,
        &goal_handle,
        Bytes::from_static(b"request result after completion"),
        GOAL_TIMEOUT,
    )
        .await
        .expect("Action result should be available");
    let result_text = String::from_utf8_lossy(result_payload.as_ref());
    println!(
        "{}",
        format!("[RESULT] Received result: `{result_text}`")
            .bold()
            .cyan()
    );

    println!("Waiting before sending cancellable goal...");
    sleep(Duration::from_secs(2)).await;

    println!("{}", "[GOAL] Sending cancellable goal...".bold().green());
    let mut cancellable_goal_handle = ActionMessenger::send_goal(
        &sender_handle,
        as_instance_id,
        NAMESPACE,
        ACTION_NAME,
        Some(ACTION_INSTANCE_ID),
        Bytes::from_static(b"This goal will be cancelled"),
        QoSProfile::Reliable,
        GOAL_TIMEOUT,
    )
        .await
        .expect("Cancellable action goal should succeed");

    let cancel_goal_response =
        String::from_utf8_lossy(cancellable_goal_handle.goal_response().as_ref());
    println!(
        "{}",
        format!("[GOAL] Received goal response: `{cancel_goal_response}`")
            .bold()
            .green()
    );

    receive_feedback(&mut cancellable_goal_handle, "cancellable goal").await;

    println!("Waiting before issuing cancel request...");
    sleep(Duration::from_secs(2)).await;

    let cancel_response = ActionMessenger::cancel_goal(
        &sender_handle,
        as_instance_id,
        &cancellable_goal_handle,
        CANCEL_TIMEOUT,
    )
        .await
        .expect("Cancel request should succeed");
    let cancel_text = String::from_utf8_lossy(cancel_response.as_ref());
    println!(
        "{}",
        format!("[CANCEL] Received cancel response: `{cancel_text}`")
            .bold()
            .magenta()
    );

    println!(
        "{}",
        "[RESULT] Attempting to request result after cancellation..."
            .bold()
            .cyan()
    );

    match ActionMessenger::poll_result(
        &sender_handle,
        as_instance_id,
        &cancellable_goal_handle,
        Bytes::from_static(b"result request after cancel"),
        GOAL_TIMEOUT,
    )
        .await
    {
        Ok(result_payload) => {
            let result_text = String::from_utf8_lossy(result_payload.as_ref());
            panic!(
                "Received result `{result_text}` even though the goal was cancelled. \
                 The action should stop responding to this goal."
            );
        }
        Err(PeppyError::ActionResultTimeout { .. })
        | Err(PeppyError::ActionResultUnreachable { .. }) => {
            println!(
                "{}",
                "[RESULT] No result returned after cancellation, as expected."
                    .bold()
                    .cyan()
            );
        }
        Err(error) => {
            panic!("Unexpected error after cancelling goal: {error:?}");
        }
    }

    println!(
        "{}",
        "Action sender finished exercising goal, feedback, result, and cancel flows."
            .bold()
            .white()
    );
}
