use askama::Template;
use pmi::{
    MessagingEngineContext, Messenger, MessengerBackend, ZenohNetProtocol,
    ZenohRouterConfigTemplate,
};
use std::fs;
use std::net::TcpListener;
use tempfile::TempDir;
use tokio::runtime::Runtime;

/// RAII guard for a Messaging router started via `pmi::Messenger`.
/// On drop, it stops the router.
pub struct RouterGuard {
    messenger: Option<Messenger>,
    _tempdir: Option<TempDir>,
}

impl Drop for RouterGuard {
    fn drop(&mut self) {
        if let Some(mut messenger) = self.messenger.take() {
            // If we're already inside a Tokio runtime, avoid creating a nested runtime.
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                // Fire-and-forget stop to avoid blocking in Drop
                handle.spawn(async move {
                    let _ = messenger.stop_router().await;
                });
            } else {
                // Otherwise, stop the router in a dedicated runtime to safely await in Drop
                if let Ok(rt) = Runtime::new() {
                    let _ = rt.block_on(async move { messenger.stop_router().await });
                } else {
                    // Fallback: attempt best-effort stop in a background thread with a new runtime
                    let _ = std::thread::spawn(move || {
                        if let Ok(rt) = Runtime::new() {
                            let _ = rt.block_on(async move { messenger.stop_router().await });
                        }
                    })
                    .join();
                }
            }
            eprintln!("messaging router stopped!");
        }
    }
}

/// Starts a messaging router and returns a guard that will stop the router when dropped.
/// `peppycl` needs the existence of a zenohd router to opperate, mostly provided by `peppy serve`.
/// However here we do not use `peppy serve` directly because `peppycl` is itself a dependency of `peppy` to
/// start its own root_node.
pub async fn start_messaging_router() -> Result<RouterGuard, pmi::PeppyMessagingInterfaceError> {
    eprintln!("Starting messaging router in background (tests)…");

    let engine = "zenoh";

    // Allocate an ephemeral free port for this test instance
    let port = pick_free_tcp_port().unwrap_or(0);
    let port = if port == 0 { 7447 } else { port };

    // Persist a per-test router config in a tempdir to avoid clashes (used by zenoh)
    let tempdir = TempDir::new().expect("failed to create tempdir for router config");
    let cfg_path = tempdir.path().join("router.json5");
    let cfg = render_default_router_config(port);
    fs::write(&cfg_path, cfg).expect("failed to write router config");

    // Try requested engine first
    let mut messenger = Messenger::new(MessagingEngineContext::new(
        engine.into(),
        Some(cfg_path.clone()),
    ))?;

    match messenger.start_router().await {
        Ok(()) => {
            eprintln!(
                "messaging router started on {}/127.0.0.1:{}!",
                &engine, port
            );
            return Ok(RouterGuard {
                messenger: Some(messenger),
                _tempdir: Some(tempdir),
            });
        }
        Err(e) => {
            eprintln!(
                "failed to start messaging router with engine '{}': {:?}",
                &engine, e
            );
            eprintln!("falling back to 'mock' engine for tests.");
        }
    }

    // Fallback to mock engine that requires no external binary
    let mut mock_messenger = Messenger::new(MessagingEngineContext::new("mock".into(), None))?;
    mock_messenger.start_router().await?;
    eprintln!("mock messaging router started for tests.");

    Ok(RouterGuard {
        messenger: Some(mock_messenger),
        _tempdir: Some(tempdir),
    })
}

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

fn render_default_router_config(port: u16) -> String {
    let template = ZenohRouterConfigTemplate {
        host: String::from("127.0.0.1"),
        port,
        protocol: ZenohNetProtocol::default(),
    };

    template
        .render()
        .unwrap_or_else(|e| panic!("Failed to render Zenoh config: {}", e))
}
