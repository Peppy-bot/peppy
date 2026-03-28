use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::super::node::NodeConfigParser;
use super::discovery::find_peppy_nodes_from_dir;
use crate::error::{Error, ParsingError, Result};
use tracing::{info, warn};

use crate::{consts::NODE_CONFIG_FILE, node::NodeConfig};

/// Aggregated state keyed by config file path. Each entry reflects the
/// current parse result of the corresponding `peppy.json5` file.
pub type NodeIndexState = HashMap<PathBuf, core::result::Result<NodeConfig, ParsingError>>;

/// Builds an aggregated node configuration index for a directory tree.
/// The index maps each `peppy.json5` file path to the parse result
/// (`Ok(NodeConfig)` or `Err(ParsingError)`).
pub struct FSNodeConfigIndex {
    state: NodeIndexState,
}

impl FSNodeConfigIndex {
    /// Build the node index by scanning `from_dir` recursively.
    pub fn new(from_dir: impl AsRef<Path>) -> Result<Self> {
        let state = Self::load_initial_state(from_dir.as_ref());
        Ok(Self { state })
    }

    /// Returns the aggregated node index.
    pub fn state(&self) -> &NodeIndexState {
        &self.state
    }

    /// Consumes the index and returns the aggregated node index.
    pub fn into_state(self) -> NodeIndexState {
        self.state
    }

    fn load_initial_state(from_dir: &Path) -> NodeIndexState {
        let config_files = find_peppy_nodes_from_dir(from_dir);
        info!(
            "Found {} initial {} files in {:?}",
            config_files.len(),
            NODE_CONFIG_FILE,
            from_dir
        );
        let mut state: NodeIndexState = HashMap::with_capacity(config_files.len());
        for path in config_files {
            match NodeConfigParser::from_path(&path) {
                Ok(cfg) => {
                    state.insert(path, Ok(cfg));
                }
                Err(err) => {
                    warn!("Could not parse {}: {}", path.display(), err);
                    if let Error::Parsing(pe) = err {
                        state.insert(path, Err(pe));
                    }
                }
            }
        }
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::NODE_CONFIG_FILE;
    use std::fs;
    use tempfile::TempDir;

    fn write_config(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(NODE_CONFIG_FILE);
        let json5 = r#"{
                schema_version: 1,
                manifest: {
                    name: "{name}",
                    tag: "0.1.0",
                },
                execution: {
                    language: "rust",
                    start_cmd: ["./target/release/{name}"]
                }
            }"#
        .replace("{name}", name);
        fs::write(&path, json5).unwrap();
        path
    }

    #[test]
    fn test_initial_state_loads_all_configs() {
        let temp = TempDir::new().unwrap();

        // config 1
        let base = write_config(temp.path(), "base_node");

        // nested config
        let nested_dir = temp.path().join("nested");
        fs::create_dir(&nested_dir).unwrap();
        let nested = write_config(&nested_dir, "nested_node");

        let state = FSNodeConfigIndex::new(temp.path())
            .expect("index init")
            .into_state();

        assert_eq!(state.len(), 2);
        assert!(state.contains_key(&base));
        assert!(state.contains_key(&nested));
        assert_eq!(
            state[&base].as_ref().unwrap().manifest.name.as_str(),
            "base_node"
        );
        assert_eq!(
            state[&nested].as_ref().unwrap().manifest.name.as_str(),
            "nested_node"
        );
        assert!(state.values().all(|e| e.is_ok()));
    }

    #[test]
    fn test_new_reports_invalid_initial_config_via_state() {
        let temp = TempDir::new().unwrap();

        // Invalid name (spaces and '!') should fail parsing on initial load
        fs::write(
            temp.path().join(NODE_CONFIG_FILE),
            "{ schema_version: 1, manifest: { name: 'Invalid Name!', tag: '0.1.0' }, execution: { language: 'rust', start_cmd: ['./target/release/Invalid Name!'] } }",
        )
        .unwrap();

        let state = FSNodeConfigIndex::new(temp.path())
            .expect("index init")
            .into_state();
        assert_eq!(state.len(), 1);
        let entry = state.values().next().unwrap();
        assert!(entry.is_err());
    }
}
