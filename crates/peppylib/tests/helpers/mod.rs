use pmi::{MessagingEngineContext, Messenger, MessengerBackend};
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
/// `peppylib` needs the existence of a zenohd router to opperate, mostly provided by `peppy serve`.
pub async fn start_messaging_router() -> Result<RouterGuard, pmi::PeppyMessagingInterfaceError> {
    eprintln!("Starting mock messaging router in background (tests)…");

    // Always use mock engine in tests; no external binaries or configs
    let tempdir = TempDir::new().expect("failed to create tempdir for test state");
    let mut messenger = Messenger::new(MessagingEngineContext::new("mock".into(), None))?;
    messenger.start_router().await?;
    eprintln!("mock messaging router started for tests.");

    Ok(RouterGuard {
        messenger: Some(messenger),
        _tempdir: Some(tempdir),
    })
}
