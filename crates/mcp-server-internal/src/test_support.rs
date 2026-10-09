//! What the tests of the node bridges and of the daemon bridge share: the
//! runtime surface of a task, scripted, a runtime whose clock moves only
//! when the test advances it, and the reader of a call's event stream.

use crate::bridges::TaskSurface;
use peppylib::messaging::MessengerHandle;
use peppylib::testing::READINESS_TIMEOUT;
use serde_json::Value;
use std::future::Future;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

/// The runtime surface, scripted: cancellation fires when the test says so
/// and every feedback message is kept for the assertions.
pub(crate) struct ScriptedSurface {
    pub(crate) cancel: CancellationToken,
    feedback: watch::Sender<Vec<String>>,
    /// How many times the bridge has asked for the client's cancel: once
    /// per turn of the loop that follows the goal.
    cancel_watches: watch::Sender<usize>,
}

impl ScriptedSurface {
    pub(crate) fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
            feedback: watch::Sender::new(Vec::new()),
            cancel_watches: watch::Sender::new(0),
        }
    }

    /// Returns once the bridge follows the admitted goal: its bound is
    /// armed, and it watches for the client's cancel.
    pub(crate) async fn followed(&self) {
        let mut watches = self.cancel_watches.subscribe();
        tokio::time::timeout(READINESS_TIMEOUT, watches.wait_for(|watches| *watches > 0))
            .await
            .expect("the bridge follows the goal")
            .expect("the surface outlives the wait");
    }

    pub(crate) fn feedback(&self) -> Vec<String> {
        self.feedback.borrow().clone()
    }

    /// Returns once the bridge has reported `count` feedback messages.
    pub(crate) async fn reported(&self, count: usize) {
        let mut reported = self.feedback.subscribe();
        tokio::time::timeout(
            READINESS_TIMEOUT,
            reported.wait_for(|messages| messages.len() >= count),
        )
        .await
        .unwrap_or_else(|_| panic!("the bridge never reported feedback message {count}"))
        .expect("the surface outlives the wait");
    }
}

impl TaskSurface for ScriptedSurface {
    fn report_feedback(&self, message: String) {
        self.feedback.send_modify(|messages| messages.push(message));
    }

    fn cancel_requested(&self) -> impl Future<Output = ()> + Send {
        self.cancel_watches.send_modify(|watches| *watches += 1);
        self.cancel.cancelled()
    }
}

/// Counts the warnings one module logs on the thread it runs on. A feedback
/// message that does not convert leaves no other trace, so the count is how
/// a test knows the bridge has taken one.
struct ModuleWarnings {
    module: &'static str,
    logged: watch::Sender<usize>,
}

impl ModuleWarnings {
    fn counts(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() == tracing::Level::WARN && metadata.target() == self.module
    }
}

