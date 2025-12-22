use crate::Result;
use crate::encoding::{NodeInitRequest, NodeInitResponse};
use bytes::Bytes;
use config::node::NodeConfigCreator;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::debug;

pub async fn listen_for_node_init(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
) -> Result<JoinHandle<Result<()>>> {
    let service_name = "node_init";

    let mut endpoint = ServiceMessenger::listen(
        messenger,
        master_node_node,
        instance_id,
        node_name,
        &service_name,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(|context| handle_node_init_request(context))
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_node_init_request(context: ServiceRequestContext) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    handle_node_init_request_inner(&context).map_err(|e| PeppyError::InvalidServiceRequest {
        identifier: sender_instance_id.to_string(),
        reason: e.to_string(),
    })
}

fn handle_node_init_request_inner(context: &ServiceRequestContext) -> Result<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = NodeInitRequest::decode(&payload.as_bytes())?;

    debug!(
        "Received `node_init` request from {sender_instance_id}, node_root_dir={}, node_name={}",
        request.node_root_dir.display(),
        request.node_name
    );

    // Create the node directory at node_root_dir/node_name
    let node_dir = request.node_root_dir.join(&request.node_name);

    if node_dir.exists() {
        return NodeInitResponse::failure(format!(
            "Node directory already exists: {}",
            node_dir.display()
        ))
        .encode();
    }

    if let Err(e) = std::fs::create_dir_all(&node_dir) {
        return NodeInitResponse::failure(format!("Failed to create node directory: {}", e))
            .encode();
    }

    // Create peppy.json5
    if let Err(e) = NodeConfigCreator::simple_node(&request.node_name)
        .and_then(|creator| creator.write_to(node_dir.join(config::consts::NODE_CONFIG_FILE)))
    {
        return NodeInitResponse::failure(format!("Failed to create peppy.json5: {}", e)).encode();
    }

    // Generate peppygen library
    if let Err(e) = generator::generate_lib_for_build_system(request.build_system, &node_dir) {
        return NodeInitResponse::failure(format!("Failed to generate peppygen: {}", e)).encode();
    }

    // Create language-specific configuration
    match request.build_system {
        config::peppy_config::BuildSystem::Rust | config::peppy_config::BuildSystem::Cargo => {
            if let Err(e) = create_rust_node_config(&request.node_name, &node_dir) {
                return NodeInitResponse::failure(format!(
                    "Failed to create Rust configuration: {}",
                    e
                ))
                .encode();
            }
        }
        config::peppy_config::BuildSystem::Python | config::peppy_config::BuildSystem::Uv => {
            if let Err(e) = create_python_node_config(&request.node_name, &node_dir) {
                return NodeInitResponse::failure(format!(
                    "Failed to create Python configuration: {}",
                    e
                ))
                .encode();
            }
        }
    }

    // Create .gitignore
    if let Err(e) = create_gitignore(&node_dir, request.build_system) {
        return NodeInitResponse::failure(format!("Failed to create .gitignore: {}", e)).encode();
    }

    NodeInitResponse::success().encode()
}

fn create_rust_node_config(node_name: &str, node_dir: &std::path::Path) -> std::io::Result<()> {
    // Create src directory
    let src_dir = node_dir.join("src");
    std::fs::create_dir_all(&src_dir)?;

    // Create main.rs
    let main_rs_content = r#"fn main() {
    println!("Hello from {}!");
}
"#
    .replace("{}", node_name);
    std::fs::write(src_dir.join("main.rs"), main_rs_content)?;

    // Create Cargo.toml with peppygen dependency
    let cargo_toml_content = format!(
        r#"[package]
name = "{}"
version = "0.1.0"
edition = "2024"

[dependencies]
peppygen = {{ path = "{}" }}
"#,
        node_name,
        config::consts::PEPPYGEN_OUTPUT_PATH
    );
    std::fs::write(node_dir.join("Cargo.toml"), cargo_toml_content)?;

    Ok(())
}

fn create_python_node_config(node_name: &str, node_dir: &std::path::Path) -> std::io::Result<()> {
    // Create pyproject.toml
    let pyproject_content = format!(
        r#"[project]
name = "{}"
version = "0.1.0"
description = "{} peppy node"
requires-python = ">=3.10"

[tool.uv]
dev-dependencies = []
"#,
        node_name, node_name
    );
    std::fs::write(node_dir.join("pyproject.toml"), pyproject_content)?;

    // Create main.py
    let main_py_content = format!(
        r#"#!/usr/bin/env python3
"""Main entry point for {} node."""


def main():
    print("Hello from {}!")


if __name__ == "__main__":
    main()
"#,
        node_name, node_name
    );
    std::fs::write(node_dir.join("main.py"), main_py_content)?;

    Ok(())
}

fn create_gitignore(
    node_dir: &std::path::Path,
    build_system: config::peppy_config::BuildSystem,
) -> std::io::Result<()> {
    let content = match build_system {
        config::peppy_config::BuildSystem::Rust | config::peppy_config::BuildSystem::Cargo => {
            r#"
# Peppy-specific files
.peppy

# Rust
/target/
*.swp
*.bak
*.orig

# Editor-specific files
.vscode/
.idea/

# Operating System files
**/.DS_Store
**/.Thumbs.db

# Logs
*.log
"#
        }
        config::peppy_config::BuildSystem::Python | config::peppy_config::BuildSystem::Uv => {
            r#"
# Peppy-specific files
.peppy

# Byte-compiled / optimized / DLL files
__pycache__/
*.pyc
*.pyd
*.pyo

# Virtual environment
.env/
.venv/
.pixi/


# Editor-specific files
.vscode/
.idea/

# Operating System files
**/.DS_Store
**/.Thumbs.db

# Jupyter Notebook checkpoints
.ipynb_checkpoints/

# Logs
*.log
"#
        }
    };
    std::fs::write(node_dir.join(".gitignore"), content)?;
    Ok(())
}
