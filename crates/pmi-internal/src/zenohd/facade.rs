use super::super::error::{Error, Result};
use super::ZenohNetProtocol;
use std::env;
use std::fs::File;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use zenoh::config::Config;

/// This structure stores the Zenoh endpoint to be reused by clients by extracting it from the config file
pub struct ZenohEndpoint {
    pub host: String,
    pub port: u16,
    pub protocol: ZenohNetProtocol,
}

/// Checks whether a child process has exited prematurely.
/// Returns the process back if still alive, or an error carrying the tail of
/// the router log if it exited. zenohd's stdout/stderr are redirected to
/// `log_path`, so the diagnostic comes from the file rather than from a pipe.
fn check_process_alive(mut child: Child, log_path: &Path) -> std::result::Result<Child, Error> {
    match child.try_wait() {
        Ok(Some(status)) => Err(Error::BackendError(format!(
            "zenohd exited unexpectedly with status: {}{}",
            status,
            zenohd_log_excerpt(log_path),
        ))),
        Ok(None) => Ok(child),
        Err(e) => Err(Error::BackendError(format!(
            "Failed to check zenohd status: {}",
            e
        ))),
    }
}

/// Builds a short suffix describing why zenohd exited by reading the tail of
/// its redirected log file. Used only on the error path; falls back to the log
/// path when the file is empty or unreadable.
fn zenohd_log_excerpt(log_path: &Path) -> String {
    match std::fs::read_to_string(log_path) {
        Ok(contents) if !contents.trim().is_empty() => {
            let tail = contents
                .lines()
                .rev()
                .take(20)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            format!(" – last zenohd log lines:\n{tail}")
        }
        _ => format!(" (see zenohd log at {})", log_path.display()),
    }
}

/// The Zenoh daemon binary facade. Zenohd is not accessible via the Rust API (or in a very limited fashion).
/// This facade allows calling the binary in the background.
pub struct ZenohdFacade {
    zenohd_path: Option<String>,
    pub zenohd_config_path: PathBuf,
    /// File that receives zenohd's stdout+stderr. These streams must be drained
    /// for the process's lifetime; an unread pipe deadlocks the router once its
    /// buffer fills (a zenohd thread blocks in `write` and stops servicing
    /// sockets). A file sink is bounded by disk and never blocks.
    zenohd_log_path: PathBuf,
    pub router_process: Option<Child>,
    pub zenoh_endpoint: ZenohEndpoint,
}

