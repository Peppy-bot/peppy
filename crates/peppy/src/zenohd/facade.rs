use crate::{Error, Result};
use askama::Template;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::{fmt, fs};
use zenoh::Session;
use zenoh::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Protocol {
    Tcp,
    Udp,
    Quic,
    Ws,
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Protocol::Tcp => write!(f, "tcp"),
            Protocol::Udp => write!(f, "udp"),
            Protocol::Quic => write!(f, "quic"),
            Protocol::Ws => write!(f, "ws"),
        }
    }
}

impl Default for Protocol {
    fn default() -> Self {
        Protocol::Tcp
    }
}

#[derive(Template)]
#[template(path = "zenoh/default_config.json5.j2")]
pub struct ZenohConfigTemplate {
    pub host: String,
    pub port: u16,
    pub protocol: Protocol,
}

impl Default for ZenohConfigTemplate {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: 7447,
            protocol: Protocol::default(),
        }
    }
}

/// The Zenoh daemon binary facade. Zenohd is not accessible via the Rust API (or in a very limited fashion).
/// This facade allows calling the binary in the background.
pub struct ZenohdFacade {
    zenohd_path: String,
    config: zenoh::config::Config,
    pub router_process: Option<Child>,
    session: Option<Session>,
}

impl ZenohdFacade {
    /// Creates a new ZenohdFacade instance with a working directory
    pub fn new(config_file: Option<PathBuf>) -> Result<Self> {
        let zenohd_path = ZenohdFacade::get_zenohd_binary()?;
        let config = ZenohdFacade::get_config(config_file)?;
        Ok(Self {
            zenohd_path,
            config,
            router_process: None,
            session: None,
        })
    }

    fn get_zenohd_binary() -> Result<String> {
        if let Some(path) = option_env!("ZENOHD_BINARY_PATH") {
            Ok(path.to_string())
        } else {
            Err(Error::ZenohdError(
                "Zenohd binary not found. Build with: cargo build --features build_zenoh"
                    .to_string(),
            ))
        }
    }

    fn get_config(config_file: Option<PathBuf>) -> Result<zenoh::config::Config> {
        match config_file {
            Some(config_path) => Config::from_file(config_path).map_err(|e| {
                Error::ConfigurationError(format!("Failed to load config file: {}", e))
            }),
            None => Config::from_json5(&Self::render_default_config()).map_err(|e| {
                Error::ConfigurationError(format!("Failed to parse default config: {}", e))
            }),
        }
    }

    fn get_config_as_path(&self) -> Result<PathBuf> {
        // Write config to a temporary file
        let temp_dir = std::env::temp_dir();
        // TODO: move this file into the .pixi /etc environment
        let config_path = temp_dir.join("zenohd_config.json5");

        // Remove existing file if it exists
        if config_path.exists() {
            fs::remove_file(&config_path).map_err(|e| {
                Error::BackendError(format!("Failed to remove existing config file: {}", e))
            })?;
        }

        // Convert the config to JSON5 string
        let config_str = json5::to_string(&self.config)
            .map_err(|e| Error::BackendError(format!("Failed to serialize config: {}", e)))?;

        // Write config to file (File::create already truncates if file exists, but we're being explicit)
        let mut file = fs::File::create(&config_path)
            .map_err(|e| Error::BackendError(format!("Failed to create config file: {}", e)))?;
        file.write_all(config_str.as_bytes())
            .map_err(|e| Error::BackendError(format!("Failed to write config file: {}", e)))?;
        Ok(config_path)
    }

    /// Renders the Zenoh configuration from the template
    fn render_default_config() -> String {
        let template = ZenohConfigTemplate::default();

        template
            .render()
            .unwrap_or_else(|e| panic!("Failed to render Zenoh config: {}", e))
    }

