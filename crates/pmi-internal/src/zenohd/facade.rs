use super::super::error::{Error, Result};
use askama::Template;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::{env, fmt};
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
    zenohd_path: Option<String>,
    pub zenohd_config_path: PathBuf,
    pub router_process: Option<Child>,
    pub zenoh_endpoint: ZenohEndpoint,
}

impl ZenohdFacade {
    /// Creates a new ZenohdFacade instance with a working directory
    pub fn new(zenohd_config_path: Option<impl AsRef<Path>>) -> Result<Self> {
        let zenohd_path = ZenohdFacade::get_zenohd_binary();
        let zenohd_config_path = ZenohdFacade::get_zenohd_config_path(zenohd_config_path);
        let zenoh_endpoint = ZenohdFacade::get_endpoint_from_config(&zenohd_config_path)?;
        Ok(Self {
            zenohd_path,
            zenohd_config_path,
            router_process: None,
            zenoh_endpoint,
        })
    }

    fn get_zenohd_binary() -> Option<String> {
        option_env!("ZENOHD_BINARY_PATH").map(|path| path.to_string())
    }

    fn get_zenohd_config_path(zenohd_config_path: Option<impl AsRef<Path>>) -> PathBuf {
        match zenohd_config_path {
            Some(cfg) => cfg.as_ref().to_path_buf(),
            None => match env::var("ZENOHD_CONFIG") {
                Ok(zenohd_config_path) => PathBuf::from(zenohd_config_path),
                Err(_) => {
                    let config_content = Self::render_default_config();
                    let mut temp_file = tempfile::Builder::new()
                        .prefix("zenohd_config_")
                        .suffix(".json5")
                        .tempfile()
                        .expect("Failed to create temporary zenoh config file");

                    temp_file
                        .write_all(config_content.as_bytes())
                        .expect("Failed to write temporary zenoh config file");

                    let (_, temp_path) = temp_file
                        .keep()
                        .expect("Failed to persist temporary zenoh config file");

                    temp_path
                }
            },
        }
    }

    fn get_endpoint_from_config(zenohd_config_path: impl AsRef<Path>) -> Result<ZenohEndpoint> {
        let config = Config::from_file(zenohd_config_path).unwrap();
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
        let zenohd_path = self.zenohd_path.as_ref().ok_or_else(|| {
            Error::ZenohdError(
                "Zenohd binary not found. Build with: cargo build --features build_zenoh"
                    .to_string(),
            )
        })?;

        let mut child = Command::new(zenohd_path)
            .env("ZENOH_CONFIG", self.zenohd_config_path.as_os_str())
            .arg("-c")
            .arg(&self.zenohd_config_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::BackendError(format!("Failed to start zenohd: {}", e)))?;

        // Give zenohd a moment to start up and bind to the port
        std::thread::sleep(std::time::Duration::from_millis(800));

        // Check if the process is still running
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(Error::BackendError(format!(
                    "zenohd exited unexpectedly with status: {}",
                    status
                )));
            }
            Ok(None) => {
                // Process is still running, which is what we want
                tracing::info!(
                    "Zenoh router started, with config {}",
                    self.zenohd_config_path.to_str().unwrap()
                );
            }
            Err(e) => {
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
    use std::{fs, net::TcpListener};

    use super::*;

    fn pick_free_tcp_port() -> Option<u16> {
        (0..10).find_map(|_| {
            TcpListener::bind(("127.0.0.1", 0)).ok().and_then(|sock| {
                let port = sock.local_addr().ok()?.port();
                // Drop socket to free port for messaging router
                drop(sock);
                Some(port)
            })
        })
    }

    #[test]
    fn test_zenohd_facade_creation_default_config() {
        let facade = ZenohdFacade::new(None::<&Path>);
        assert!(facade.is_ok());
    }

    #[test]
    fn test_zenohd_facade_creation_with_config() {
        // Create a temporary directory that will be cleaned up automatically
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
        let zenohd_config_path = temp_dir.path().join("test_zenoh_config.json5");

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

        fs::write(&zenohd_config_path, config_content).expect("Failed to write test config");

        // Create facade with the config file
        let facade = ZenohdFacade::new(Some(zenohd_config_path.clone()));
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
        let zenohd_config_path = temp_dir.path().join("test_zenoh_config.json5");

        let port = pick_free_tcp_port().unwrap();
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

        fs::write(&zenohd_config_path, config_content).expect("Failed to write test config");

        let mut facade =
            ZenohdFacade::new(Some(zenohd_config_path.clone())).expect("Failed to create facade");

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
        let mut facade = ZenohdFacade::new(None::<&Path>).expect("Failed to create facade");

        // First stop should succeed (no process to stop)
        assert!(facade.stop_router().is_ok());

        // Second stop should also succeed (idempotent)
        assert!(facade.stop_router().is_ok());

        // Process should remain None
        assert!(facade.router_process.is_none());
    }
}
