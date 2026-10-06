//! Support for tests: the records a queue holds, and an OTLP/HTTP receiver
//! that decodes each export request it gets and keeps the records, the path
//! and the headers of the request.

use crate::exporter::LogRecordReceiver;
use crate::otlp::messages::{AnyValue, ExportLogsServiceRequest, KeyValue, any_value};
use crate::record::LogRecord;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use prost::Message;
use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// The records `receiver` holds now, in the order they were exported.
pub fn drain_records(receiver: &mut LogRecordReceiver) -> Vec<LogRecord> {
    std::iter::from_fn(|| receiver.try_recv())
        .map(|queued| queued.record().clone())
        .collect()
}

/// An OTLP/HTTP server on a port of its own.
pub struct OtlpReceiver {
    address: SocketAddr,
    requests: mpsc::UnboundedReceiver<ReceivedRequest>,
    answers: Arc<Mutex<Answers>>,
    server: JoinHandle<()>,
}

/// The statuses the receiver answers with.
struct Answers {
    /// The answers of the next requests, in order.
    next: VecDeque<u16>,
    /// The answer of every other request.
    otherwise: u16,
}

/// One export request, as the receiver got it.
#[derive(Debug)]
pub struct ReceivedRequest {
    pub path: String,
    /// The request headers, by lowercase name. A header sent more than once
    /// holds its values joined by `, `.
    pub headers: BTreeMap<String, String>,
    /// The status the receiver answered.
    pub answered: u16,
    pub records: Vec<ReceivedRecord>,
}

/// One log record of an export request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedRecord {
    /// The attributes of the resource the record belongs to.
    pub resource: BTreeMap<String, String>,
    /// The attributes of the record. A boolean reads `true` or `false`.
    pub attributes: BTreeMap<String, String>,
    pub severity_number: i32,
    pub severity_text: String,
    pub body: String,
    pub time_unix_nano: u64,
    pub observed_time_unix_nano: u64,
}

#[derive(Clone)]
struct Shared {
    requests: mpsc::UnboundedSender<ReceivedRequest>,
    answers: Arc<Mutex<Answers>>,
}

impl OtlpReceiver {
    /// Starts a receiver on `bind`, such as `127.0.0.1:0` for a free loopback
    /// port.
    pub async fn start(bind: &str) -> Self {
        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .expect("the receiver binds its address");
        Self::serve(listener)
    }

    /// Starts a receiver on `preferred`, or on a free loopback port when
    /// another process listens there.
    pub async fn start_or_free(preferred: &str) -> Self {
        let listener = match tokio::net::TcpListener::bind(preferred).await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("a free loopback port binds")
            }
            Err(error) => panic!("the receiver binds {preferred}: {error}"),
        };
        Self::serve(listener)
    }

    fn serve(listener: tokio::net::TcpListener) -> Self {
        let address = listener.local_addr().expect("the receiver has an address");
        let (sender, requests) = mpsc::unbounded_channel();
        let answers = Arc::new(Mutex::new(Answers {
            next: VecDeque::new(),
            otherwise: 200,
        }));
        let app = Router::new()
            .fallback(receive)
            .layer(DefaultBodyLimit::disable())
            .with_state(Shared {
                requests: sender,
                answers: Arc::clone(&answers),
            });
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("the receiver serves");
        });
        Self {
            address,
            requests,
            answers,
            server,
        }
    }

    pub fn port(&self) -> u16 {
        self.address.port()
    }

    /// The `otlp_endpoint` of the receiver for a client on this machine.
    pub fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port())
    }

    /// Answers the next requests with `statuses`, in order.
    pub fn answer_next(&self, statuses: impl IntoIterator<Item = u16>) {
        self.answers
            .lock()
            .expect("the answers are not poisoned")
            .next
            .extend(statuses);
    }

    /// Answers every request with `status` from now on, once the answers of
    /// [`Self::answer_next`] are used up. A new receiver answers 200.
    pub fn answer_with(&self, status: u16) {
        self.answers
            .lock()
            .expect("the answers are not poisoned")
            .otherwise = status;
    }

    /// The next request, once it arrives.
    pub async fn next_request(&mut self) -> ReceivedRequest {
        self.requests
            .recv()
            .await
            .expect("the receiver is still serving")
    }

    /// Every request that arrived since the last call.
    pub fn take_requests(&mut self) -> Vec<ReceivedRequest> {
        std::iter::from_fn(|| self.requests.try_recv().ok()).collect()
    }

    /// The records of every request the receiver answered 200 since the last
    /// call.
    pub fn take_records(&mut self) -> Vec<ReceivedRecord> {
        self.take_requests()
            .into_iter()
            .filter(|request| request.answered == 200)
            .flat_map(|request| request.records)
            .collect()
    }
}

impl Drop for OtlpReceiver {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn receive(
    State(shared): State<Shared>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let Ok(request) = ExportLogsServiceRequest::decode(body) else {
        return StatusCode::BAD_REQUEST;
    };
    let answered = {
        let mut answers = shared.answers.lock().expect("the answers are not poisoned");
        answers.next.pop_front().unwrap_or(answers.otherwise)
    };
    let headers = headers.iter().fold(
        BTreeMap::<String, String>::new(),
        |mut by_name, (name, value)| {
            let value = String::from_utf8_lossy(value.as_bytes());
            by_name
                .entry(name.as_str().to_owned())
                .and_modify(|values| {
                    values.push_str(", ");
                    values.push_str(&value);
                })
                .or_insert_with(|| value.into_owned());
            by_name
        },
    );
    let _ = shared.requests.send(ReceivedRequest {
        path: uri.path().to_owned(),
        headers,
        answered,
        records: records(request),
    });
    StatusCode::from_u16(answered).expect("an answer is a status code")
}

fn records(request: ExportLogsServiceRequest) -> Vec<ReceivedRecord> {
    request
        .resource_logs
        .into_iter()
        .flat_map(|resource_logs| {
            let resource = attributes(
                resource_logs
                    .resource
                    .map(|resource| resource.attributes)
                    .unwrap_or_default(),
            );
            resource_logs
                .scope_logs
                .into_iter()
                .flat_map(|scope_logs| scope_logs.log_records)
                .map(move |record| ReceivedRecord {
                    resource: resource.clone(),
                    attributes: attributes(record.attributes),
                    severity_number: record.severity_number,
                    severity_text: record.severity_text,
                    body: record.body.map(text).unwrap_or_default(),
                    time_unix_nano: record.time_unix_nano,
                    observed_time_unix_nano: record.observed_time_unix_nano,
                })
        })
        .collect()
}

fn attributes(attributes: Vec<KeyValue>) -> BTreeMap<String, String> {
    attributes
        .into_iter()
        .map(|attribute| (attribute.key, attribute.value.map(text).unwrap_or_default()))
        .collect()
}

fn text(value: AnyValue) -> String {
    match value.value {
        Some(any_value::Value::StringValue(text)) => text,
        Some(any_value::Value::BoolValue(flag)) => flag.to_string(),
        None => String::new(),
    }
}