impl tracing::Subscriber for ModuleWarnings {
    fn register_callsite(
        &self,
        _metadata: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        // Every test thread has a subscriber of its own: each event asks the
        // one of the thread it happens on.
        tracing::subscriber::Interest::sometimes()
    }

    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        self.counts(metadata)
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        if self.counts(event.metadata()) {
            self.logged.send_modify(|logged| *logged += 1);
        }
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

/// A bridge's work on a runtime of its own, whose clock moves only when the
/// test advances it. The mesh and the provider keep real time, so between
/// two advances every step is one of the peers acting, and the bridge's
/// bounds see exactly the time the test gives them. The runtime runs on one
/// thread, so what the bridge does in one step, such as showing a message
/// and starting a new window, is never split by an advance. The runtime
/// runs the tasks the work spawned until the test drops this handle, also
/// after the work gave its output.
pub(crate) struct PausedRuntime<T> {
    advances: mpsc::UnboundedSender<(Duration, oneshot::Sender<()>)>,
    /// How many warnings the module under test has logged.
    warnings: watch::Receiver<usize>,
    output: oneshot::Receiver<T>,
}

impl<T: Send + 'static> PausedRuntime<T> {
    /// Starts the work `start` gives on a paused runtime, counting the
    /// warnings `module` logs there. `session` is the mesh session the work
    /// uses: Zenoh refuses to close a session from a current-thread runtime,
    /// so the session outlives the runtime even when the test ends first, and
    /// the tasks the runtime drops never hold its last handle.
    pub(crate) fn spawn<F, Fut>(session: MessengerHandle, module: &'static str, start: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
    {
        let (advances, mut advance_requests) =
            mpsc::unbounded_channel::<(Duration, oneshot::Sender<()>)>();
        let (logged, warnings) = watch::channel(0);
        let (output_sender, output) = oneshot::channel();
        tokio::task::spawn_blocking(move || {
            let _warnings = tracing::subscriber::set_default(ModuleWarnings { module, logged });
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .expect("the paused runtime builds");
            runtime.block_on(async move {
                // A blocking task that is still running keeps Tokio from
                // moving the paused clock on its own while the work waits on
                // the mesh: only an advance the test asks for moves it.
                let (hold_clock, clock_released) = std::sync::mpsc::channel::<()>();
                let clock_guard = tokio::task::spawn_blocking(move || {
                    let _ = clock_released.recv();
                });
                let mut work = tokio::spawn(start());
                let mut output_sender = Some(output_sender);
                loop {
                    tokio::select! {
                        output = &mut work, if output_sender.is_some() => {
                            let output = output.expect("the work does not panic");
                            if let Some(sender) = output_sender.take() {
                                let _ = sender.send(output);
                            }
                        }
                        request = advance_requests.recv() => {
                            // The test dropped its end: the runtime's work is
                            // over.
                            let Some((by, advanced)) = request else { break };
                            tokio::time::advance(by).await;
                            let _ = advanced.send(());
                        }
                    }
                }
                drop(hold_clock);
                clock_guard.await.expect("the clock guard ends");
            });
            drop(runtime);
            drop(session);
        });
        Self {
            advances,
            warnings,
            output,
        }
    }

    /// Moves the runtime's clock forward by `by`, firing every timer due by
    /// then.
    pub(crate) async fn advance(&self, by: Duration) {
        let (advanced, done) = oneshot::channel();
        self.advances
            .send((by, advanced))
            .expect("the paused runtime still runs");
        done.await.expect("the runtime's clock advanced");
    }

    /// Returns once the module under test has logged `count` warnings.
    pub(crate) async fn logged_warnings(&self, count: usize) {
        let mut warnings = self.warnings.clone();
        tokio::time::timeout(
            READINESS_TIMEOUT,
            warnings.wait_for(|logged| *logged >= count),
        )
        .await
        .unwrap_or_else(|_| panic!("the bridge never logged warning {count}"))
        .expect("the bridge's log outlives the wait");
    }

    /// The output of the work, once it gives it. The runtime goes on with
    /// the tasks the work spawned.
    pub(crate) async fn ended(&mut self) -> T {
        tokio::time::timeout(READINESS_TIMEOUT, &mut self.output)
            .await
            .expect("the work gives its output")
            .expect("the paused runtime runs until the work gives its output")
    }

    /// The output of the work; the runtime stops after it.
    pub(crate) async fn outcome(mut self) -> T {
        self.ended().await
    }
}

/// The response stream of a call, read one server-sent event at a time.
pub(crate) struct Events {
    response: reqwest::Response,
    unread: String,
}

impl Events {
    /// The stream of a call whose headers say it streams events.
    pub(crate) fn of(response: reqwest::Response) -> Self {
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        assert!(
            content_type.starts_with("text/event-stream"),
            "the call answers with an event stream, not with {content_type:?}"
        );
        Self {
            response,
            unread: String::new(),
        }
    }

    /// The JSON-RPC message the next event carries. An event without data
    /// carries no message and is passed over.
    pub(crate) async fn next_message(&mut self) -> Value {
        loop {
            if let Some(end) = self.unread.find("\n\n") {
                let event: String = self.unread.drain(..end + 2).collect();
                let data: Vec<&str> = event
                    .lines()
                    .filter_map(|line| line.strip_prefix("data:"))
                    .map(str::trim_start)
                    .collect();
                if data.is_empty() {
                    continue;
                }
                return serde_json::from_str(&data.join("\n"))
                    .expect("an event carries one JSON-RPC message");
            }
            let chunk = tokio::time::timeout(READINESS_TIMEOUT, self.response.chunk())
                .await
                .expect("the next event arrives")
                .expect("the stream is readable")
                .expect("the stream carries another event");
            let text = std::str::from_utf8(&chunk).expect("server-sent events are text");
            self.unread.push_str(&text.replace("\r\n", "\n"));
        }
    }
}
