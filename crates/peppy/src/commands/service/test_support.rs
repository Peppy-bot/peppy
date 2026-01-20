//! Test support utilities for ServeCommand.
//!
//! This module provides configuration and context types for running
//! ServeCommand in test environments with proper isolation.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmi::{Messenger, MessengerBackend, MockAdapter, MockInstance, PeppyMessagingInterfaceError};
#[cfg(feature = "zenoh")]
use pmi::{ZenohAdapter, ZenohdInstance};
use tempfile::TempDir;
use tokio::sync::Mutex;

use crate::daemon_state::DaemonState;

/// Owns a messenger instance (Mock or Zenoh) for RAII cleanup.
///
/// When this handle is dropped, the underlying router is stopped.
pub enum MessengerInstanceHandle {
    /// Mock messenger instance (no external processes)
    Mock(MockInstance),
    /// Zenoh messenger instance (manages zenohd process)
    #[cfg(feature = "zenoh")]
    Zenoh(ZenohdInstance),
}

impl MessengerInstanceHandle {
    /// Returns the port the messenger is listening on.
    pub fn port(&self) -> u16 {
        match self {
            MessengerInstanceHandle::Mock(m) => m.port,
            #[cfg(feature = "zenoh")]
            MessengerInstanceHandle::Zenoh(z) => z.port,
        }
    }
}

/// Configuration for running ServeCommand in test mode.
///
/// This struct holds the pre-configured messenger and related resources
/// needed to run ServeCommand without starting a new router.
pub struct ServeTestConfig {
    /// Pre-configured messenger (session already started)
    pub messenger: Option<Arc<Mutex<Messenger>>>,
    /// Owns the messenger instance for cleanup
    pub instance_handle: Option<MessengerInstanceHandle>,
    /// Custom git hash for DaemonState (defaults to "test-git-hash")
    pub git_hash: Option<String>,
}

impl Default for ServeTestConfig {
    fn default() -> Self {
        Self {
            messenger: None,
            instance_handle: None,
            git_hash: Some("test-git-hash".to_string()),
        }
    }
}

impl ServeTestConfig {
    /// Creates a test config with a mock messenger.
    ///
    /// This is the recommended approach for most tests as it doesn't require
    /// any external processes and is faster.
    pub async fn with_mock() -> Result<Self, PeppyMessagingInterfaceError> {
        let mut instance = MockAdapter::start_router().await?;
        instance.messenger().start_session().await?;
        let messenger = Arc::new(Mutex::new(instance.take_messenger()));

        Ok(Self {
            messenger: Some(messenger),
            instance_handle: Some(MessengerInstanceHandle::Mock(instance)),
            git_hash: Some("test-git-hash".to_string()),
        })
    }

    /// Creates a test config with a zenoh messenger on an ephemeral port.
    ///
    /// Use this when you need to test real zenoh messaging behavior.
    #[cfg(feature = "zenoh")]
    pub async fn with_zenoh() -> Result<Self, PeppyMessagingInterfaceError> {
        Self::with_zenoh_port(None).await
    }

    /// Creates a test config with a zenoh messenger on a specific port.
    ///
    /// Pass `None` for an ephemeral port (recommended for tests to avoid conflicts),
    /// or `Some(port)` for a specific port.
    #[cfg(feature = "zenoh")]
    pub async fn with_zenoh_port(
        port: Option<u16>,
    ) -> Result<Self, PeppyMessagingInterfaceError> {
        let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", port).await?;
        instance.messenger().start_session().await?;
        let messenger = Arc::new(Mutex::new(instance.take_messenger()));

        Ok(Self {
            messenger: Some(messenger),
            instance_handle: Some(MessengerInstanceHandle::Zenoh(instance)),
            git_hash: Some("test-git-hash".to_string()),
        })
    }

    /// Returns the messenger if configured.
    pub fn messenger(&self) -> Option<Arc<Mutex<Messenger>>> {
        self.messenger.clone()
    }

    /// Returns the messaging port if a messenger is configured.
    pub fn messaging_port(&self) -> Option<u16> {
        self.instance_handle.as_ref().map(|h| h.port())
    }
}

/// Messenger backend type selection for tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessengerBackendType {
    /// Use MockAdapter (recommended for unit tests)
    Mock,
    /// Use ZenohAdapter with ephemeral port (for integration tests)
    #[cfg(feature = "zenoh")]
    Zenoh,
}

/// Context for a running serve command in tests.
///
/// This struct holds all the resources needed to interact with a test
/// ServeCommand and ensures proper cleanup on drop.
pub struct ServeTestContext {
    /// Temporary directory for test isolation
    pub temp_dir: TempDir,
    /// Shared messenger for sending commands
    pub messenger: Arc<Mutex<Messenger>>,
    /// Path to the daemon state file
    pub daemon_state_path: PathBuf,
    /// The daemon state for this test (can be used with AppContext::with_messenger_and_state)
    pub daemon_state: DaemonState,
    /// Shutdown token to stop the serve command
    pub shutdown_token: tokio_util::sync::CancellationToken,
    /// Receiver that signals when the serve command is ready
    pub ready_receiver: tokio::sync::oneshot::Receiver<()>,
    /// Owns the messenger instance for cleanup
    _instance_handle: Option<MessengerInstanceHandle>,
}

