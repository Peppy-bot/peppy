//! The events of one exposure that its subscriptions relay to clients, and
//! how a subscription catches up on the events that it did not receive.
//!
//! rmcp acknowledges a `subscriptions/listen` before it starts the handler
//! of the subscription, and the client takes the acknowledgment as the
//! start of the subscription. An event announced between the arrival of
//! the request and the start of the handler reaches no receiver of the
//! channel. So the HTTP service of the endpoint stamps each request with
//! the [`EventStamp`] of the moment it arrives, and the handler starts with
//! [`CatalogEvents::listen_since`] that stamp: it gets the events announced
//! after the stamp that its receiver did not get, then each later one
//! through its receiver.
//!
//! The catch-up keeps latest-snapshot semantics: it holds the latest
//! announcement of each distinct event, so a resource that took many
//! snapshots since the stamp is one notification, and the client reads the
//! newest snapshot.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// An event the subscription forwarder relays to listening clients: a
/// resource took a new snapshot, or the resource list changed because a
/// robot joined or left a per-robot surface.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum CatalogEvent {
    ResourceUpdated { uri: String },
    ResourceListChanged,
}

/// A position in the events of one exposure: the count of the events
/// announced before it. A stamp taken when a request arrives marks the
/// events that come after the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct EventStamp(u64);

/// The channel of the events of one exposure, with the position of the
/// latest announcement of each distinct event. Cloning it shares the
/// channel.
#[derive(Debug, Clone)]
pub(crate) struct CatalogEvents(Arc<Channel>);

#[derive(Debug)]
struct Channel {
    sender: broadcast::Sender<CatalogEvent>,
    /// Held while an event is announced and while a subscription starts,
    /// so each event reaches a subscription once: through the catch-up of
    /// a subscription that starts after the announcement, or through the
    /// receiver of one that started before it.
    journal: Mutex<Journal>,
}

#[derive(Debug, Default)]
struct Journal {
    /// The count of the events announced so far.
    announced: u64,
    /// The position of the latest announcement of each distinct event. It
    /// holds one entry for each resource URI that ever took a snapshot,
    /// and one for the list change.
    latest: HashMap<CatalogEvent, u64>,
}

impl CatalogEvents {
    /// A channel whose receivers each hold up to `capacity` events that
    /// they did not read yet.
    pub(crate) fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self(Arc::new(Channel {
            sender,
            journal: Mutex::new(Journal::default()),
        }))
    }

    /// Tells the listening subscriptions of `event`, and keeps its position
    /// for the subscriptions that start later.
    pub(crate) fn announce(&self, event: CatalogEvent) {
        let mut journal = self.journal();
        journal.announced += 1;
        let position = journal.announced;
        journal.latest.insert(event.clone(), position);
        // Send fails only when nobody listens, which is fine.
        let _ = self.0.sender.send(event);
    }

    /// The position of the events now.
    pub(crate) fn stamp(&self) -> EventStamp {
        EventStamp(self.journal().announced)
    }

    /// Starts a subscription whose request arrived at `since`: the receiver
    /// of each event announced from now on, and the latest announcement of
    /// each distinct event announced after `since` and before now, oldest
    /// first.
    pub(crate) fn listen_since(
        &self,
        since: EventStamp,
    ) -> (broadcast::Receiver<CatalogEvent>, Vec<CatalogEvent>) {
        let journal = self.journal();
        let receiver = self.0.sender.subscribe();
        let mut missed: Vec<(u64, &CatalogEvent)> = journal
            .latest
            .iter()
            .filter(|&(_, &position)| position > since.0)
            .map(|(event, &position)| (position, event))
            .collect();
        missed.sort_unstable_by_key(|&(position, _)| position);
        let missed = missed.into_iter().map(|(_, event)| event.clone()).collect();
        (receiver, missed)
    }

    fn journal(&self) -> std::sync::MutexGuard<'_, Journal> {
        self.0
            .journal
            .lock()
            .expect("the journal lock is never poisoned")
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::{CatalogEvent, CatalogEvents};
    use tokio::sync::broadcast;

    /// The receiver of each event of `events` announced from now on.
    pub(crate) fn listen_from_now(events: &CatalogEvents) -> broadcast::Receiver<CatalogEvent> {
        let (receiver, _) = events.listen_since(events.stamp());
        receiver
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn updated(uri: &str) -> CatalogEvent {
        CatalogEvent::ResourceUpdated {
            uri: uri.to_owned(),
        }
    }

    #[test]
    fn a_subscription_catches_up_on_the_events_announced_after_its_stamp() {
        let events = CatalogEvents::new(16);
        events.announce(updated("peppy://resource/before"));
        let since = events.stamp();
        events.announce(updated("peppy://resource/status"));
        events.announce(CatalogEvent::ResourceListChanged);
        events.announce(updated("peppy://resource/frame"));

        let (mut receiver, missed) = events.listen_since(since);

        assert_eq!(
            missed,
            [
                updated("peppy://resource/status"),
                CatalogEvent::ResourceListChanged,
                updated("peppy://resource/frame"),
            ],
            "the events after the stamp, oldest first, and none before it"
        );
        assert!(
            receiver.try_recv().is_err(),
            "the receiver holds no event of the catch-up"
        );
    }

    #[test]
    fn the_catch_up_holds_the_latest_announcement_of_each_event() {
        let events = CatalogEvents::new(16);
        let since = events.stamp();
        events.announce(updated("peppy://resource/status"));
        events.announce(CatalogEvent::ResourceListChanged);
        events.announce(updated("peppy://resource/status"));

        let (_, missed) = events.listen_since(since);

        assert_eq!(
            missed,
            [
                CatalogEvent::ResourceListChanged,
                updated("peppy://resource/status"),
            ],
            "a resource that took two snapshots is one event, at its latest position"
        );
    }

    #[test]
    fn an_event_announced_after_the_start_reaches_the_receiver_alone() {
        let events = CatalogEvents::new(16);
        let since = events.stamp();
        let (mut receiver, missed) = events.listen_since(since);
        events.announce(updated("peppy://resource/status"));

        assert!(missed.is_empty(), "nothing was announced before the start");
        assert_eq!(
            receiver.try_recv().expect("the receiver gets the event"),
            updated("peppy://resource/status")
        );
        assert!(receiver.try_recv().is_err(), "it gets the event once");
    }

    #[test]
    fn a_stamp_counts_the_events_announced_before_it() {
        let events = CatalogEvents::new(16);
        let first = events.stamp();
        events.announce(CatalogEvent::ResourceListChanged);
        let second = events.stamp();

        assert!(first < second);
        assert_eq!(events.stamp(), second, "a stamp alone announces nothing");
    }
}
