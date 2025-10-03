use std::path::PathBuf;

use super::{ServeAsyncCommand, ServeFuture};
use crate::{AppContext, AppEvent, Error, Result};
use config::FSNodeConfigWatcher;
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct NodeWatcher {
    from_dir: PathBuf,
    event_sender: broadcast::Sender<AppEvent>,
}

impl NodeWatcher {
    pub fn new(ctx: &AppContext) -> Self {
        let event_sender = ctx.event_sender();
        Self {
            event_sender,
            from_dir: ctx.root_dir.clone(),
        }
    }

    /// Transmit changes detected in NodeConfigWatcher to the broader AppContext. Adds cleaner separation of
    /// concerns at the cost of a little bit of overhead on messages relaying.
    async fn watch_nodes(&self) -> Result<()> {
        let watcher = FSNodeConfigWatcher::new(&self.from_dir)
            .map_err(|err| Error::NodeWatcher(err.to_string()))?;

        let mut rx = watcher.subscribe();
        let mut watcher_task = watcher
            .start()
            .await
            .map_err(|err| Error::NodeWatcher(err.to_string()))?;

        let initial_state = rx.borrow().clone();
        // Ignore missing-receiver errors: watcher must stay alive even if no subscriber is ready yet.
        let _ = self
            .event_sender
            .send(AppEvent::NodeConfigChanged(initial_state));

        let watcher_result = loop {
            tokio::select! {
                changed = rx.changed() => {
                    changed.map_err(|err| Error::NodeWatcher(err.to_string()))?;
                    let event = AppEvent::NodeConfigChanged(rx.borrow().clone());
                    // Broadcast best-effort; lack of listeners simply means nobody subscribed yet.
                    let _ = self.event_sender.send(event);
                }
                result = &mut watcher_task => break result,
            }
        };

        match watcher_result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(Error::NodeWatcher(err.to_string())),
            Err(err) => Err(Error::NodeWatcher(err.to_string())),
        }
    }
}

impl ServeAsyncCommand for NodeWatcher {
    fn run(&self) -> ServeFuture {
        let this = self.clone();
        Box::pin(async move { this.watch_nodes().await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context;
    use config::consts::PEPPY_CONFIG_FILE;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::time::timeout;

    fn write_config(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(PEPPY_CONFIG_FILE);
        let json5 = format!(
            r#"{{
  manifest: {{
    name: "{name}",
    tag: "0.1.0",
    launch_cmd: ["cargo", "run", "--release"]
  }}
}}"#
        );
        fs::write(&path, json5).expect("write config");
        path
    }

    #[tokio::test]
    async fn broadcasts_initial_state() {
        let temp = TempDir::new().expect("tempdir");
        let config_path = write_config(temp.path(), "initial");

        let ctx = AppContext::new(context::DEFAULT_CHANNEL_CAPACITY, temp.path());
        let mut events = ctx.subscribe();
        let watcher = NodeWatcher::new(&ctx);
        let watcher_handle = tokio::spawn(watcher.run());

        let event = timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("initial event timeout")
            .expect("channel closed");

        match event {
            AppEvent::NodeConfigChanged(state) => {
                let entry = state
                    .get(&config_path)
                    .expect("state contains initial config");
                let config = entry.as_ref().expect("config parses");
                assert_eq!(config.manifest.name.as_str(), "initial");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        watcher_handle.abort();
        let _ = watcher_handle.await;
    }

    #[tokio::test]
    async fn propagates_updates() {
        let temp = TempDir::new().expect("tempdir");
        let config_path = write_config(temp.path(), "initial");

        let ctx = AppContext::new(context::DEFAULT_CHANNEL_CAPACITY, temp.path());
        let mut events = ctx.subscribe();
        let watcher = NodeWatcher::new(&ctx);
        let watcher_handle = tokio::spawn(watcher.run());

        // Consume initial state
        timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("initial event timeout")
            .expect("channel closed");

        // Modify configuration on disk
        write_config(temp.path(), "updated");

        let event = timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("update event timeout")
            .expect("channel closed");

        match event {
            AppEvent::NodeConfigChanged(state) => {
                let entry = state
                    .get(&config_path)
                    .expect("state contains updated config");
                let config = entry.as_ref().expect("config parses");
                assert_eq!(config.manifest.name.as_str(), "updated");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        watcher_handle.abort();
        let _ = watcher_handle.await;
    }
}
