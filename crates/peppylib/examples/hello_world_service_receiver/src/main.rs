use bytes::Bytes;
use chrono::Local;
use config::consts::DEFAULT_ZENOH_PORT;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{PeppyResult, ServiceMessenger};
use tokio::signal;

const SERVICE_NAME: &str = "hello_service";
const NAMESPACE: &str = "/hello_ns";

async fn connect_messenger(host: &str, port: u16) -> ServiceMessenger {
    ServiceMessenger::from_host_port(host, port)
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
    String::from_utf8_lossy(request.message.payload.as_ref()).to_string()
}

async fn handle_request(request: ServiceRequestContext) -> PeppyResult<Bytes> {
    let payload_text = payload_as_text(&request);

    println!(
        "[{}] Received request with payload `{payload_text}`",
        current_timestamp()
    );

    let response_text = format!("ack: {payload_text}");
    println!(
        "[{}] Responding with `{response_text}`",
        current_timestamp()
    );

    Ok(Bytes::from(response_text))
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
    let receiver_node = connect_messenger("127.0.0.1", DEFAULT_ZENOH_PORT).await;

    let mut service = receiver_node
        .listen(NAMESPACE, SERVICE_NAME)
        .await
        .expect("Should expose the service");

    println!("Waiting for service requests... Press CTRL+C to stop.");
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
