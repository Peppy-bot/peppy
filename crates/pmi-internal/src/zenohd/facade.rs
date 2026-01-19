use super::super::error::{Error, Result};
use std::net::TcpStream;
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
    pub fn new(zenohd_config_path: impl AsRef<Path>) -> Result<Self> {
        let zenohd_path = ZenohdFacade::get_zenohd_binary();
        let zenoh_endpoint = ZenohdFacade::get_endpoint_from_config(&zenohd_config_path)?;
        Ok(Self {
            zenohd_path,
            zenohd_config_path: zenohd_config_path.as_ref().to_path_buf(),
            router_process: None,
            zenoh_endpoint,
        })
    }

    fn get_zenohd_binary() -> Option<String> {
        // 1) Runtime override for packaged installs / system configuration.
        for key in ["PEPPY_ZENOHD_PATH", "ZENOHD_BINARY_PATH"] {
            if let Ok(path) = env::var(key).map(|path| path.trim().to_string())
                && !path.is_empty()
            {
                return Some(path);
            }
        }

        // 2) Prefer a `zenohd` binary placed next to the current executable.
        // This is important for `sudo peppy ...` where PATH may be restricted by `secure_path`.
        if let Ok(exe_path) = env::current_exe()
            && let Some(exe_dir) = exe_path.parent()
        {
            let candidate = exe_dir.join("zenohd");
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }

        // 3) Compile-time path injected by build script (may be stale for packaged/cargo-installed binaries).
        if let Some(path) = option_env!("ZENOHD_BINARY_PATH") {
            // If this looks like a path, verify it exists to avoid "os error 2" at spawn-time.
            if (path.contains('/') || path.contains('\\')) && !Path::new(path).is_file() {
                // Continue to other discovery mechanisms.
            } else {
                return Some(path.to_string());
            }
        }

        // 4) Fallback to searching PATH.
        if let Some(path_var) = env::var_os("PATH") {
            for dir in env::split_paths(&path_var) {
                let candidate = dir.join("zenohd");
                if candidate.is_file() {
                    return Some(candidate.to_string_lossy().into_owned());
                }
            }
        }

        None
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

    /// Starts a zenohd process, using std::process::Command is the recommended way as using the
    /// rust crate directly prevents the user from using plugins/adminspace
    pub fn start_router(&mut self) -> Result<()> {
        let zenohd_path = self.zenohd_path.as_ref().ok_or_else(|| {
            Error::ZenohdError(
                "Zenohd binary not found. Install `zenohd` (or place it next to the `peppy` binary), or set PEPPY_ZENOHD_PATH/ZENOHD_BINARY_PATH."
                    .to_string(),
            )
        })?;

        let connect_host = if self.zenoh_endpoint.host == "0.0.0.0" {
            "127.0.0.1"
        } else {
            self.zenoh_endpoint.host.as_str()
        };
        let connect_addr = format!("{connect_host}:{}", self.zenoh_endpoint.port);

        if self.zenoh_endpoint.protocol == ZenohNetProtocol::Tcp
            && TcpStream::connect(&connect_addr).is_ok()
        {
            return Err(Error::BackendError(format!(
                "Zenoh router port already in use: {}",
                connect_addr
            )));
        }

        let mut child = Command::new(zenohd_path)
            .env("ZENOH_CONFIG", self.zenohd_config_path.as_os_str())
            .arg("-c")
            .arg(&self.zenohd_config_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::BackendError(format!("Failed to start zenohd: {}", e)))?;

        if self.zenoh_endpoint.protocol == ZenohNetProtocol::Tcp {
            tracing::info!(
                "Waiting for Zenoh router to accept connections at {}://{}",
                self.zenoh_endpoint.protocol,
                connect_addr
            );
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let output = child.wait_with_output().map_err(|e| {
                            Error::BackendError(format!("Failed to capture zenohd output: {}", e))
                        })?;
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        return Err(Error::BackendError(format!(
                            "zenohd exited unexpectedly with status: {}{}",
                            status,
                            if stderr.trim().is_empty() {
                                String::new()
                            } else {
                                format!(" – {}", stderr.trim())
                            }
                        )));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        return Err(Error::BackendError(format!(
                            "Failed to check zenohd status: {}",
                            e
                        )));
                    }
                }

                match TcpStream::connect(&connect_addr) {
                    Ok(_) => break,
                    Err(_) => std::thread::yield_now(),
                }
            }
        } else {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let output = child.wait_with_output().map_err(|e| {
                        Error::BackendError(format!("Failed to capture zenohd output: {}", e))
                    })?;
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(Error::BackendError(format!(
                        "zenohd exited unexpectedly with status: {}{}",
                        status,
                        if stderr.trim().is_empty() {
                            String::new()
                        } else {
                            format!(" – {}", stderr.trim())
                        }
                    )));
                }
                Ok(None) => {}
                Err(e) => {
                    return Err(Error::BackendError(format!(
                        "Failed to check zenohd status: {}",
                        e
                    )));
                }
            }
        }

        tracing::info!(
            "Zenoh router started, with config {}",
            self.zenohd_config_path.to_str().unwrap()
        );

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

impl Drop for ZenohdFacade {
    fn drop(&mut self) {
        if let Err(e) = self.stop_router() {
            tracing::warn!("Failed to stop router during drop: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::zenohd_support::{pick_free_tcp_port, write_zenohd_config};

    use super::*;

    #[test]
    fn test_zenohd_facade_creation_with_config() {
        // Store the expected host and port in separate variables
        let expected_host = "127.0.0.1";
        let expected_port = config::consts::DEFAULT_MESSAGING_PORT;
        let (_temp_dir, zenohd_config_path) =
            write_zenohd_config(expected_host, expected_port).expect("Failed to write test config");

        // Create facade with the config file
        let facade = ZenohdFacade::new(zenohd_config_path.clone());
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
        let port = pick_free_tcp_port();
        let (_temp_dir, zenohd_config_path) =
            write_zenohd_config("127.0.0.1", port).expect("Failed to write test config");

        let mut facade =
            ZenohdFacade::new(zenohd_config_path.clone()).expect("Failed to create facade");

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
        // Create a config with a random port to avoid conflicts
        let port = pick_free_tcp_port();
        let (_temp_dir, zenohd_config_path) =
            write_zenohd_config("127.0.0.1", port).expect("Failed to write test config");

        let mut facade = ZenohdFacade::new(zenohd_config_path).expect("Failed to create facade");

        // First stop should succeed (no process to stop)
        assert!(facade.stop_router().is_ok());

        // Second stop should also succeed (idempotent)
        assert!(facade.stop_router().is_ok());

        // Process should remain None
        assert!(facade.router_process.is_none());
    }
}
