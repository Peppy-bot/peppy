use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Error, Debug)]
pub enum NodeCreationError {
    #[error("Failed to create directory: {0}")]
    DirectoryCreation(#[from] std::io::Error),

    #[error("Failed to get current directory")]
    CurrentDir(std::io::Error),

    #[error("Invalid node configuration: {}", .0)]
    InvalidConfig(String),
}

/// Creates a new node and updates the peppy.star configuration file where the command is run
pub fn create(
    node_name: &str,
    lang: &str,
    to_dir: Option<PathBuf>,
) -> Result<(), NodeCreationError> {
    let node_path = match to_dir {
        Some(dir) => dir,
        None => std::env::current_dir()
            .map_err(|e| NodeCreationError::CurrentDir(e))?
            .join(node_name),
    };

    fs::create_dir_all(&node_path)?;

    create_pixi_toml(&node_path, &lang)?;
    create_peppy_config(&node_path, &node_name)?;

    // TODO create the pixi venv and add the peppycl lib to it

    println!("Created node '{}' at: {}", node_name, node_path.display());

    Ok(())
}

fn create_pixi_toml(node_path: &Path, lang: &str) -> Result<(), NodeCreationError> {
    todo!("Create .gitignore with .peppy and .pixi in it");
    let pixi_toml_path = node_path.join("pixi.toml");
    let mut file = fs::File::create(pixi_toml_path)?;

    // TODO Use askama
    let pixi_content = r#"[project]
name = "peppy-node"
version = "0.1.0"
description = "A peppy node"
authors = ["peppy-user"]
channels = ["conda-forge"]
platforms = ["linux-64", "osx-64", "osx-arm64", "win-64"]

[dependencies]

[tasks]
"#;

    file.write_all(pixi_content.as_bytes())?;

    Ok(())
}

/// Convert a string to PascalCase for class naming
fn to_pascal_case(s: &str) -> String {
    s.split(|c: char| c == '_' || c == '-' || c == ' ')
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
            }
        })
        .collect()
}

fn get_node_create_template(node_name: &str) -> String {
    // Convert node name to PascalCase for class name
    let class_name = to_pascal_case(node_name);

    format!(
        r#"# Peppy node configuration file for {}

# Define the node class with a specific name for modularity
class {}Node:
    # Basic node identification
    name: str = "{}"
    namespace: str = "/"
    
    # Node lifecycle settings
    auto_start: bool = True
    respawn: bool = False
    respawn_delay: float = 2.0  # seconds
    
    # Communication interfaces
    publishes: list = [] # Example would be `publishes: list = [{{"type": "service", "parameters": []}}]`
    subscribe_from: list = [] # Example would be `subscribe_from: list = [{{"node_name": }}]`
    services: list = []
    actions: list = []
    
    # Parameters
    parameters: dict = {{}}
    parameter_overrides: dict = {{}}
    
    # QoS settings
    qos_profile: str = "default"  # options: "default", "sensor_data", "services", "parameters", "system_default"
    
    # Resource limits
    max_memory_mb: int = 512
    cpu_affinity: list = []  # CPU cores to bind to, empty means no binding
    
    # Logging configuration
    log_level: str = "info"  # options: "debug", "info", "warn", "error", "fatal"
    log_to_file: bool = False
    log_file_path: str = ""
    
    # Timing and scheduling
    update_rate_hz: float = 10.0
    priority: int = 0  # Process priority (-20 to 19, 0 is default)
    
    # Dependencies - can use load() to import other node definitions
    depends_on: list = []  # List of other nodes this node depends on
    
    # Custom initialization
    init_script: str = ""  # Path to initialization script
    
    # Monitoring and diagnostics
    enable_diagnostics: bool = True
    diagnostics_rate_hz: float = 1.0
    
    # Network configuration (for distributed systems)
    host: str = "localhost"
    port: int = 0  # 0 means auto-assign
    
    # Security
    enable_encryption: bool = False
    auth_required: bool = False

# Export the node class for use by other nodes
{}Node = {}Node

# Create instance - this will be the active node when this file is executed
node = {}Node()

# Example of loading dependencies from other nodes:
# load("../sensor_node/peppy.star", "SensorNode")
# load("../controller_node/peppy.star", "ControllerNode")
#
# # Configure this node to depend on others
# node.depends_on = ["sensor_node", "controller_node"]
#
# # Example publisher configuration
# node.publishers = [
#     {{
#         "topic": "/{}/output",
#         "msg_type": "std_msgs/String",
#         "qos": "default",
#         "rate_hz": 10.0
#     }}
# ]
#
# # Example subscriber configuration  
# node.subscribers = [
#     {{
#         "topic": "/sensor_node/data",
#         "msg_type": "sensor_msgs/Image",
#         "qos": "sensor_data",
#         "callback": "on_sensor_data"
#     }}
# ]
"#,
        node_name, class_name, node_name, class_name, class_name, class_name, node_name
    )
}

fn create_peppy_config(node_path: &Path, node_name: &str) -> Result<(), NodeCreationError> {
    let peppy_star_path = node_path.join("peppy.star");
    let mut file = fs::File::create(peppy_star_path)?;

    let peppy_content = get_node_create_template(node_name);

    file.write_all(peppy_content.as_bytes())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_peppy_config() {
        let expected_content = r#"
        # Peppy configuration file
        
    "#;
    }

    #[test]
    fn test_create_pixi_toml() {
        todo!()
    }
}
