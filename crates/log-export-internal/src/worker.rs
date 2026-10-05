//! The export worker: takes records off the queue, sends them in requests,
//! and retries a request the endpoint did not accept.

use crate::exporter::{LogRecordReceiver, QueuedRecord, Reservation};
use crate::headers::OtlpHeaders;
use crate::otlp::encode::{ExportResource, RequestBuilder};
use crate::otlp::messages::ExportLogsServiceResponse;
use crate::transport::{Transport, TransportError, TransportResponse};
use bytes::Bytes;
use prost::Message;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// The most bytes of records one request carries.
const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
/// How long a request waits for more records after its first one.
const FILL_INTERVAL: Duration = Duration::from_secs(1);
/// The longest wait before the first retry of a request.
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
/// The longest wait before any retry, and the longest `Retry-After` the
/// worker follows.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// How long the worker keeps sending once the daemon stops.
pub const SHUTDOWN_FLUSH: Duration = Duration::from_secs(3);
/// The shortest time between two reports of discarded records.
const DISCARD_REPORT_INTERVAL: Duration = Duration::from_secs(60);

/// What an export worker needs besides the queue and the transport.
pub struct ExportWorkerConfig {
    pub resource: ExportResource,
    /// The host of the endpoint, for the output of the daemon.
    pub endpoint_host: String,
    pub headers: OtlpHeaders,
    /// The file `headers` came from, read again when the endpoint refuses
    /// them.
    pub headers_path: PathBuf,
}

/// Sends the records of one queue to one endpoint until the daemon stops.
pub struct ExportWorker<T> {
    receiver: LogRecordReceiver,
    transport: T,
    config: ExportWorkerConfig,
    /// Called with the number of records discarded since the last call.
    report_discarded: Box<dyn Fn(u64) + Send + Sync>,
    filling: Filling,
    /// A request that was waiting for its retry when the daemon stopped.
    interrupted: Option<Request>,
    /// Records discarded and not reported yet, besides those the queue
    /// counts.
    unreported: u64,
    /// When the next report of discarded records may be written.
    next_report: Option<Instant>,
    /// The failure the output of the daemon last showed, until a request is
    /// delivered.
    shown_failure: Option<String>,
}

/// The records of the request being filled.
#[derive(Default)]
struct Filling {
    builder: RequestBuilder,
    reservations: Vec<Reservation>,
    /// When the request is sent even if it is not full.
    due: Option<Instant>,
}

/// An encoded request. Its records hold their share of the queue until the
/// request is dropped.
struct Request {
    body: Bytes,
    records: u64,
    _reservations: Vec<Reservation>,
}

/// What a request got: the answer of the endpoint, or why there is none.
type Answer = Result<TransportResponse, TransportError>;

/// What became of one attempt to send a request.
#[derive(Debug, PartialEq, Eq)]
enum Attempt {
    /// The endpoint took the request, and refused `rejected` of its records.
    Delivered { rejected: u64 },
    /// The request may succeed later, `after` this long when the endpoint
    /// said so.
    Retry {
        after: Option<Duration>,
        reason: String,
    },
    /// The endpoint refused the headers.
    Unauthorized { status: u16 },
    /// The endpoint refused the request for good.
    Refused { status: u16 },
}

impl Filling {
    fn push(&mut self, queued: QueuedRecord) {
        let (record, reservation) = queued.into_parts();
        self.builder.push(record);
        self.reservations.push(reservation);
        self.due
            .get_or_insert_with(|| Instant::now() + FILL_INTERVAL);
    }

    fn is_empty(&self) -> bool {
        self.reservations.is_empty()
    }

    fn is_full(&self) -> bool {
        self.builder.encoded_bytes() >= MAX_REQUEST_BYTES
    }
}

