use super::*;
use crate::exporter::LogExporter;
use crate::otlp::messages::{
    ExportLogsPartialSuccess, ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use crate::record::{LogIdentity, LogKind};
use crate::test_fixtures::{headers_file, resource};
use daemon_config::peppy_config::Severity;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tokio::task::JoinHandle;

/// What the scripted endpoint does with one request.
enum Scripted {
    Status(u16),
    RetryAfter {
        status: u16,
        seconds: u64,
    },
    PartialSuccess {
        rejected: i64,
    },
    /// Answers `status` once `after` has passed.
    Delayed {
        after: Duration,
        status: u16,
    },
    NoAnswer,
    Hang,
}

/// One request the scripted endpoint got.
struct Post {
    at: Instant,
    bodies: Vec<String>,
    bytes: usize,
    api_key: Option<String>,
}

/// A transport that answers each request with the next step of its script,
/// and with 200 once the script is over.
#[derive(Clone, Default)]
struct ScriptedTransport {
    script: Arc<Mutex<VecDeque<Scripted>>>,
    posts: Arc<Mutex<Vec<Post>>>,
}

impl ScriptedTransport {
    fn new(script: impl IntoIterator<Item = Scripted>) -> Self {
        Self {
            script: Arc::new(Mutex::new(script.into_iter().collect())),
            posts: Arc::default(),
        }
    }

    fn posts(&self) -> std::sync::MutexGuard<'_, Vec<Post>> {
        self.posts.lock().unwrap()
    }
}

impl Transport for ScriptedTransport {
    async fn post(
        &self,
        body: Bytes,
        headers: &OtlpHeaders,
    ) -> Result<TransportResponse, TransportError> {
        let request = ExportLogsServiceRequest::decode(body.clone()).expect("an OTLP request");
        let bodies = request
            .resource_logs
            .into_iter()
            .flat_map(|resource| resource.scope_logs)
            .flat_map(|scope| scope.log_records)
            .filter_map(|record| record.body?.value)
            .map(|value| match value {
                crate::otlp::messages::any_value::Value::StringValue(text) => text,
                crate::otlp::messages::any_value::Value::BoolValue(flag) => flag.to_string(),
            })
            .collect();
        self.posts().push(Post {
            at: Instant::now(),
            bodies,
            bytes: body.len(),
            api_key: headers
                .as_map()
                .get("x-api-key")
                .map(|value| value.to_str().unwrap().to_owned()),
        });
        let step = self.script.lock().unwrap().pop_front();
        let answer = |status, retry_after, body| {
            Ok(TransportResponse {
                status,
                retry_after,
                body,
            })
        };
        match step.unwrap_or(Scripted::Status(200)) {
            Scripted::Status(status) => answer(status, None, Vec::new()),
            Scripted::RetryAfter { status, seconds } => {
                answer(status, Some(Duration::from_secs(seconds)), Vec::new())
            }
            Scripted::PartialSuccess { rejected } => {
                let response = ExportLogsServiceResponse {
                    partial_success: Some(ExportLogsPartialSuccess {
                        rejected_log_records: rejected,
                        error_message: "over quota".to_owned(),
                    }),
                };
                answer(200, None, response.encode_to_vec())
            }
            Scripted::Delayed { after, status } => {
                tokio::time::sleep(after).await;
                answer(status, None, Vec::new())
            }
            Scripted::NoAnswer => Err(TransportError("cannot connect".to_owned())),
            Scripted::Hang => std::future::pending().await,
        }
    }
}

/// A running worker with everything a test drives and observes.
struct Harness {
    exporter: LogExporter,
    identity: Arc<LogIdentity>,
    transport: ScriptedTransport,
    reported: Arc<Mutex<Vec<u64>>>,
    shutdown: CancellationToken,
    worker: JoinHandle<()>,
    started: Instant,
    _headers_dir: tempfile::TempDir,
    headers_path: PathBuf,
}