impl ZenohdFacade {
    /// Creates a new ZenohdFacade instance with a working directory
    pub fn new(zenohd_config_path: impl AsRef<Path>) -> Result<Self> {
        let zenohd_path = ZenohdFacade::get_zenohd_binary();
        let zenoh_endpoint = ZenohdFacade::get_endpoint_from_config(&zenohd_config_path)?;
        let zenohd_config_path = zenohd_config_path.as_ref().to_path_buf();
        // Keep the log next to the generated config (same directory), one file
        // per router port.
        let zenohd_log_path = zenohd_config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!("zenohd_{}.log", zenoh_endpoint.port));
        Ok(Self {
            zenohd_path,
            zenohd_config_path,
            zenohd_log_path,
            router_process: None,
            zenoh_endpoint,
        })
    }

    fn get_zenohd_binary() -> Option<String> {
        // 1) Runtime override for packaged installs / system configuration.
        if let Ok(path) = env::var("PEPPY_ZENOHD_PATH").map(|path| path.trim().to_string())
            && !path.is_empty()
        {
            return Some(path);
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
        let config = Config::from_file(zenohd_config_path).map_err(|e| {
            Error::ConfigurationError(format!("Failed to load zenoh config file: {}", e))
        })?;
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
                "Zenohd binary not found. Install `zenohd` (or place it next to the `peppy` binary), or set PEPPY_ZENOHD_PATH."
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

        // Redirect stdout+stderr to a log file instead of unread pipes: a full
        // pipe buffer blocks a zenohd thread in `write` and deadlocks the whole
        // router. Pin the log level too, so a verbose inherited `RUST_LOG`
        // can't flood the file (override with `PEPPY_ZENOHD_LOG`).
        let log_file = File::create(&self.zenohd_log_path).map_err(|e| {
            Error::BackendError(format!(
                "Failed to create zenohd log file {}: {}",
                self.zenohd_log_path.display(),
                e
            ))
        })?;
        let stderr_file = log_file
            .try_clone()
            .map_err(|e| Error::BackendError(format!("Failed to set up zenohd log file: {}", e)))?;
        let zenohd_log_level =
            env::var("PEPPY_ZENOHD_LOG").unwrap_or_else(|_| "zenoh=warn".to_string());

        let mut child = Command::new(zenohd_path)
            .env("ZENOH_CONFIG", self.zenohd_config_path.as_os_str())
            .env("RUST_LOG", zenohd_log_level)
            .arg("-c")
            .arg(&self.zenohd_config_path)
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(stderr_file))
            .spawn()
            .map_err(|e| Error::BackendError(format!("Failed to start zenohd: {}", e)))?;

        if self.zenoh_endpoint.protocol == ZenohNetProtocol::Tcp {
            tracing::info!(
                "Waiting for Zenoh router to accept connections at {}://{}",
                self.zenoh_endpoint.protocol,
                connect_addr
            );
            let start = std::time::Instant::now();
            let timeout = std::time::Duration::from_secs(30);
            let mut backoff = std::time::Duration::from_millis(10);
            let max_backoff = std::time::Duration::from_millis(500);

            loop {
                child = check_process_alive(child, &self.zenohd_log_path)?;

                match TcpStream::connect(&connect_addr) {
                    Ok(_) => break,
                    Err(_) if start.elapsed() >= timeout => {
                        return Err(Error::BackendError(format!(
                            "zenohd readiness timeout after {}s (TCP {})",
                            timeout.as_secs(),
                            connect_addr
                        )));
                    }
                    Err(_) => {
                        std::thread::sleep(backoff);
                        backoff = (backoff * 2).min(max_backoff);
                    }
                }
            }
        } else {
            child = check_process_alive(child, &self.zenohd_log_path)?;
        }

        tracing::info!(
            "Zenoh router started (config {}, logs {})",
            self.zenohd_config_path.display(),
            self.zenohd_log_path.display()
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
    use super::*;

    #[test]
    fn test_zenohd_facade_creation_with_config() {
        use std::io::Write;
        use tempfile::Builder;

        let expected_host = "127.0.0.1";
        let expected_port = 7447u16;

        // Create a minimal zenoh config file with .json5 extension
        let mut config_file = Builder::new()
            .suffix(".json5")
            .tempfile()
            .expect("Failed to create temp file");
        writeln!(
            config_file,
            r#"{{
                "listen": {{
                    "endpoints": {{
                        "router": ["tcp/{expected_host}:{expected_port}"]
                    }}
                }}
            }}"#
        )
        .expect("Failed to write config");

        let facade = ZenohdFacade::new(config_file.path());
        assert!(facade.is_ok(), "Error creating facade: {:?}", facade.err());

        let facade = facade.unwrap();
        assert_eq!(facade.zenoh_endpoint.host, expected_host);
        assert_eq!(facade.zenoh_endpoint.port, expected_port);
        assert_eq!(facade.zenoh_endpoint.protocol, ZenohNetProtocol::Tcp);

        // The log file sits next to the config, named per router port.
        assert_eq!(
            facade.zenohd_log_path,
            config_file
                .path()
                .parent()
                .unwrap()
                .join(format!("zenohd_{expected_port}.log"))
        );
    }

    #[test]
    fn test_stop_router_multiple_times() {
        use std::io::Write;
        use tempfile::Builder;

        // Create a config file with .json5 extension
        let mut config_file = Builder::new()
            .suffix(".json5")
            .tempfile()
            .expect("Failed to create temp file");
        writeln!(
            config_file,
            r#"{{
                "listen": {{
                    "endpoints": {{
                        "router": ["tcp/127.0.0.1:7447"]
                    }}
                }}
            }}"#
        )
        .expect("Failed to write config");

        let mut facade = ZenohdFacade::new(config_file.path()).expect("Failed to create facade");

        // First stop should succeed (no process to stop)
        assert!(facade.stop_router().is_ok());

        // Second stop should also succeed (idempotent)
        assert!(facade.stop_router().is_ok());

        // Process should remain None
        assert!(facade.router_process.is_none());
    }

    #[test]
    fn test_zenohd_log_excerpt() {
        use std::io::Write;
        use tempfile::Builder;

        // Missing file: fall back to pointing at the path. Build it inside a
        // temp dir (without creating the file) so the test is portable.
        let dir = tempfile::tempdir().expect("Failed to create temp dir");
        let missing = dir.path().join("zenohd_0.log");
        assert!(zenohd_log_excerpt(&missing).contains("see zenohd log"));

        // Present file: include the tail of its contents.
        let mut log = Builder::new()
            .suffix(".log")
            .tempfile()
            .expect("Failed to create temp log");
        writeln!(log, "starting up\nerror: address already in use").expect("write");
        let excerpt = zenohd_log_excerpt(log.path());
        assert!(excerpt.contains("address already in use"));
    }
}
