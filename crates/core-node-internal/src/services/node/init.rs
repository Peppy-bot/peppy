mod templates;

use self::templates::{apply_python_templates, apply_rust_templates};
use crate::Result;
use crate::services::raw_calls::RawCallGate;
use crate::services::response::{failure, into_service_answer};
use config::failure_kind::PeppyFailureKind;
use config::node::Toolchain;
use core_node_api::ServiceId;
use core_node_api::encoding::{NodeInitRequest, NodeInitResponse};
use core_node_api::names;
use daemon_config::consts::PeppyDirs;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::{Failure, ServiceAnswer, ServiceRequestContext};
use peppylib::{MessengerHandle, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::debug;

pub async fn listen_for_node_init(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    peppy_dirs: PeppyDirs,
    raw_calls: RawCallGate,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        core_node_name,
        instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        ServiceId::NodeInit.name(),
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(raw_calls.guard(ServiceId::NodeInit.name(), move |context| {
                handle_node_init_request(context, peppy_dirs.clone())
            }))
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_node_init_request(
    context: ServiceRequestContext,
    peppy_dirs: PeppyDirs,
) -> ServiceAnswer {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload_bytes();

    let request = NodeInitRequest::decode(payload.as_ref())
        .map_err(|error| failure(PeppyFailureKind::Rejected, error))?;

    debug!(
        "Received `node_init` request from {sender_instance_id}, node_root_dir={}, node_name={}",
        request.node_root_dir.display(),
        request.node_name
    );

    scaffold_node(&request, &peppy_dirs)?;
    into_service_answer(NodeInitResponse::new().encode().map_err(Into::into))
}

/// Creates the node directory at `node_root_dir/node_name` and writes the
/// toolchain's configuration, the peppygen library and a `.gitignore` into
/// it. Stops at the first step that fails and returns the failure the
/// caller sees: `precondition_failed` when the directory already exists,
/// `attempt_failed` for a step that fails.
fn scaffold_node(
    request: &NodeInitRequest,
    peppy_dirs: &PeppyDirs,
) -> std::result::Result<(), Failure> {
    let node_dir = request.node_root_dir.join(&request.node_name);
    if node_dir.exists() {
        return Err(failure(
            PeppyFailureKind::PreconditionFailed,
            format!("Node directory already exists: {}", node_dir.display()),
        ));
    }
    let attempt_failed = |message: String| failure(PeppyFailureKind::AttemptFailed, message);
    std::fs::create_dir_all(&node_dir)
        .map_err(|e| attempt_failed(format!("Failed to create node directory: {e}")))?;

    // Create language-specific configuration (must be done before peppygen generation
    // since generate_peppygen_lib requires peppy.json5 to exist)
    match request.toolchain {
        Toolchain::Cargo => {
            apply_rust_templates(
                &request.node_name,
                &request.description,
                &node_dir,
                request.with_container,
            )
            .map_err(|e| attempt_failed(format!("Failed to create Rust configuration: {e}")))?;
        }
        Toolchain::Uv => {
            apply_python_templates(
                &request.node_name,
                &request.description,
                &node_dir,
                request.with_container,
            )
            .map_err(|e| attempt_failed(format!("Failed to create Python configuration: {e}")))?;
        }
    }

    generator::generate_peppygen_lib(
        request.toolchain.map_to_language(),
        &node_dir,
        Vec::new(),
        &request.git_hash,
        peppy_dirs,
        generator::CrateDeployMode::default(),
        None,
        generator::NodeTree::Source,
    )
    .map_err(|e| attempt_failed(format!("Failed to generate peppygen: {e}")))?;

    create_gitignore(&node_dir, request.toolchain)
        .map_err(|e| attempt_failed(format!("Failed to create .gitignore: {e}")))
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
