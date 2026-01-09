use crate::Result;
use crate::encoding::{NodeAddRequest, NodeAddResponse};
use crate::names;
use bytes::Bytes;
use config::consts::{AppEnv, NODE_CONFIG_FILE, app_env};
use config::node::NodeConfigParser;
use node_stack::NodeStack;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use rand::Rng;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::debug;

/// Returns the base directory for storing copied node folders.
/// In production: ~/.peppy/<master_node_instance_id>/nodes
/// In development: /tmp/.peppy/<master_node_instance_id>/nodes
fn nodes_storage_dir(master_node_instance_id: &str) -> PathBuf {
    match app_env() {
        AppEnv::Prod => {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(PathBuf::from);
            home.unwrap_or_else(std::env::temp_dir)
                .join(".peppy")
                .join(master_node_instance_id)
                .join("nodes")
        }
        AppEnv::Dev => std::env::temp_dir()
            .join(".peppy")
            .join(master_node_instance_id)
            .join("nodes"),
    }
}

fn generate_random_id() -> String {
    let mut rng = rand::rng();
    let bytes: [u8; 3] = rng.random();
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;

    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dest_path)?;
        } else {
            std::fs::copy(&src_path, &dest_path)?;
        }
    }
    Ok(())
}

/// Runs the add_cmd for a node if specified.
/// This is executed right before the node is added to the node stack.
/// Returns Ok(()) if add_cmd is None or executes successfully.
fn run_add_cmd(
    add_cmd: Option<&Vec<String>>,
    working_dir: &Path,
) -> std::result::Result<(), String> {
    let Some(cmd) = add_cmd else {
        return Ok(());
    };

    let Some((program, args)) = cmd.split_first() else {
        return Err("add_cmd is empty".to_string());
    };

    debug!(
        "Running add_cmd: {} {:?} in dir {:?}",
        program, args, working_dir
    );

    let output = Command::new(program)
        .args(args)
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("failed to execute add_cmd: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "add_cmd failed with status {}: stderr: {}, stdout: {}",
            output.status,
            stderr.trim(),
            stdout.trim()
        ));
    }

    debug!("add_cmd completed successfully");
    Ok(())
}

/// Copies a node folder to the peppy nodes storage directory.
///
/// The destination path follows the format: `<storage_dir>/<node_name>_<tag>_<uuid>`
///
/// Returns the path to the copied folder.
fn copy_node_to_storage(
    from_dir: &Path,
    node_name: &str,
    node_tag: &str,
    master_node_instance_id: &str,
) -> Result<PathBuf> {
    let storage_dir = nodes_storage_dir(master_node_instance_id);
    let random_id = generate_random_id();
    let folder_name = format!("{}_{}_{}", node_name, node_tag, random_id);
    let dest_path = storage_dir.join(&folder_name);

    debug!(
        "Copying node folder from {} to {}",
        from_dir.display(),
        dest_path.display()
    );

    copy_dir_recursive(from_dir, &dest_path)?;

    Ok(dest_path)
}

pub async fn listen_for_node_add(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        master_node_node,
        instance_id,
        node_name,
        names::NODE_ADD,
    )
    .await?;

    let instance_id = instance_id.to_owned();
    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(|context| {
                handle_node_add_request(context, node_stack.clone(), instance_id.clone())
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_node_add_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
    master_node_instance_id: String,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    handle_node_add_request_inner(&context, node_stack, &master_node_instance_id)
        .await
        .map_err(|e| PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.to_string(),
            reason: e.to_string(),
        })
}

async fn handle_node_add_request_inner(
    context: &ServiceRequestContext,
    node_stack: Arc<NodeStack>,
    master_node_instance_id: &str,
) -> Result<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = NodeAddRequest::decode(&payload.as_bytes())?;

    debug!(
        "Received `node_add` request from {sender_instance_id}, from_dir={}",
        request.from_dir.display()
    );

    // Parse the node configuration from the request directory.
    let config_path = request.from_dir.join(NODE_CONFIG_FILE);
    let node_config = match NodeConfigParser::from_path(&config_path) {
        Ok(config) => config,
        Err(e) => {
            return NodeAddResponse::failure(format!(
                "Failed to parse node config at {}: {}",
                config_path.display(),
                e
            ))
            .encode();
        }
    };

    let node_name = node_config.manifest.name.as_str().to_owned();
    let node_tag = node_config.manifest.tag.clone();

    // Copy the node folder to the peppy storage directory.
    // Each added node gets its own isolated copy.
    let copied_path = match copy_node_to_storage(
        &request.from_dir,
        &node_name,
        &node_tag,
        master_node_instance_id,
    ) {
        Ok(path) => path,
        Err(e) => {
            return NodeAddResponse::failure(format!("Failed to copy node folder: {}", e)).encode();
        }
    };

    // Run add_cmd on the copied folder if specified (e.g., cargo build).
    // This is always done AFTER the copy because the node could be fetched remotely (git/http)
    if let Err(e) = run_add_cmd(node_config.manifest.add_cmd.as_ref(), &copied_path) {
        // Clean up the copied folder on failure
        let _ = std::fs::remove_dir_all(&copied_path);
        return NodeAddResponse::failure(format!("add_cmd failed: {}", e)).encode();
    }

    // Add the node config to the stack with its copied path (all dependencies must be satisfied)
    // Note: `add` only registers the node configuration, it does not spawn any instance.
    // Use `node_start` to spawn instances after adding a node.
    if let Err(e) = node_stack.push_config(node_config, false, &copied_path) {
        // Clean up the copied folder on failure
        let _ = std::fs::remove_dir_all(&copied_path);
        return NodeAddResponse::failure(format!("Failed to add node config: {}", e)).encode();
    }

    debug!(
        "Added node {}:{} at {}",
        node_name,
        node_tag,
        copied_path.display()
    );

    NodeAddResponse::success(copied_path).encode()
}
