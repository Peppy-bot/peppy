use super::templates::{apply_python_templates, apply_rust_templates};
use crate::Result;
use crate::encoding::{NodeInitRequest, NodeInitResponse};
use crate::names;
use bytes::Bytes;
use config::peppy_config::Toolchain;
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
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        master_node_node,
        instance_id,
        node_name,
        names::NODE_INIT,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(handle_node_init_request)
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

    // Create language-specific configuration (must be done before peppygen generation
    // since generate_lib_for_build_system requires peppy.json5 to exist)
    let toolchain = Toolchain::Cargo; // FIXME: Use the toolchain in `peppy.json5`
    match toolchain {
        config::peppy_config::Toolchain::Rust | config::peppy_config::Toolchain::Cargo => {
            if let Err(e) = apply_rust_templates(&request.node_name, &node_dir) {
                return NodeInitResponse::failure(format!(
                    "Failed to create Rust configuration: {}",
                    e
                ))
                .encode();
            }
        }
        config::peppy_config::Toolchain::Python | config::peppy_config::Toolchain::Uv => {
            if let Err(e) = apply_python_templates(&request.node_name, &node_dir) {
                return NodeInitResponse::failure(format!(
                    "Failed to create Python configuration: {}",
                    e
                ))
                .encode();
            }
        }
    }

    // Generate peppygen library (requires peppy.json5 to exist)
    if let Err(e) = generator::generate_lib_for_build_system(
        toolchain,
        &node_dir,
        Vec::new(),
        &request.git_hash,
    ) {
        return NodeInitResponse::failure(format!("Failed to generate peppygen: {}", e)).encode();
    }

    // Create .gitignore
    if let Err(e) = create_gitignore(&node_dir, toolchain) {
        return NodeInitResponse::failure(format!("Failed to create .gitignore: {}", e)).encode();
    }

    NodeInitResponse::success().encode()
}

fn create_gitignore(
    node_dir: &std::path::Path,
    build_system: config::peppy_config::Toolchain,
) -> std::io::Result<()> {
    let content = match build_system {
        config::peppy_config::Toolchain::Rust | config::peppy_config::Toolchain::Cargo => {
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
        config::peppy_config::Toolchain::Python | config::peppy_config::Toolchain::Uv => {
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
