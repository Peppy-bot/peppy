use super::{ServeAsyncCommand, ServeFuture};
use crate::{AppContext, AppEvent, Error, Result};
use config::NodeConfigWatcher;
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct NodeWatcher {
    event_sender: broadcast::Sender<AppEvent>,
}

impl NodeWatcher {
    pub fn new(ctx: &AppContext) -> Self {
        let event_sender = ctx.event_sender();
        Self { event_sender }
    }

    /// Transmit changes detected in NodeConfigWatcher to the broader AppContext. Adds cleaner separation of
    /// concerns at the cost of a little bit of overhead on messages relaying.
    async fn watch_nodes(&self) -> Result<()> {
        let root_dir = std::env::current_dir().expect("Failed to get current directory");
        let watcher =
            NodeConfigWatcher::new(root_dir).map_err(|err| Error::NodeWatcher(err.to_string()))?;

        let mut rx = watcher.subscribe();
        let mut watcher_task = watcher
            .start()
            .await
            .map_err(|err| Error::NodeWatcher(err.to_string()))?;

        let initial_state = rx.borrow().clone();
        self.event_sender
            .send(AppEvent::NodeConfigChanged(initial_state))
            .map_err(|err| Error::NodeWatcher(err.to_string()))?;

        let watcher_result = loop {
            tokio::select! {
                changed = rx.changed() => {
                    changed.map_err(|err| Error::NodeWatcher(err.to_string()))?;
                    let event = AppEvent::NodeConfigChanged(rx.borrow().clone());
                    self.event_sender.send(event).map_err(|err| Error::NodeWatcher(err.to_string()))?;
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
