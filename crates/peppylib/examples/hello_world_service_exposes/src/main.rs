use chrono::Local;
use config::consts::DEFAULT_MESSAGING_PORT;
use names_generator2::get_random;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, Payload, PeppyResult, ServiceMessenger};
use rand::rng;
use tokio::signal;

const SERVICE_NAME: &str = "hello_service";
const NODE_NAME: &str = "hello_node";

async fn connect_messenger(host: &str, port: u16) -> MessengerHandle {
    MessengerHandle::from_host_port(host, port)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "failed to create service messenger on {host}:{port}: \
                 {error:?}. Did you start a zenohd server with the `zenohd_simple` example?"
            )
        })
}

fn current_timestamp() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn payload_as_text(request: &ServiceRequestContext) -> String {
    let payload = request.message().payload();
    String::from_utf8_lossy(payload.as_ref()).to_string()
}

async fn handle_request(request: ServiceRequestContext) -> PeppyResult<Payload> {
    let payload_text = payload_as_text(&request);
    let instance_id = request.message().instance_id();
    let daemon_node = request.message().daemon_node();

    println!(
        "[{}] Received request with payload `{payload_text}` from `{instance_id}` and daemon node `{daemon_node}`",
        current_timestamp()
    );

    let response_text = format!("ack: {payload_text}");
    println!(
        "[{}] Responding with `{response_text}`",
        current_timestamp()
    );

    Ok(Payload::from(response_text))
}

fn handle_service_result(result: PeppyResult<bool>) -> bool {
    match result {
        Ok(true) => true,
        Ok(false) => {
            println!("Service listener closed by client.");
            false
        }
        Err(error) => {
            eprintln!("Failed to handle service request: {error:?}");
            false
        }
    }
}

#[tokio::main]
async fn main() {
    // Create a messenger for the receiving node.
    let receiver_handle = connect_messenger("127.0.0.1", DEFAULT_MESSAGING_PORT).await;
    let daemon_node = format!("{}_daemon", get_random(rng()));
    let instance_id = format!("{}_listener", get_random(rng()));

    let mut service = ServiceMessenger::listen(
        &receiver_handle,
        &daemon_node,
        &instance_id,
        NODE_NAME,
        SERVICE_NAME,
    )
    .await
    .expect("Should expose the service");

    println!("Waiting for service requests as instance_id {instance_id}... Press CTRL+C to stop.");
    loop {
        tokio::select! {
            _ = signal::ctrl_c() => {
                println!("Received CTRL+C, exiting.");
                break;
            }
            result = service.handle_next_request(handle_request) => {
                if !handle_service_result(result) {
                    break;
                }
            }
        }
    }
}