impl Harness {
    fn start(script: impl IntoIterator<Item = Scripted>) -> Self {
        let (exporter, receiver) = LogExporter::channel(None);
        Self::start_on(exporter, receiver, script)
    }

    fn start_on(
        exporter: LogExporter,
        receiver: LogRecordReceiver,
        script: impl IntoIterator<Item = Scripted>,
    ) -> Self {
        let (headers_dir, headers_path) = headers_file("{ \"x-api-key\": \"first\" }");
        let transport = ScriptedTransport::new(script);
        let reported = Arc::new(Mutex::new(Vec::new()));
        let config = ExportWorkerConfig {
            resource: resource(),
            endpoint_host: "collector:4318".to_owned(),
            headers: OtlpHeaders::load(&headers_path).unwrap(),
            headers_path: headers_path.clone(),
        };
        let report = {
            let reported = Arc::clone(&reported);
            move |count| reported.lock().unwrap().push(count)
        };
        let shutdown = CancellationToken::new();
        let worker = ExportWorker::new(receiver, transport.clone(), config, report);
        Self {
            exporter,
            identity: Arc::new(LogIdentity::new(LogKind::Run, "/logs/run/node.log")),
            transport,
            reported,
            worker: tokio::spawn(worker.run(shutdown.clone())),
            shutdown,
            started: Instant::now(),
            _headers_dir: headers_dir,
            headers_path,
        }
    }

    fn export(&self, line: &str) {
        self.exporter
            .export_daemon_line(&self.identity, SystemTime::now(), Severity::Info, line);
    }

    /// When each request was posted, since the worker started.
    fn post_times(&self) -> Vec<Duration> {
        self.transport
            .posts()
            .iter()
            .map(|post| post.at - self.started)
            .collect()
    }

    fn post_bodies(&self) -> Vec<Vec<String>> {
        self.transport
            .posts()
            .iter()
            .map(|post| post.bodies.clone())
            .collect()
    }

    fn reported(&self) -> Vec<u64> {
        self.reported.lock().unwrap().clone()
    }

    /// Stops the worker. Returns how long it took to end.
    async fn stop(self) -> Duration {
        let stopped = Instant::now();
        self.shutdown.cancel();
        self.worker.await.expect("the worker ends without a panic");
        Instant::now() - stopped
    }
}

const SECOND: Duration = Duration::from_secs(1);

#[tokio::test(start_paused = true)]
async fn a_request_goes_out_a_second_after_its_first_record() {
    let harness = Harness::start([]);
    harness.export("one");
    tokio::time::sleep(SECOND / 2).await;
    harness.export("two");
    tokio::time::sleep(10 * SECOND).await;

    assert_eq!(harness.post_bodies(), [["one", "two"]]);
    assert_eq!(harness.post_times(), [SECOND]);
    assert_eq!(
        harness.transport.posts()[0].api_key.as_deref(),
        Some("first")
    );
}

#[tokio::test(start_paused = true)]
async fn a_full_request_goes_out_at_once() {
    let harness = Harness::start([]);
    let line = "x".repeat(8 * 1024);
    // 300 lines of 8 KiB are more than one request of 2 MiB holds.
    for _ in 0..300 {
        harness.export(&line);
    }
    tokio::time::sleep(10 * SECOND).await;

    let posts = harness.transport.posts();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[0].at - harness.started, Duration::ZERO);
    assert!(
        (MAX_REQUEST_BYTES..MAX_REQUEST_BYTES + 32 * 1024).contains(&posts[0].bytes),
        "the first request is full: {} bytes",
        posts[0].bytes
    );
    assert_eq!(posts[0].bodies.len() + posts[1].bodies.len(), 300);
}

