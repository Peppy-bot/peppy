use super::templates::{apply_python_templates, apply_rust_templates};
use crate::Result;
use crate::encoding::{NodeInitRequest, NodeInitResponse};
use crate::names;
use config::consts::PeppyDirs;
use config::node::Toolchain;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::debug;

pub async fn listen_for_node_init(
    messenger: &MessengerHandle,
    daemon_node_node: &str,
    instance_id: &str,
    node_name: &str,
    peppy_dirs: PeppyDirs,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        daemon_node_node,
        instance_id,
        node_name,
        names::NODE_INIT,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(move |context| handle_node_init_request(context, peppy_dirs.clone()))
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_node_init_request(
    context: ServiceRequestContext,
    peppy_dirs: PeppyDirs,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id();
    handle_node_init_request_inner(&context, &peppy_dirs).map_err(|e| {
        PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.to_string(),
            reason: e.to_string(),
        }
    })
}

fn handle_node_init_request_inner(
    context: &ServiceRequestContext,
    peppy_dirs: &PeppyDirs,
) -> Result<Payload> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = NodeInitRequest::decode(payload.as_ref())?;
    let toolchain = request.toolchain;

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
    // since generate_peppygen_lib requires peppy.json5 to exist)
    match toolchain {
        Toolchain::Cargo => {
            if let Err(e) =
                apply_rust_templates(&request.node_name, &node_dir, request.with_container)
            {
                return NodeInitResponse::failure(format!(
                    "Failed to create Rust configuration: {}",
                    e
                ))
                .encode();
            }
        }
        Toolchain::Uv => {
            if let Err(e) =
                apply_python_templates(&request.node_name, &node_dir, request.with_container)
            {
                return NodeInitResponse::failure(format!(
                    "Failed to create Python configuration: {}",
                    e
                ))
                .encode();
            }
        }
    }

    let language = toolchain.map_to_language();
    // Generate peppygen library (requires peppy.json5 to exist)
    if let Err(e) = generator::generate_peppygen_lib(
        language,
        &node_dir,
        Vec::new(),
        &request.git_hash,
        peppy_dirs,
    ) {
        return NodeInitResponse::failure(format!("Failed to generate peppygen: {}", e)).encode();
    }

    // Create .gitignore
    if let Err(e) = create_gitignore(&node_dir, toolchain) {
        return NodeInitResponse::failure(format!("Failed to create .gitignore: {}", e)).encode();
    }

    NodeInitResponse::success().encode()
}

fn create_gitignore(node_dir: &std::path::Path, build_system: Toolchain) -> std::io::Result<()> {
    let content = match build_system {
        Toolchain::Cargo => {
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
        Toolchain::Uv => {
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
