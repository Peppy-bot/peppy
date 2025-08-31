use super::super::error::{Error, Result};
use askama::Template;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::{fmt, fs};
use zenoh::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
#[derive(Default)]
pub enum ZenohNetProtocol {
    #[default]
    Tcp,
    Udp,
    Quic,
    Ws,
}

impl fmt::Display for ZenohNetProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ZenohNetProtocol::Tcp => write!(f, "tcp"),
            ZenohNetProtocol::Udp => write!(f, "udp"),
            ZenohNetProtocol::Quic => write!(f, "quic"),
            ZenohNetProtocol::Ws => write!(f, "ws"),
        }
    }
}

#[derive(Template)]
#[template(path = "zenoh/default_router_config.json5.j2")]
pub struct ZenohRouterConfigTemplate {
    pub host: String,
    pub port: u16,
    pub protocol: ZenohNetProtocol,
}

impl Default for ZenohRouterConfigTemplate {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: 7447,
            protocol: ZenohNetProtocol::default(),
        }
    }
}

/// This structure stores the Zenoh endpoint to be reused by clients by extracting it from the config file
pub struct ZenohEndpoint {
    pub host: String,
    pub port: u16,
    pub protocol: ZenohNetProtocol,
}

/// The Zenoh daemon binary facade. Zenohd is not accessible via the Rust API (or in a very limited fashion).
/// This facade allows calling the binary in the background.
pub struct ZenohdFacade {
    zenohd_path: String,
    pub config: zenoh::config::Config,
    pub router_process: Option<Child>,
    pub zenoh_endpoint: ZenohEndpoint,
}

impl ZenohdFacade {
    /// Creates a new ZenohdFacade instance with a working directory
    pub fn new(config_file: Option<PathBuf>) -> Result<Self> {
        let zenohd_path = ZenohdFacade::get_zenohd_binary()?;
        let config = ZenohdFacade::get_config(config_file)?;
        let zenoh_endpoint = ZenohdFacade::get_endpoint_from_config(&config)?;
        Ok(Self {
            zenohd_path,
            config,
            router_process: None,
            zenoh_endpoint,
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

    fn get_endpoint_from_config(config: &zenoh::config::Config) -> Result<ZenohEndpoint> {
        // Get the listen configuration
        let listen_json = config.get_json("listen").map_err(|e| {
            Error::ConfigurationError(format!("Failed to get listen config: {}", e))
        })?;

        let listen: serde_json::Value = serde_json::from_str(&listen_json).map_err(|e| {
            Error::ConfigurationError(format!("Failed to parse listen config: {}", e))
        })?;

        let endpoint_str = listen["endpoints"]["router"][0].as_str().ok_or_else(|| {
            Error::ConfigurationError("No router endpoint found in config".to_string())
        })?;

        let (protocol_str, host_port) = endpoint_str.split_once('/').ok_or_else(|| {
            Error::ConfigurationError(format!("Invalid endpoint format: {}", endpoint_str))
        })?;

        let protocol = match protocol_str {
            "tcp" => ZenohNetProtocol::Tcp,
            "udp" => ZenohNetProtocol::Udp,
            "quic" => ZenohNetProtocol::Quic,
            "ws" => ZenohNetProtocol::Ws,
            _ => {
                return Err(Error::ConfigurationError(format!(
                    "Unknown protocol: {}",
                    protocol_str
                )));
            }
        };

        let (host, port_str) = host_port.split_once(':').ok_or_else(|| {
            Error::ConfigurationError(format!("Invalid host:port format: {}", host_port))
        })?;

        let port = port_str
            .parse::<u16>()
            .map_err(|_| Error::ConfigurationError(format!("Invalid port number: {}", port_str)))?;

        Ok(ZenohEndpoint {
            host: host.to_string(),
            port,
            protocol,
        })
    }

    fn get_config_as_path(&self) -> Result<PathBuf> {
        // Write config to a temporary file that persists by leaking the TempDir
        let temp_dir = Box::new(tempfile::TempDir::new().expect("Failed to create temp dir"));
        // TODO: move this file into the .pixi /etc environment
        let config_path = temp_dir.path().join("zenohd_config.json5");

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
        
        // Leak the temp_dir to keep it alive for the lifetime of the process
        Box::leak(temp_dir);
        
        Ok(config_path)
    }

    /// Renders the Zenoh configuration from the template
    fn render_default_config() -> String {
        let template = ZenohRouterConfigTemplate::default();

        template
            .render()
            .unwrap_or_else(|e| panic!("Failed to render Zenoh config: {}", e))
    }

    /// Starts a zenohd process, using std::process::Command is the recommended way as using the
    /// rust crate directly prevents the user from using plugins/adminspace
    pub fn start_router(&mut self) -> Result<()> {
        let config_path = self.get_config_as_path()?;

        let mut child = Command::new(&self.zenohd_path)
            .arg("-c")
            .arg(&config_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::BackendError(format!("Failed to start zenohd: {}", e)))?;

        // Give zenohd a moment to start up and bind to the port
        std::thread::sleep(std::time::Duration::from_millis(800));

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

        // Store the expected host and port in separate variables
        let expected_host = "127.0.0.1";
        let expected_port = 7447;
        let expected_protocol = "tcp";

        // Write config directly in the test, inspired by the template
        let config_content = format!(
            r#"{{
            "listen": {{
                "endpoints": {{
                    "router": ["{}/{expected_host}:{expected_port}"]
                }}
            }}
        }}"#,
            expected_protocol
        );

        fs::write(&config_path, config_content).expect("Failed to write test config");

        // Create facade with the config file
        let facade = ZenohdFacade::new(Some(config_path));
        if let Err(e) = &facade {
            eprintln!("Error creating facade: {:?}", e);
        }
        assert!(facade.is_ok());

        // Verify that the endpoint was correctly extracted from the config
        let facade = facade.unwrap();
        assert_eq!(facade.zenoh_endpoint.host, expected_host);
        assert_eq!(facade.zenoh_endpoint.port, expected_port);
        assert_eq!(facade.zenoh_endpoint.protocol, ZenohNetProtocol::Tcp);
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
