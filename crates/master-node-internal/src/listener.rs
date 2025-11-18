use std::sync::Arc;

use crate::{Error, Result};
use pmi::{Messenger, MessengerBackend, SubscriberQoS};
use tokio::sync::Mutex;
use tracing::info;

pub async fn start_node(messenger: Arc<Mutex<Messenger>>) -> Result<()> {
    info!("Starting commands listener...");

    let mut subscription = {
        let messenger = messenger.lock().await;
        messenger
            .subscribe(
                config::consts::PEPPYD_COMMANDS_TOPIC,
                SubscriberQoS::Standard,
            )
            .await
    }
    .map_err(Error::PeppyMessagingInterface)?;

    let shutdown_signal = tokio::signal::ctrl_c();
    tokio::pin!(shutdown_signal);

    loop {
        tokio::select! {
            ctrl_c_result = &mut shutdown_signal => {
                ctrl_c_result.map_err(Error::from)?;
                break;
            }
            maybe_message = subscription.on_next_message() => {
                match maybe_message {
                    Some(message) => {
                        let payload = String::from_utf8_lossy(message.payload.as_ref());
                        let command = payload.trim();

                        match command {
                            "ping" => info!("Received 'ping' command over {}", message.topic),
                            "status" => info!("Would respond with status for {}", message.topic),
                            "shutdown" => info!("Received 'shutdown' command (toy example)"),
                            other => info!("Received unhandled command '{}'", other),
                        }
                    }
                    None => {
                        info!("Command subscription closed; no longer listening for messages");
                        break;
                    }
                }
            }
        }
    }

    info!("Shutting down commands listener...");
    Ok(())
}
