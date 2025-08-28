use crate::{Error, Result};
use askama::Template;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::{fmt, fs};
use zenoh::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
#[derive(Default)]
pub enum Protocol {
    #[default]
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

#[derive(Template)]
#[template(path = "zenoh/default_router_config.json5.j2")]
pub struct ZenohRouterConfigTemplate {
    pub host: String,
    pub port: u16,
    pub protocol: Protocol,
}

impl Default for ZenohRouterConfigTemplate {
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
    pub config: zenoh::config::Config,
    pub router_endpoint: String,
    pub config_template: ZenohRouterConfigTemplate,
    pub router_process: Option<Child>,
}

impl ZenohdFacade {
    /// Creates a new ZenohdFacade instance with a working directory
    pub fn new(config_file: Option<PathBuf>) -> Result<Self> {
        let zenohd_path = ZenohdFacade::get_zenohd_binary()?;
        let (config, router_endpoint, config_template) =
            ZenohdFacade::get_config_and_endpoint(config_file)?;
        Ok(Self {
            zenohd_path,
            config,
            router_endpoint,
            config_template,
            router_process: None,
        })
    }

    /// Parses an endpoint string like "tcp/127.0.0.1:7447" into protocol, host, and port
    fn parse_endpoint(endpoint: &str) -> Result<(Protocol, String, u16)> {
        // Handle both formats: "tcp/127.0.0.1:7447" and "tcp://127.0.0.1:7447"
        let parts: Vec<&str> = if endpoint.contains("://") {
            endpoint.splitn(2, "://").collect()
        } else {
            endpoint.splitn(2, '/').collect()
        };

        if parts.len() != 2 {
            return Err(Error::ConfigurationError(format!(
                "Invalid endpoint format: {}",
                endpoint
            )));
        }

        let protocol = match parts[0] {
            "tcp" => Protocol::Tcp,
            "udp" => Protocol::Udp,
            "quic" => Protocol::Quic,
            "ws" => Protocol::Ws,
            _ => {
                return Err(Error::ConfigurationError(format!(
                    "Unknown protocol: {}",
                    parts[0]
                )));
            }
        };

        let host_port_parts: Vec<&str> = parts[1].rsplitn(2, ':').collect();
        if host_port_parts.len() != 2 {
            return Err(Error::ConfigurationError(format!(
                "Invalid host:port format: {}",
                parts[1]
            )));
        }

        let port = host_port_parts[0].parse::<u16>().map_err(|_| {
            Error::ConfigurationError(format!("Invalid port: {}", host_port_parts[0]))
        })?;
        let host = host_port_parts[1].to_string();

        Ok((protocol, host, port))
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

    fn get_config_and_endpoint(
        config_file: Option<PathBuf>,
    ) -> Result<(zenoh::config::Config, String, ZenohRouterConfigTemplate)> {
        match config_file {
            Some(config_path) => {
                let config = Config::from_file(&config_path).map_err(|e| {
                    Error::ConfigurationError(format!("Failed to load config file: {}", e))
                })?;

                // Read the config file to extract endpoint
                let config_str = fs::read_to_string(&config_path).map_err(|e| {
                    Error::ConfigurationError(format!("Failed to read config file: {}", e))
                })?;

                // Parse as JSON5 to extract endpoint
                let config_json: serde_json::Value = json5::from_str(&config_str).map_err(|e| {
                    Error::ConfigurationError(format!("Failed to parse config JSON: {}", e))
                })?;

                let endpoint = config_json
                    .get("listen")
                    .and_then(|v| v.get("endpoints"))
                    .and_then(|v| v.get("router"))
                    .and_then(|v| v.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|v| v.as_str())
                    .unwrap_or("tcp/127.0.0.1:7447")
                    .to_string();

                // Parse the endpoint to extract protocol, host, and port for the template
                let (protocol, host, port) = Self::parse_endpoint(&endpoint)?;
                let template = ZenohRouterConfigTemplate {
                    protocol,
                    host,
                    port,
                };
                Ok((config, endpoint, template))
            }
            None => {
                let template = ZenohRouterConfigTemplate::default();
                // For connecting, we need to use 127.0.0.1 even if the server listens on 0.0.0.0
                let connect_host = if template.host == "0.0.0.0" {
                    "127.0.0.1"
                } else {
                    &template.host
                };
                let endpoint = format!("{}/{}:{}", template.protocol, connect_host, template.port);
                let config = Config::from_json5(&Self::render_default_config()).map_err(|e| {
                    Error::ConfigurationError(format!("Failed to parse default config: {}", e))
                })?;
                Ok((config, endpoint, template))
            }
        }
    }

    fn get_config_as_path(&self) -> Result<PathBuf> {
        // Write config to a temporary file
        let temp_dir = std::env::temp_dir();
        // TODO: move this file into the .pixi /etc environment
        let config_path = temp_dir.join("zenohd_config.json5");

        // Remove existing file if it exists (ignore errors if it doesn't exist)
        let _ = fs::remove_file(&config_path);

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
        let template = ZenohRouterConfigTemplate::default();

        template
            .render()
            .unwrap_or_else(|e| panic!("Failed to render Zenoh config: {}", e))
    }

    pub fn start_router(&mut self) -> Result<()> {
        let config_path = self.get_config_as_path()?;

        tracing::info!("Starting zenohd from path: {}", self.zenohd_path);
        tracing::info!("Using config file: {:?}", config_path);

        // Start zenohd as a separate process with the config file
        let mut child = Command::new(&self.zenohd_path)
            .arg("-c")
            .arg(&config_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::BackendError(format!("Failed to start zenohd: {}", e)))?;

        // Give zenohd a moment to start up
        std::thread::sleep(std::time::Duration::from_millis(1000));

        // Check if the process is still running
        match child.try_wait() {
            Ok(Some(status)) => {
                // Try to read stderr for error messages
                let mut stderr = String::new();
                if let Some(mut stderr_handle) = child.stderr.take() {
                    use std::io::Read;
                    let _ = stderr_handle.read_to_string(&mut stderr);
                }

                // Clean up config file on failure
                let _ = fs::remove_file(&config_path);
                return Err(Error::BackendError(format!(
                    "zenohd exited unexpectedly with status: {}. stderr: {}",
                    status, stderr
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
        // Terminate the zenohd router process if it's running
        if let Some(mut child) = self.router_process.take() {
            // Try to kill the process gracefully
            if let Err(e) = child.kill() {
                tracing::warn!("Failed to terminate zenohd router process: {}", e);
            } else {
                tracing::info!("Zenohd router process terminated");
            }

            // Wait for the process to actually exit and log any error
            if let Err(e) = child.wait() {
                tracing::warn!("Error waiting for zenohd process to exit: {}", e);
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
            "mode": "router",
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
            "mode": "router",
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
        assert_eq!(facade.router_endpoint, format!("tcp/127.0.0.1:{}", port));

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

    #[test]
    fn test_stop_router_multiple_times() {
        let mut facade = ZenohdFacade::new(None).expect("Failed to create facade");

        // First stop should succeed (no process to stop)
        assert!(facade.stop_router().is_ok());

        // Second stop should also succeed (idempotent)
        assert!(facade.stop_router().is_ok());

        // Process should remain None
        assert!(facade.router_process.is_none());
    }
}
