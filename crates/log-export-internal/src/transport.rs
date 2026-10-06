//! How an encoded export request reaches the endpoint.

use crate::headers::OtlpHeaders;
use bytes::Bytes;
use daemon_config::peppy_config::OtlpEndpoint;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue, RETRY_AFTER};
use std::future::Future;
use std::time::Duration;

/// How long one request may take, from the connection to the last byte of
/// the answer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// The media type of a protobuf OTLP request.
const PROTOBUF: &str = "application/x-protobuf";
/// The most bytes of an answer's body the daemon reads.
const MAX_ANSWER_BYTES: usize = 64 * 1024;

/// What the endpoint answered.
#[derive(Debug)]
pub struct TransportResponse {
    pub status: u16,
    /// The `Retry-After` header, when it holds a number of seconds.
    pub retry_after: Option<Duration>,
    /// The start of the body, up to `MAX_ANSWER_BYTES`.
    pub body: Vec<u8>,
}

/// Why a request got no answer.
#[derive(Debug)]
pub struct TransportError(pub String);

/// Sends encoded export requests to one endpoint.
pub trait Transport: Send + Sync + 'static {
    /// Posts `body` with `headers`, and returns the answer.
    fn post(
        &self,
        body: Bytes,
        headers: &OtlpHeaders,
    ) -> impl Future<Output = Result<TransportResponse, TransportError>> + Send;
}

/// The OTLP/HTTP transport: a `POST` of the protobuf request to the logs URL
/// of the endpoint.
pub struct HttpTransport {
    client: reqwest::Client,
    logs_url: String,
}

impl HttpTransport {
    pub fn new(endpoint: &OtlpEndpoint) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            // The records and the headers are for the configured endpoint
            // alone: the client follows no redirect and uses no proxy of the
            // environment.
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .user_agent(format!("peppy/{}", daemon_config::consts::PEPPY_VERSION))
            .build()
            .map_err(|error| format!("cannot set up the HTTP client of the log export: {error}"))?;
        Ok(Self {
            client,
            logs_url: endpoint.logs_url().to_owned(),
        })
    }
}

impl Transport for HttpTransport {
    async fn post(
        &self,
        body: Bytes,
        headers: &OtlpHeaders,
    ) -> Result<TransportResponse, TransportError> {
        // The body is protobuf whatever the headers file says.
        let mut headers = headers.as_map().clone();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(PROTOBUF));
        let mut response = self
            .client
            .post(&self.logs_url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|error| TransportError(reason(&error)))?;
        let status = response.status().as_u16();
        let retry_after = retry_after(response.headers());
        let mut body = Vec::new();
        // An answer that ends early still carries its status.
        while body.len() < MAX_ANSWER_BYTES
            && let Ok(Some(chunk)) = response.chunk().await
        {
            let room = MAX_ANSWER_BYTES - body.len();
            body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        }
        Ok(TransportResponse {
            status,
            retry_after,
            body,
        })
    }
}

/// The `Retry-After` of an answer, when it is a number of seconds.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
        .map(Duration::from_secs)
}

