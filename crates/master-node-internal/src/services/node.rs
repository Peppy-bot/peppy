use std::sync::Arc;

use crate::Result;
use crate::encoding::{
    NodeAddRequest, NodeAddResponse, NodeInitRequest, NodeInitResponse, NodeListRequest,
    NodeListResponse, NodeSyncRequest, NodeSyncResponse,
};
use bytes::Bytes;
use config::node::{Name, NodeConfigCreator, NodeConfigParser};
use node_stack::NodeStack;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::debug;

// ============================================================================
// Node List Service
// ============================================================================

pub async fn listen_for_node_list(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
) -> Result<JoinHandle<Result<()>>> {
    let service_name = "node_list";
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
            .handle_requests(|context| handle_node_list_request(context, node_stack.clone()))
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_node_list_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    handle_node_list_request_inner(&context, node_stack).map_err(|e| {
        PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.to_string(),
            reason: e.to_string(),
        }
    })
}

fn handle_node_list_request_inner(
    context: &ServiceRequestContext,
    node_stack: Arc<NodeStack>,
) -> Result<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let _request = NodeListRequest::decode(&payload.as_bytes())?;

    debug!("Received `node_list` request from {sender_instance_id}");

    let dot_graph = node_stack.to_dot();
    NodeListResponse::new(dot_graph).encode()
}

// ============================================================================
// Node Add Service
// ============================================================================

pub async fn listen_for_node_add(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
) -> Result<JoinHandle<Result<()>>> {
    let service_name = "node_add";
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
            .handle_requests(|context| handle_node_add_request(context, node_stack.clone()))
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_node_add_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    handle_node_add_request_inner(&context, node_stack).map_err(|e| {
        PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.to_string(),
            reason: e.to_string(),
        }
    })
}

fn handle_node_add_request_inner(
    context: &ServiceRequestContext,
    node_stack: Arc<NodeStack>,
) -> Result<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = NodeAddRequest::decode(&payload.as_bytes())?;

    debug!(
        "Received `node_add` request from {sender_instance_id}, from_dir={}",
        request.from_dir.display()
    );

    // Parse the node configuration from JSON5
    let node_config = match NodeConfigParser::from_content(&request.peppy_json5) {
        Ok(config) => config,
        Err(e) => {
            return NodeAddResponse::failure(format!("Failed to parse node config: {}", e))
                .encode();
        }
    };

    // Parse the optional instance_id
    let instance_id = match request.instance_id {
        Some(ref id) => match Name::new(id) {
            Ok(name) => Some(name),
            Err(e) => {
                return NodeAddResponse::failure(format!("Invalid instance_id: {}", e)).encode();
            }
        },
        None => None,
    };

    // Add the node to the stack (all dependencies must be satisfied)
    match node_stack.push_config(&node_config, instance_id.as_ref(), false) {
        Ok(instance_id) => {
            debug!(
                "Added node {}:{} with instance_id {}",
                node_config.manifest.name.as_str(),
                node_config.manifest.tag,
                instance_id.as_str()
            );
            NodeAddResponse::success(instance_id.as_str()).encode()
        }
        Err(e) => NodeAddResponse::failure(format!("Failed to add node: {}", e)).encode(),
    }
}

// ============================================================================
// Node Init Service
// ============================================================================
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

// ============================================================================
// Node Sync Service
// ============================================================================

pub async fn listen_for_node_sync(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
) -> Result<JoinHandle<Result<()>>> {
    let service_name = "node_sync";
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
            .handle_requests(|context| handle_node_sync_request(context))
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_node_sync_request(context: ServiceRequestContext) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    handle_node_sync_request_inner(&context).map_err(|e| PeppyError::InvalidServiceRequest {
        identifier: sender_instance_id.to_string(),
        reason: e.to_string(),
    })
}

fn handle_node_sync_request_inner(context: &ServiceRequestContext) -> Result<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = NodeSyncRequest::decode(&payload.as_bytes())?;

    debug!("Received `node_sync` request from {sender_instance_id}");

    if request.node_root_dir.as_os_str().is_empty() {
        return NodeSyncResponse::failure("Missing `node_root_dir` in node_sync request").encode();
    }

    if !request.node_root_dir.exists() {
        return NodeSyncResponse::failure(format!(
            "`node_root_dir` does not exist: {}",
            request.node_root_dir.display()
        ))
        .encode();
    }

    if !request.node_root_dir.is_dir() {
        return NodeSyncResponse::failure(format!(
            "`node_root_dir` is not a directory: {}",
            request.node_root_dir.display()
        ))
        .encode();
    }

    if let Err(e) =
        generator::generate_lib_for_build_system(request.build_system, &request.node_root_dir)
    {
        return NodeSyncResponse::failure(format!("Failed to generate peppygen: {}", e)).encode();
    }

    NodeSyncResponse::success().encode()
}