    pub fn start_router(&mut self) -> Result<()> {
        let config_path = self.get_config_as_path()?;

        // Start zenohd as a separate process with the config file
        let mut child = Command::new(&self.zenohd_path)
            .arg("-c")
            .arg(&config_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::BackendError(format!("Failed to start zenohd: {}", e)))?;

        // Give zenohd a moment to start up
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Check if the process is still running
        match child.try_wait() {
            Ok(Some(status)) => {
                // Clean up config file on failure
                let _ = fs::remove_file(&config_path);
                return Err(Error::BackendError(format!(
                    "zenohd exited unexpectedly with status: {}",
                    status
                )));
            }
            Ok(None) => {
                // Process is still running, which is what we want
                tracing::info!(
                    "Zenoh router started, with config {}",
                    config_path.to_str().unwrap()
                );
            }
            Err(e) => {
                // Clean up config file on failure
                let _ = fs::remove_file(&config_path);
                return Err(Error::BackendError(format!(
                    "Failed to check zenohd status: {}",
                    e
                )));
            }
        }

        // Store the child process handle
        self.router_process = Some(child);

        Ok(())
    }

    pub fn stop_router(&mut self) -> Result<()> {
        // Close the Zenoh session if it exists
        if let Some(session) = self.session.take() {
            drop(session);
        }

        // Terminate the zenohd router process if it's running
        if self.router_process.is_some() {
            if let Some(mut child) = self.router_process.take() {
                // Try to kill the process gracefully
                match child.kill() {
                    Ok(()) => {
                        tracing::info!("Zenohd router process terminated");
                    }
                    Err(e) => {
                        tracing::warn!("Failed to terminate zenohd router process: {}", e);
                    }
                }

                // Wait for the process to actually exit
                let _ = child.wait();
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_zenohd_facade_creation_default_config() {
        let facade = ZenohdFacade::new(None);
        assert!(facade.is_ok());
    }

    #[test]
    fn test_zenohd_facade_creation_with_config() {
        // Create a temporary directory that will be cleaned up automatically
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("test_zenoh_config.json5");

        // Write config directly in the test, inspired by the template
        let config_content = r#"{
            "listen": {
                "endpoints": {
                    "router": ["tcp/127.0.0.1:7447"]
                }
            }
        }"#;

        fs::write(&config_path, config_content).expect("Failed to write test config");

        // Create facade with the config file
        let facade = ZenohdFacade::new(Some(config_path));
        assert!(facade.is_ok());
    }

    #[test]
    fn test_zenohd_router_lifecycle() {
        // Create a config with a random port to avoid conflicts
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
        let config_path = temp_dir.path().join("test_zenoh_config.json5");

        // Use a random port to avoid conflicts
        let port = 8000 + (std::process::id() % 1000);
        let config_content = format!(
            r#"{{
            "listen": {{
                "endpoints": {{
                    "router": ["tcp/127.0.0.1:{}"]
                }}
            }}
        }}"#,
            port
        );

        fs::write(&config_path, config_content).expect("Failed to write test config");

        let mut facade = ZenohdFacade::new(Some(config_path)).expect("Failed to create facade");

        // Start the router
        let start_result = facade.start_router();
        assert!(
            start_result.is_ok(),
            "Failed to start router: {:?}",
            start_result.err()
        );

        // Verify the process was launched
        assert!(
            facade.router_process.is_some(),
            "Router process should be Some after starting"
        );

        // Check if the process is still running using try_wait
        let is_running = facade
            .router_process
            .as_mut()
            .unwrap()
            .try_wait()
            .expect("Failed to check process status")
            .is_none();

        assert!(is_running, "Router process should be running after start");

        // Get the process ID for verification
        let pid = facade.router_process.as_ref().unwrap().id();
        assert!(pid > 0, "Process ID should be valid");

        // Stop the router
        let stop_result = facade.stop_router();
        assert!(
            stop_result.is_ok(),
            "Failed to stop router: {:?}",
            stop_result.err()
        );

        // Verify the process handle was cleared
        assert!(
            facade.router_process.is_none(),
            "Router process should be None after stopping"
        );
    }
}