impl<T: Transport> ExportWorker<T> {
    pub fn new(
        receiver: LogRecordReceiver,
        transport: T,
        config: ExportWorkerConfig,
        report_discarded: impl Fn(u64) + Send + Sync + 'static,
    ) -> Self {
        Self {
            receiver,
            transport,
            config,
            report_discarded: Box::new(report_discarded),
            filling: Filling::default(),
            interrupted: None,
            unreported: 0,
            next_report: None,
            shown_failure: None,
        }
    }

    /// Exports until `shutdown` is cancelled or the queue is closed, then
    /// sends what is queued for up to [`SHUTDOWN_FLUSH`].
    pub async fn run(mut self, shutdown: CancellationToken) {
        let flush_until = self.export(&shutdown).await;
        self.flush(flush_until).await;
    }

    /// Exports until the daemon stops. Returns when the flush ends.
    async fn export(&mut self, shutdown: &CancellationToken) -> Instant {
        loop {
            let ready = tokio::select! {
                biased;
                () = shutdown.cancelled() => false,
                ready = self.fill() => ready,
            };
            if !ready {
                return Instant::now() + SHUTDOWN_FLUSH;
            }
            let request = self.take_request();
            if let Some(flush_until) = self.deliver(request, shutdown).await {
                return flush_until;
            }
        }
    }

    /// Waits until a request is ready: it is full, or its first record is
    /// [`FILL_INTERVAL`] old. Returns `false` once the queue is closed; the
    /// flush sends what the request holds by then.
    async fn fill(&mut self) -> bool {
        loop {
            if self.filling.is_full() {
                return true;
            }
            let next = match self.filling.due {
                None => self.receiver.recv().await,
                Some(due) => match tokio::time::timeout_at(due, self.receiver.recv()).await {
                    Ok(next) => next,
                    Err(_) => return true,
                },
            };
            match next {
                Some(queued) => self.filling.push(queued),
                None => return false,
            }
        }
    }

    fn take_request(&mut self) -> Request {
        let filling = std::mem::take(&mut self.filling);
        Request {
            records: filling.builder.records(),
            body: Bytes::from(filling.builder.encode(&self.config.resource)),
            _reservations: filling.reservations,
        }
    }

    /// Sends `request` until the endpoint takes it or refuses it for good.
    /// Returns when the flush ends if the daemon stopped meanwhile.
    async fn deliver(&mut self, request: Request, shutdown: &CancellationToken) -> Option<Instant> {
        let mut backoff = Backoff::default();
        loop {
            let (answer, flush_until) = match self.post(&request, shutdown).await {
                Posted::Answered(answer) => (answer, None),
                Posted::AnsweredWhileStopping {
                    answer,
                    flush_until,
                } => (answer, Some(flush_until)),
                Posted::Unanswered { flush_until } => {
                    self.unreported = self.unreported.saturating_add(request.records);
                    return Some(flush_until);
                }
            };
            match attempt(answer) {
                Attempt::Delivered { rejected } => {
                    self.delivered(rejected.min(request.records));
                    return flush_until;
                }
                Attempt::Retry { after, reason } => {
                    self.show_failure(format!("{reason}; retrying"));
                    if flush_until.is_some() {
                        // The flush sends it once more.
                        self.interrupted = Some(request);
                        return flush_until;
                    }
                    let wait = backoff.next_wait();
                    let wait = after.map_or(wait, |after| after.min(MAX_BACKOFF).max(wait));
                    tokio::select! {
                        biased;
                        () = shutdown.cancelled() => {
                            self.interrupted = Some(request);
                            return Some(Instant::now() + SHUTDOWN_FLUSH);
                        }
                        () = tokio::time::sleep(wait) => {}
                    }
                }
                Attempt::Unauthorized { status } => {
                    let advice = match self.reload_headers() {
                        HeadersReload::Changed if flush_until.is_none() => continue,
                        HeadersReload::Changed | HeadersReload::Unchanged => {
                            format!("check {}", self.config.headers_path.display())
                        }
                        HeadersReload::Unreadable(error) => error,
                    };
                    self.refused(&request, status, &advice);
                    return flush_until;
                }
                Attempt::Refused { status } => {
                    let advice = match status {
                        300..=399 => {
                            "the daemon follows no redirect; set otlp_endpoint to the URL it \
                             points at"
                        }
                        404 => {
                            "set otlp_endpoint to the base URL of the collector, such as \
                             http://localhost:4318"
                        }
                        _ => "",
                    };
                    self.refused(&request, status, advice);
                    return flush_until;
                }
            }
        }
    }

