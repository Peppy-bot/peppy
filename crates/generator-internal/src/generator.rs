mod checker;
mod common;
mod python;
mod rust;
pub mod types;

use crate::error::{Error, Result};
use config::{consts::PEPPY_NODE_CONFIG_FILE, node::NodeConfigParser, peppy_config::BuildSystem};
use python::PythonGenerator;
use rust::RustGenerator;
use std::{fs, path::Path};
use types::{DeploymentInterface, InterfaceVariant, LanguageGenerator};

/// The standard output directory for generated peppygen libraries relative to node_dir.
const PEPPYGEN_OUTPUT_PATH: &str = ".peppy/libs/peppygen";

/// Generate an interface library for the given build system from a node directory.
///
/// This function reads the `peppy.json5` configuration file from the `node_dir`,
/// extracts the exposed interfaces, and generates a library for the specified build system.
/// The library is generated at `node_dir/.peppy/libs/peppygen`.
///
/// # Arguments
/// * `build_system` - The build system to generate for (Rust/Cargo or Python/Uv)
/// * `node_dir` - Path to the node directory containing `peppy.json5`
///
/// # Errors
/// Returns an error if:
/// - The `peppy.json5` file is not found in `node_dir`
/// - The configuration file cannot be parsed
/// - Code generation fails
pub fn generate_lib_for_build_system(
    build_system: BuildSystem,
    node_dir: impl AsRef<Path>,
) -> Result<()> {
    let node_dir = node_dir.as_ref();
    let node_config_path = node_dir.join(PEPPY_NODE_CONFIG_FILE);

    if !node_config_path.exists() {
        return Err(Error::NodeNotFound(node_dir.display().to_string()));
    }

    let node_config = NodeConfigParser::from_path(&node_config_path)
        .map_err(|e| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;

    let interfaces = collect_exposed_interfaces(&node_config);

    // Create the output directory
    let output_dir = node_dir.join(PEPPYGEN_OUTPUT_PATH);
    fs::create_dir_all(&output_dir)?;

    // Temporarily copy peppy.json5 to the output directory (required for fingerprinting during build)
    let output_config_path = output_dir.join(PEPPY_NODE_CONFIG_FILE);
    fs::copy(&node_config_path, &output_config_path)?;

    let result = match build_system {
        BuildSystem::Rust | BuildSystem::Cargo => {
            generate_with_backend(RustGenerator::new(), &interfaces, &output_dir)
        }
        BuildSystem::Python | BuildSystem::Uv => {
            generate_with_backend(PythonGenerator::new(), &interfaces, &output_dir)
        }
    };

    // Clean up the temporary config copy
    let _ = fs::remove_file(&output_config_path);

    result
}

/// Collects all exposed interfaces from a NodeConfig into DeploymentInterface instances.
fn collect_exposed_interfaces(config: &config::node::NodeConfig) -> Vec<DeploymentInterface> {
    let mut interfaces = Vec::new();

    if let Some(exposes) = &config.interfaces.exposes {
        if let Some(topics) = &exposes.topics {
            for topic in topics {
                interfaces.push(DeploymentInterface::new(InterfaceVariant::ExposedTopic(
                    topic.clone(),
                )));
            }
        }

        if let Some(services) = &exposes.services {
            for service in services {
                interfaces.push(DeploymentInterface::new(InterfaceVariant::ExposedService(
                    service.clone(),
                )));
            }
        }

        if let Some(actions) = &exposes.actions {
            for action in actions {
                interfaces.push(DeploymentInterface::new(InterfaceVariant::ExposedAction(
                    action.clone(),
                )));
            }
        }
    }

    interfaces
}

fn generate_with_backend<B>(
    mut backend: B,
    interfaces: &[DeploymentInterface],
    output_dir: &Path,
) -> Result<()>
where
    B: LanguageGenerator,
{
    for interface in interfaces {
        interface.register_with(&mut backend)?;
    }
    backend.build(output_dir)
}