#[tokio::test(start_paused = true)]
async fn a_request_is_sent_again_until_the_endpoint_takes_it() {
    let harness = Harness::start([
        Scripted::NoAnswer,
        Scripted::Status(503),
        Scripted::Status(200),
    ]);
    harness.export("kept");
    tokio::time::sleep(60 * SECOND).await;

    assert_eq!(harness.post_bodies(), [["kept"], ["kept"], ["kept"]]);
    assert!(harness.reported().is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_wait_between_retries_doubles_up_to_thirty_seconds() {
    let harness = Harness::start((0..8).map(|_| Scripted::NoAnswer));
    harness.export("kept");
    tokio::time::sleep(600 * SECOND).await;

    let times = harness.post_times();
    assert_eq!(times.len(), 9);
    let waits: Vec<Duration> = times.windows(2).map(|pair| pair[1] - pair[0]).collect();
    let ceilings = [1, 2, 4, 8, 16, 30, 30, 30].map(Duration::from_secs);
    for (wait, ceiling) in waits.iter().zip(ceilings) {
        assert!(
            (ceiling / 2..=ceiling).contains(wait),
            "a wait of {wait:?} under a ceiling of {ceiling:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn retry_after_is_followed_up_to_thirty_seconds() {
    let harness = Harness::start([
        Scripted::RetryAfter {
            status: 429,
            seconds: 5,
        },
        Scripted::RetryAfter {
            status: 503,
            seconds: 600,
        },
    ]);
    harness.export("kept");
    tokio::time::sleep(120 * SECOND).await;

    assert_eq!(harness.post_times(), [SECOND, 6 * SECOND, 36 * SECOND]);
}

#[tokio::test(start_paused = true)]
async fn a_retry_after_of_zero_still_waits_for_the_backoff() {
    let harness = Harness::start([
        Scripted::RetryAfter {
            status: 503,
            seconds: 0,
        },
        Scripted::RetryAfter {
            status: 503,
            seconds: 0,
        },
    ]);
    harness.export("kept");
    tokio::time::sleep(60 * SECOND).await;

    let times = harness.post_times();
    assert_eq!(times.len(), 3);
    assert!(times[1] - times[0] >= SECOND / 2, "{times:?}");
    assert!(times[2] - times[1] >= SECOND, "{times:?}");
}

#[tokio::test(start_paused = true)]
async fn rejected_headers_are_read_again_and_the_request_is_sent_with_the_new_ones() {
    let harness = Harness::start([Scripted::Status(401)]);
    std::fs::write(&harness.headers_path, "{ \"x-api-key\": \"second\" }").unwrap();
    harness.export("kept");
    tokio::time::sleep(10 * SECOND).await;

    let keys: Vec<Option<String>> = harness
        .transport
        .posts()
        .iter()
        .map(|post| post.api_key.clone())
        .collect();
    assert_eq!(keys, [Some("first".to_owned()), Some("second".to_owned())]);
    assert_eq!(harness.post_bodies(), [["kept"], ["kept"]]);
    assert!(harness.reported().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_request_is_discarded_when_the_headers_file_cannot_be_read_again() {
    let harness = Harness::start([Scripted::Status(401)]);
    std::fs::write(&harness.headers_path, "{ \"x-api-key\": ").unwrap();
    harness.export("lost");
    tokio::time::sleep(10 * SECOND).await;

    assert_eq!(harness.post_bodies(), [["lost"]]);
    assert_eq!(harness.reported(), [1]);
}

#[tokio::test(start_paused = true)]
async fn a_request_whose_unchanged_headers_are_refused_is_discarded() {
    let harness = Harness::start([Scripted::Status(403)]);
    harness.export("lost");
    tokio::time::sleep(10 * SECOND).await;
    assert_eq!(harness.post_bodies(), [["lost"]]);
    assert_eq!(harness.reported(), [1]);

    harness.export("kept");
    tokio::time::sleep(10 * SECOND).await;
    assert_eq!(harness.post_bodies(), [["lost"], ["kept"]]);
    assert_eq!(harness.reported(), [1]);
}

#[tokio::test(start_paused = true)]
async fn a_refused_request_is_discarded_and_counted() {
    let harness = Harness::start([Scripted::Status(400)]);
    harness.export("lost one");
    harness.export("lost two");
    tokio::time::sleep(10 * SECOND).await;
    harness.export("kept");
    tokio::time::sleep(10 * SECOND).await;

    assert_eq!(
        harness.post_bodies(),
        [vec!["lost one", "lost two"], vec!["kept"]]
    );
    assert_eq!(harness.reported(), [2]);
}

#[tokio::test(start_paused = true)]
async fn records_the_endpoint_refused_of_a_delivered_request_are_counted() {
    let harness = Harness::start([Scripted::PartialSuccess { rejected: 3 }]);
    for line in ["one", "two", "three", "four"] {
        harness.export(line);
    }
    tokio::time::sleep(10 * SECOND).await;

    assert_eq!(harness.reported(), [3]);
}

#[tokio::test(start_paused = true)]
async fn an_answer_cannot_refuse_more_records_than_its_request_held() {
    let harness = Harness::start([
        Scripted::PartialSuccess { rejected: i64::MAX },
        Scripted::PartialSuccess { rejected: -4 },
    ]);
    harness.export("one");
    harness.export("two");
    tokio::time::sleep(10 * SECOND).await;
    harness.export("three");
    tokio::time::sleep(10 * SECOND).await;

    assert_eq!(harness.reported(), [2]);
}

#[tokio::test(start_paused = true)]
async fn discards_of_the_full_queue_are_reported_at_most_once_a_minute() {
    // Room for two short records.
    let (exporter, receiver) = LogExporter::with_capacity(None, 300, 0);
    let harness = Harness::start_on(exporter, receiver, [Scripted::NoAnswer]);
    for line in ["kept one", "kept two", "lost one", "lost two", "lost three"] {
        harness.export(line);
    }
    tokio::time::sleep(10 * SECOND).await;
    assert_eq!(
        harness.post_bodies(),
        [["kept one", "kept two"], ["kept one", "kept two"]]
    );
    assert_eq!(harness.reported(), [3]);

    // A second round of discards inside the minute waits for it to pass.
    for line in ["kept three", "kept four", "lost four"] {
        harness.export(line);
    }
    tokio::time::sleep(10 * SECOND).await;
    assert_eq!(harness.reported(), [3]);

    tokio::time::sleep(60 * SECOND).await;
    harness.export("kept five");
    tokio::time::sleep(10 * SECOND).await;
    assert_eq!(harness.reported(), [3, 1]);
}

#[tokio::test(start_paused = true)]
async fn a_stop_sends_what_is_queued() {
    let harness = Harness::start([]);
    harness.export("queued");
    let bodies = harness.transport.clone();

    let took = harness.stop().await;

    assert_eq!(took, Duration::ZERO);
    assert_eq!(bodies.posts().len(), 1);
    assert_eq!(bodies.posts()[0].bodies, ["queued"]);
}

#[tokio::test(start_paused = true)]
async fn a_stop_sends_the_request_that_was_waiting_for_more_records() {
    let harness = Harness::start([]);
    harness.export("waiting");
    tokio::time::sleep(SECOND / 2).await;
    harness.export("queued");
    let transport = harness.transport.clone();
    assert!(transport.posts().is_empty());

    let took = harness.stop().await;

    assert_eq!(took, Duration::ZERO);
    assert_eq!(transport.posts().len(), 1);
    assert_eq!(transport.posts()[0].bodies, ["waiting", "queued"]);
}

#[tokio::test(start_paused = true)]
async fn a_request_that_fails_while_the_daemon_stops_is_sent_once_more_before_newer_ones() {
    let harness = Harness::start([Scripted::Delayed {
        after: 2 * SECOND,
        status: 503,
    }]);
    harness.export("in flight");
    // The request is posted at one second and answered at three.
    tokio::time::sleep(2 * SECOND).await;
    harness.export("newer");
    let transport = harness.transport.clone();

    let took = harness.stop().await;

    assert_eq!(took, SECOND);
    let bodies: Vec<Vec<String>> = transport
        .posts()
        .iter()
        .map(|post| post.bodies.clone())
        .collect();
    assert_eq!(
        bodies,
        [vec!["in flight"], vec!["in flight"], vec!["newer"]]
    );
}

#[tokio::test(start_paused = true)]
async fn a_stop_during_a_retry_wait_sends_the_request_once_more() {
    let harness = Harness::start([Scripted::NoAnswer, Scripted::NoAnswer]);
    harness.export("queued");
    // The first attempt is at one second, and its retry no sooner than half
    // a second later.
    tokio::time::sleep(SECOND + SECOND / 4).await;
    let transport = harness.transport.clone();
    assert_eq!(transport.posts().len(), 1);

    let took = harness.stop().await;

    assert_eq!(took, Duration::ZERO);
    assert_eq!(transport.posts().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_stop_gives_a_request_in_flight_three_seconds() {
    let harness = Harness::start([Scripted::Hang]);
    harness.export("in flight");
    harness.export("queued");
    tokio::time::sleep(2 * SECOND).await;
    let transport = harness.transport.clone();
    assert_eq!(transport.posts().len(), 1);
    harness.export("late");

    let reported = Arc::clone(&harness.reported);
    let took = harness.stop().await;

    assert_eq!(took, SHUTDOWN_FLUSH);
    assert_eq!(transport.posts().len(), 1);
    // The two records of the request in flight and the late one are reported.
    assert_eq!(*reported.lock().unwrap(), [3]);
}

#[tokio::test(start_paused = true)]
async fn a_stop_reports_the_records_the_flush_could_not_send() {
    let harness = Harness::start([Scripted::Status(404)]);
    harness.export("refused one");
    harness.export("refused two");
    let reported = Arc::clone(&harness.reported);

    harness.stop().await;

    assert_eq!(*reported.lock().unwrap(), [2]);
}

#[tokio::test(start_paused = true)]
async fn a_refused_request_is_reported_without_a_delivery() {
    let harness = Harness::start([Scripted::Status(400)]);
    harness.export("lost");
    tokio::time::sleep(10 * SECOND).await;

    assert_eq!(harness.reported(), [1]);
}

#[tokio::test(start_paused = true)]
async fn the_worker_ends_once_every_exporter_is_gone() {
    let Harness {
        exporter,
        identity,
        transport,
        worker,
        ..
    } = Harness::start([]);
    exporter.export_daemon_line(&identity, SystemTime::now(), Severity::Info, "last");
    drop(exporter);

    worker.await.expect("the worker ends without a panic");

    assert_eq!(transport.posts().len(), 1);
    assert_eq!(transport.posts()[0].bodies, ["last"]);
}

#[test]
fn each_answer_decides_what_becomes_of_the_request() {
    let status = |status| {
        attempt(Ok(TransportResponse {
            status,
            retry_after: None,
            body: Vec::new(),
        }))
    };
    for delivered in [200, 202, 204] {
        assert_eq!(status(delivered), Attempt::Delivered { rejected: 0 });
    }
    for retried in [429, 502, 503, 504] {
        assert!(matches!(
            status(retried),
            Attempt::Retry { after: None, .. }
        ));
    }
    for unauthorized in [401, 403] {
        assert_eq!(
            status(unauthorized),
            Attempt::Unauthorized {
                status: unauthorized
            }
        );
    }
    for refused in [301, 400, 404, 413, 500] {
        assert_eq!(status(refused), Attempt::Refused { status: refused });
    }
    assert_eq!(
        attempt(Err(TransportError("cannot connect".to_owned()))),
        Attempt::Retry {
            after: None,
            reason: "cannot connect".to_owned()
        }
    );
    // A body that is no OTLP response refused nothing.
    assert_eq!(
        attempt(Ok(TransportResponse {
            status: 200,
            retry_after: None,
            body: b"{\"ok\":true}".to_vec(),
        })),
        Attempt::Delivered { rejected: 0 }
    );
    for redirect in [301, 307] {
        assert_eq!(status(redirect), Attempt::Refused { status: redirect });
    }
}
