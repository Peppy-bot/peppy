use colored::Colorize;
use config::{consts::DEFAULT_MESSAGING_PORT, node::QoSProfile};
use names_generator2::get_random;
use peppylib::messaging::{ActionGoalHandle, ResultStatus, decode_cancel_ack};
use peppylib::{ActionMessenger, MessengerHandle, Payload};
use rand::rng;
use std::time::Duration;
use tokio::time::{sleep, timeout};
use peppylib::messaging::SenderTarget;

const NODE_NAME: &str = "hello_node";
const ACTION_NAME: &str = "hello_action";

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
    let Ok(feedback_result) = timeout(FEEDBACK_TIMEOUT, handle.on_next_feedback()).await else {
        println!(
            "{}",
            format!("[FEEDBACK] Timed out waiting for feedback for `{goal_label}`")
                .bold()
                .yellow()
        );
        return;
    };

    match feedback_result {
        Ok(message) => {
            let feedback_bytes = message.payload();
            let feedback_text = String::from_utf8_lossy(feedback_bytes.as_ref());
            let core_node = message.core_node();
            let instance_id = message.instance_id();
            println!(
                "{}",
                format!("[FEEDBACK] Received feedback for `{goal_label}` from `{instance_id}` and core node `{core_node}`: `{feedback_text}`")
                    .bold()
                    .yellow()
            );
        }
        Err(_) => {
            println!(
                "{}",
                format!("[FEEDBACK] Feedback channel closed early for `{goal_label}`")
                    .bold()
                    .yellow()
            );
        }
    }
}

#[tokio::main]
async fn main() {
    let sender_handle = connect_messenger("127.0.0.1", DEFAULT_MESSAGING_PORT).await;
    let core_node_name = format!("{}_core", get_random(rng()));
    let as_instance_id = format!("{}_listener", get_random(rng()));

    println!(
        "{}",
        format!("[GOAL] Sending goal to `{ACTION_NAME}` action as `{as_instance_id}` and core node `{core_node_name}`...")
            .bold()
            .green()
    );
    let mut goal_handle = ActionMessenger::send_goal(
        &sender_handle,
        &core_node_name,
        &as_instance_id,
        SenderTarget::node(NODE_NAME, "v1").expect("test target"),
        ACTION_NAME,
        None, // Binds with the first core node that is found
        None, // Binds with the first action that is found
        Payload::from_static(b"Hello from the action client"),
        QoSProfile::Reliable,
        GOAL_TIMEOUT,
    )

    .await
    .expect("Action goal should succeed");

    let goal_core_node = goal_handle.goal_response().core_node();
    let goal_instance_id = goal_handle.goal_response().instance_id();
    let goal_response_bytes = goal_handle.goal_response().payload();
    let goal_response_text = String::from_utf8_lossy(goal_response_bytes.as_ref());
    println!(
        "{}",
        format!("[GOAL] Received goal response from `{goal_instance_id}` and core node `{goal_core_node}`: `{goal_response_text}`")
            .bold()
            .green()
    );

    receive_feedback(&mut goal_handle, "initial goal").await;

    println!(
        "{}",
        format!("[RESULT] Requesting result payload...")
            .bold()
            .cyan()
    );
    let result =
        ActionMessenger::request_result(&sender_handle, &goal_handle, GOAL_TIMEOUT)
            .await
            .expect("Action result should be available");
    let result_text = String::from_utf8_lossy(result.body.as_ref());
    println!(
        "{}",
        format!("[RESULT] Received {:?} result: `{result_text}`", result.status)
            .bold()
            .cyan()
    );

    println!("Waiting before sending cancellable goal...");
    sleep(Duration::from_secs(2)).await;

    println!("{}", "[GOAL] Sending cancellable goal...".bold().green());
    let mut goal_handle = ActionMessenger::send_goal(
        &sender_handle,
        &core_node_name,
        &as_instance_id,
        SenderTarget::node(NODE_NAME, "v1").expect("test target"),
        ACTION_NAME,
        None, // Binds with the first core node that is found
        None, // Binds with the first action that is found
        Payload::from_static(b"This goal will be cancelled"),
        QoSProfile::Reliable,
        GOAL_TIMEOUT,
    )

    .await
    .expect("Cancellable action goal should succeed");

    let cancel_goal_response_bytes = goal_handle.goal_response().payload();
    let cancel_goal_response = String::from_utf8_lossy(cancel_goal_response_bytes.as_ref());
    println!(
        "{}",
        format!("[GOAL] Received goal response: `{cancel_goal_response}`")
            .bold()
            .green()
    );

    receive_feedback(&mut goal_handle, "cancellable goal").await;

    println!("Waiting before issuing cancel request...");
    sleep(Duration::from_secs(2)).await;

    let cancel_response =
        ActionMessenger::cancel_goal(&sender_handle, &goal_handle, CANCEL_TIMEOUT)
            .await
            .expect("Cancel request should succeed");
    let cancel_state =
        decode_cancel_ack(cancel_response.payload().as_ref()).expect("decode cancel state");
    println!(
        "{}",
        format!("[CANCEL] Received cancel state: `{cancel_state:?}`")
            .bold()
            .magenta()
    );

    println!(
        "{}",
        "[RESULT] Requesting the result after cancellation..."
            .bold()
            .cyan()
    );

    // After a cancel, `get_result` still resolves to a definitive typed outcome
    // (the worker here observes the cancel and reports `Cancelled`) rather than
    // erroring or hanging.
    let result = ActionMessenger::request_result(&sender_handle, &goal_handle, GOAL_TIMEOUT)
        .await
        .expect("cancelled goal still resolves to a typed outcome");
    let result_text = String::from_utf8_lossy(result.body.as_ref());
    match result.status {
        ResultStatus::Cancelled => println!(
            "{}",
            format!("[RESULT] Goal was cancelled, result: `{result_text}`")
                .bold()
                .cyan()
        ),
        ResultStatus::Abandoned => println!(
            "{}",
            "[RESULT] Goal was abandoned after the cancel."
                .bold()
                .cyan()
        ),
        other => panic!("Unexpected outcome after cancelling goal: {other:?}"),
    }

    println!(
        "{}",
        "Action sender finished exercising goal, feedback, result, and cancel flows."
            .bold()
            .white()
    );
}
