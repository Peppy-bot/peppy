mod error;
mod factory;
mod messaging;
mod node_watcher;
mod types;

use factory::MessagingFactory;
use types::MessagingConfiguration;

use super::{Command, CommandError};

pub struct ServeCommand {
    pub host: String,
    pub port: u16,
}

impl Command for ServeCommand {
    fn execute(self) -> Result<(), CommandError> {
        println!("Launching nodes on: {}:{}", &self.host, self.port);
        handle_serve(MessagingConfiguration::new(&self.host, self.port));
        Ok(())
    }
}

#[tokio::main]
async fn start_router(
    mut messenger: Box<dyn messaging::MessengerBackend>,
) -> Result<(), messaging::MessengerError> {
    messenger.start_router().await?;
    Ok(())
}

pub fn handle_serve(engine_configuration: MessagingConfiguration) {
    node_watcher::watch_node_configuration_files_changes();

    let messenger = MessagingFactory::build_messenger(engine_configuration);

    // Start the Zenoh router
    let router_thread = std::thread::spawn(move || {
        if let Err(e) = start_router(messenger) {
            eprintln!("Router error: {}", e);
        }
    });

    // Run a separate thread that listen to Zenoh communication so that it can internally create a map of those communication between nodes
    // Run a separate thread that is a web server API that display the node communication, node list etc...

    // Wait for router thread to complete
    if let Err(e) = router_thread.join() {
        eprintln!("Router thread panicked: {:?}", e);
    }
}