    /// Posts `request` once. When the daemon stops meanwhile, the request in
    /// flight gets the time of the flush to be answered.
    async fn post(&self, request: &Request, shutdown: &CancellationToken) -> Posted {
        let post = self
            .transport
            .post(request.body.clone(), &self.config.headers);
        tokio::pin!(post);
        tokio::select! {
            biased;
            answer = &mut post => Posted::Answered(answer),
            () = shutdown.cancelled() => {
                let flush_until = Instant::now() + SHUTDOWN_FLUSH;
                match tokio::time::timeout_at(flush_until, &mut post).await {
                    Ok(answer) => Posted::AnsweredWhileStopping { answer, flush_until },
                    Err(_) => Posted::Unanswered { flush_until },
                }
            }
        }
    }

    /// Sends what is queued, one attempt per request, until `flush_until` or
    /// the first request the endpoint does not take. What is left is
    /// discarded and reported in the stack log file.
    async fn flush(&mut self, flush_until: Instant) {
        let mut unsent = 0u64;
        while let Some(request) = self.next_queued_request() {
            if Instant::now() >= flush_until {
                unsent = unsent.saturating_add(request.records);
                continue;
            }
            let post = self
                .transport
                .post(request.body.clone(), &self.config.headers);
            let answer = tokio::time::timeout_at(flush_until, post).await;
            match answer.map(attempt) {
                Ok(Attempt::Delivered { rejected }) => {
                    self.unreported = self
                        .unreported
                        .saturating_add(rejected.min(request.records));
                }
                Ok(
                    Attempt::Retry { .. } | Attempt::Unauthorized { .. } | Attempt::Refused { .. },
                )
                | Err(_) => {
                    unsent = unsent.saturating_add(request.records);
                    break;
                }
            }
        }
        while let Some(request) = self.next_queued_request() {
            unsent = unsent.saturating_add(request.records);
        }
        self.unreported = self.unreported.saturating_add(unsent);
        self.report_discards(When::Now);
    }

    /// The request that was waiting for its retry, or else one of the
    /// records queued now.
    fn next_queued_request(&mut self) -> Option<Request> {
        if let Some(request) = self.interrupted.take() {
            return Some(request);
        }
        while !self.filling.is_full()
            && let Some(queued) = self.receiver.try_recv()
        {
            self.filling.push(queued);
        }
        (!self.filling.is_empty()).then(|| self.take_request())
    }

    /// The endpoint took a request and refused `rejected` of its records.
    fn delivered(&mut self, rejected: u64) {
        if self.shown_failure.take().is_some() {
            tracing::info!("OTLP log export to {} recovered", self.config.endpoint_host);
        }
        self.unreported = self.unreported.saturating_add(rejected);
        self.report_discards(When::Due);
    }

    /// The endpoint refused `request` for good: its records are discarded.
    fn refused(&mut self, request: &Request, status: u16, advice: &str) {
        let separator = if advice.is_empty() { "" } else { "; " };
        self.show_failure(format!(
            "the endpoint refused the request ({status}) and its records are discarded\
             {separator}{advice}"
        ));
        self.unreported = self.unreported.saturating_add(request.records);
        self.report_discards(When::Due);
    }

