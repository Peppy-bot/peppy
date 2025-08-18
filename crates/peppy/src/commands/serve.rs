mod node_watcher;
mod pubsub;

use pubsub::{DynMessenger, MessengerError, ZenohBackend};

#[tokio::main]
async fn start_messenger(backend: Box<dyn pubsub::MessengerBackend>) -> Result<(), MessengerError> {
    let messenger = DynMessenger::new(backend).await?;
    messenger.publish("foo", b"bar").await?;
    Ok(())
}

pub fn handle_serve(_host: &str, _zenoh_port: u16) {
    node_watcher::watch_node_configuration_files_changes();

    // Start messenger in a separate thread
    let messenger_thread = std::thread::spawn(|| {
        let backend = Box::new(ZenohBackend::default());
        if let Err(e) = start_messenger(backend) {
            eprintln!("Messenger error: {}", e);
        }
    });

    // Run a separate thread that starts a Zenoh router
    // Run a separate thread that listen to Zenoh communication so that it can internally create a map of those communication between nodes
    // Run a separate thread that is a web server API that display the node communication, node list etc...

    // Wait for messenger thread to complete
    if let Err(e) = messenger_thread.join() {
        eprintln!("Messenger thread panicked: {:?}", e);
    }
}