impl ServeTestContext {
    /// Returns the path to the temporary directory.
    pub fn temp_dir_path(&self) -> &Path {
        self.temp_dir.path()
    }

    /// Returns a clone of the shared messenger.
    pub fn messenger(&self) -> Arc<Mutex<Messenger>> {
        Arc::clone(&self.messenger)
    }

    /// Returns a clone of the daemon state.
    pub fn daemon_state(&self) -> DaemonState {
        self.daemon_state.clone()
    }

    /// Signals the serve command to shut down.
    pub fn shutdown(&self) {
        self.shutdown_token.cancel();
    }

    /// Waits for the serve command to be ready.
    ///
    /// This should be called after spawning the serve command to ensure
    /// it's fully initialized before running test commands.
    pub async fn wait_ready(&mut self) -> Result<(), tokio::sync::oneshot::error::RecvError> {
        // Take ownership of receiver by swapping with a dummy channel
        let (dummy_tx, dummy_rx) = tokio::sync::oneshot::channel();
        let real_rx = std::mem::replace(&mut self.ready_receiver, dummy_rx);
        // Drop the dummy_tx so the dummy_rx will error if ever awaited again
        drop(dummy_tx);
        real_rx.await
    }
}

/// Convenience function to set up a complete test environment for ServeCommand.
///
/// This function:
/// 1. Creates a temporary directory for test isolation
/// 2. Starts the appropriate messenger backend (mock or zenoh)
/// 3. Builds a ServeCommand with master node configured
/// 4. Creates daemon state that can be used with AppContext::with_messenger_and_state
///
/// Returns a context with the test resources and a handle to the built serve command.
/// The shutdown token is included in the context and already configured in the serve command.
///
/// # Example
///
/// ```ignore
/// #[test]
/// fn my_test() {
///     let rt = tokio::runtime::Runtime::new().unwrap();
///     let (mut ctx, handle) = rt.block_on(setup_serve_test(MessengerBackendType::Mock)).unwrap();
///
///     // Start serve in background
///     let serve = handle.into_serve();
///     rt.spawn(serve.execute_async());
///
///     // Wait for serve to be ready
///     rt.block_on(ctx.wait_ready()).unwrap();
///
///     // Create AppContext with daemon state for isolation
///     let node_ctx = Arc::new(AppContext::with_messenger_and_state(
///         temp_dir.path(),
///         ctx.messenger(),
///         ctx.daemon_state(),
///     ));
///
///     // ... run test commands ...
///
///     // Signal shutdown when done
///     ctx.shutdown();
/// }
/// ```
pub async fn setup_serve_test(
    backend: MessengerBackendType,
) -> Result<(ServeTestContext, super::serve::ServeHandle), Box<dyn std::error::Error + Send + Sync>> {
    use super::builder::ServeCommandBuilder;
    use tokio_util::sync::CancellationToken;

    let temp_dir = TempDir::new()?;
    let daemon_state_path = DaemonState::state_file_in(temp_dir.path());

    // Create test config based on backend type
    let test_config = match backend {
        MessengerBackendType::Mock => ServeTestConfig::with_mock().await?,
        #[cfg(feature = "zenoh")]
        MessengerBackendType::Zenoh => ServeTestConfig::with_zenoh().await?,
    };

    let messenger = test_config.messenger.clone().ok_or("messenger not configured")?;

    // Build the serve command with test configuration
    let shutdown_token = CancellationToken::new();

    // Store the instance_handle separately for cleanup in the context
    let instance_handle = test_config.instance_handle;

    // Create a new config without the instance_handle for the builder
    let builder_config = ServeTestConfig {
        messenger: test_config.messenger,
        instance_handle: None,
        git_hash: test_config.git_hash.clone(),
    };

    // Get the port from the original instance_handle
    let messaging_port = instance_handle.as_ref().map(|h| h.port()).unwrap_or(0);

    let builder = ServeCommandBuilder::new(temp_dir.path())?
        .with_test_config(builder_config)
        .with_master_node(Some("test-master".to_string()))?
        .with_daemon_state_path(daemon_state_path.clone())
        .with_shutdown_token(shutdown_token.clone())
        .with_messaging_port(messaging_port);

    let handle = builder.build_with_handle()?;

    // Create daemon state that tests can use with AppContext::with_messenger_and_state
    let daemon_state = DaemonState::new(
        handle.master_node_name(),
        handle.messaging_port(),
        test_config.git_hash.as_deref().unwrap_or("test-git-hash"),
    );

    // Create the ready signal channel
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

    // Configure the serve with the ready signal
    let serve = handle.into_serve().with_ready_signal(ready_tx);

    // Recreate the handle with the updated serve
    let handle = super::serve::ServeHandle::new(
        serve,
        Arc::clone(&messenger),
        daemon_state.master_node_name.clone(),
        daemon_state.messaging_port,
    );

    let context = ServeTestContext {
        temp_dir,
        messenger,
        daemon_state_path,
        daemon_state,
        shutdown_token,
        ready_receiver: ready_rx,
        _instance_handle: instance_handle,
    };

    Ok((context, handle))
}
