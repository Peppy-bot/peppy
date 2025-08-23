mod builder;
mod messaging;
mod node_watcher;
mod router;
mod types;

use super::Command;
use crate::Result;
use builder::ServeCommandBuilder;

pub struct ServeCommand {
    pub engine: String,
    pub host: Option<String>,
    pub port: Option<u16>,
}

impl Command for ServeCommand {
    fn execute(self) -> Result<()> {
        match self.engine.as_str() {
            "zenoh" => {
                let host = self.host.as_deref().unwrap_or("127.0.0.1");
                let port = self.port.unwrap_or(7447);
                println!("Launching nodes with Zenoh engine on: {}:{}", host, port);
            }
            _ => {
                println!("Unknown engine");
                std::process::exit(1);
            }
        }
        // TODO Run a separate thread that listen to Zenoh communication so that it can internally create a map of those communication between nodes
        // TODO Run a separate thread that is a web server API that display the node communication, node list etc...

        let executor = ServeCommandBuilder::new(self.engine, self.host, self.port)
            .with_node_watcher()
            .with_messaging_router()
            // Future commands can be added here:
            // .with_async_command(Arc::new(ZenohListenerCommand::new(...)))
            // .with_async_command(Arc::new(WebApiCommand::new(...)))
            .build();

        if let Err(e) = executor.execute() {
            eprintln!("Serve command failed: {}", e);
        }
        Ok(())
    }
}