    /// Hands the number of discarded records to the reporter, when there are
    /// any.
    fn report_discards(&mut self, when: When) {
        self.unreported = self
            .unreported
            .saturating_add(self.receiver.take_discarded());
        if self.unreported == 0 {
            return;
        }
        let now = Instant::now();
        let due = self.next_report.is_none_or(|at| now >= at);
        if when == When::Due && !due {
            return;
        }
        (self.report_discarded)(self.unreported);
        self.unreported = 0;
        self.next_report = Some(now + DISCARD_REPORT_INTERVAL);
    }

    /// Reads the headers file again.
    fn reload_headers(&mut self) -> HeadersReload {
        match OtlpHeaders::load(&self.config.headers_path) {
            Ok(headers) if headers != self.config.headers => {
                self.config.headers = headers;
                HeadersReload::Changed
            }
            Ok(_) => HeadersReload::Unchanged,
            Err(error) => HeadersReload::Unreadable(error),
        }
    }

    /// Shows `failure` in the output of the daemon, once until it changes or
    /// a request is delivered.
    fn show_failure(&mut self, failure: String) {
        if self.shown_failure.as_deref() == Some(failure.as_str()) {
            return;
        }
        tracing::warn!(
            "OTLP log export to {}: {failure}",
            self.config.endpoint_host
        );
        self.shown_failure = Some(failure);
    }
}

/// What became of one post of a request.
enum Posted {
    /// The endpoint answered, or the request got no answer.
    Answered(Answer),
    /// The same, after the daemon began to stop. The flush ends at
    /// `flush_until`.
    AnsweredWhileStopping {
        answer: Answer,
        flush_until: Instant,
    },
    /// The daemon began to stop, and the flush ended with the request still
    /// in flight.
    Unanswered { flush_until: Instant },
}

/// What reading the headers file again found.
enum HeadersReload {
    /// Other headers than the ones the endpoint refused.
    Changed,
    /// The headers the endpoint refused.
    Unchanged,
    /// A file that cannot be read, and why.
    Unreadable(String),
}

/// When a report of discarded records is written.
#[derive(Clone, Copy, PartialEq, Eq)]
enum When {
    /// Now.
    Now,
    /// Now, or with a later report when the last one is under
    /// [`DISCARD_REPORT_INTERVAL`] old.
    Due,
}

/// What `answer` means for the request it answers.
fn attempt(answer: Answer) -> Attempt {
    let response = match answer {
        Ok(response) => response,
        Err(TransportError(reason)) => {
            return Attempt::Retry {
                after: None,
                reason,
            };
        }
    };
    match response.status {
        200..=299 => Attempt::Delivered {
            rejected: rejected_records(&response.body),
        },
        429 | 502 | 503 | 504 => Attempt::Retry {
            after: response.retry_after,
            reason: format!("the endpoint answered {}", response.status),
        },
        401 | 403 => Attempt::Unauthorized {
            status: response.status,
        },
        status => Attempt::Refused { status },
    }
}

/// How many records a successful answer says the endpoint refused. An answer
/// with no body, or with one that is not an OTLP response, refused none.
fn rejected_records(body: &[u8]) -> u64 {
    ExportLogsServiceResponse::decode(body)
        .ok()
        .and_then(|response| response.partial_success)
        .and_then(|partial| u64::try_from(partial.rejected_log_records).ok())
        .unwrap_or_default()
}

/// The waits between the retries of one request: each at most twice the one
/// before, up to [`MAX_BACKOFF`].
struct Backoff {
    ceiling: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            ceiling: FIRST_BACKOFF,
        }
    }
}

impl Backoff {
    /// The next wait: between half the current ceiling and the ceiling, so
    /// daemons that lost one endpoint together retry apart.
    fn next_wait(&mut self) -> Duration {
        let ceiling = self.ceiling;
        self.ceiling = (ceiling * 2).min(MAX_BACKOFF);
        let half = ceiling / 2;
        let half_millis = u64::try_from(half.as_millis()).unwrap_or(u64::MAX);
        half + Duration::from_millis(rand::random_range(0..=half_millis))
    }
}

#[cfg(test)]
mod tests;