/// Why `error` happened, without the URL of the request.
fn reason(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        return "the request timed out".to_owned();
    }
    let causes: Vec<String> =
        std::iter::successors(std::error::Error::source(error), |cause| cause.source())
            .map(ToString::to_string)
            .collect();
    let kind = if error.is_connect() {
        "cannot connect"
    } else {
        "the request failed"
    };
    match causes.last() {
        Some(cause) => format!("{kind} ({cause})"),
        None => kind.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exporter::LogExporter;
    use crate::otlp::encode::RequestBuilder;
    use crate::record::{LogIdentity, LogKind};
    use crate::test_fixtures::{headers_file, resource};
    use crate::test_support::OtlpReceiver;
    use daemon_config::peppy_config::Severity;
    use std::sync::Arc;
    use std::time::SystemTime;

    /// An encoded request of one record whose body is `line`.
    fn request(line: &str) -> Bytes {
        let (exporter, mut receiver) = LogExporter::channel(None);
        let identity = Arc::new(LogIdentity::new(LogKind::Stack, "/stack_log.log"));
        exporter.export_daemon_line(&identity, SystemTime::now(), Severity::Info, line);
        let (record, _reservation) = receiver.try_recv().unwrap().into_parts();
        let mut builder = RequestBuilder::default();
        builder.push(record);
        Bytes::from(builder.encode(&resource()))
    }

    fn headers(content: &str) -> OtlpHeaders {
        let (_dir, path) = headers_file(content);
        OtlpHeaders::load(&path).unwrap()
    }

    #[tokio::test]
    async fn a_request_is_posted_to_the_logs_path_with_its_headers() {
        let mut receiver = OtlpReceiver::start("127.0.0.1:0").await;
        let endpoint = OtlpEndpoint::parse(&receiver.endpoint()).unwrap();
        let transport = HttpTransport::new(&endpoint).unwrap();

        let response = transport
            .post(
                request("a line"),
                &headers(
                    "{ \"x-api-key\": \"key\", \"x-scope-orgid\": \"robots\", \
                     \"content-type\": \"text/plain\" }",
                ),
            )
            .await
            .unwrap();

        assert_eq!(response.status, 200);
        let received = receiver.next_request().await;
        assert_eq!(received.path, "/v1/logs");
        assert_eq!(received.headers["content-type"], "application/x-protobuf");
        assert_eq!(
            received.headers["user-agent"],
            format!("peppy/{}", daemon_config::consts::PEPPY_VERSION)
        );
        assert_eq!(received.headers["x-api-key"], "key");
        assert_eq!(received.headers["x-scope-orgid"], "robots");
        let bodies: Vec<&str> = received
            .records
            .iter()
            .map(|record| record.body.as_str())
            .collect();
        assert_eq!(bodies, ["a line"]);
    }

    #[tokio::test]
    async fn a_url_with_the_logs_path_is_posted_to_as_written() {
        let mut receiver = OtlpReceiver::start("127.0.0.1:0").await;
        let url = format!("{}/intake/v1/logs", receiver.endpoint());
        let transport = HttpTransport::new(&OtlpEndpoint::parse(&url).unwrap()).unwrap();

        transport
            .post(request("a line"), &OtlpHeaders::default())
            .await
            .unwrap();

        assert_eq!(receiver.next_request().await.path, "/intake/v1/logs");
    }

    #[tokio::test]
    async fn the_status_of_the_answer_is_returned() {
        let receiver = OtlpReceiver::start("127.0.0.1:0").await;
        receiver.answer_next([503]);
        let endpoint = OtlpEndpoint::parse(&receiver.endpoint()).unwrap();
        let transport = HttpTransport::new(&endpoint).unwrap();

        let response = transport
            .post(request("a line"), &OtlpHeaders::default())
            .await
            .unwrap();

        assert_eq!(response.status, 503);
        assert_eq!(response.retry_after, None);
    }

    #[tokio::test]
    async fn a_port_nothing_listens_on_gives_no_answer() {
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let endpoint = OtlpEndpoint::parse(&format!("http://127.0.0.1:{port}")).unwrap();
        let transport = HttpTransport::new(&endpoint).unwrap();

        let TransportError(reason) = transport
            .post(request("a line"), &OtlpHeaders::default())
            .await
            .unwrap_err();

        assert!(reason.starts_with("cannot connect"), "{reason}");
        assert!(!reason.contains("127.0.0.1"), "{reason}");
    }

    #[test]
    fn retry_after_is_read_when_it_is_a_number_of_seconds() {
        let with = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(RETRY_AFTER, value.parse().unwrap());
            retry_after(&headers)
        };
        assert_eq!(with("120"), Some(Duration::from_secs(120)));
        assert_eq!(with(" 5 "), Some(Duration::from_secs(5)));
        assert_eq!(with("Wed, 21 Oct 2026 07:28:00 GMT"), None);
        assert_eq!(retry_after(&HeaderMap::new()), None);
    }
}
