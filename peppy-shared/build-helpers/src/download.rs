//! Downloading files from build scripts.

use std::path::Path;
use std::process::Command;

/// The curl options of every download a build script makes, written as a
/// shell command line takes them, so that a script a build runs in a VM
/// downloads with the same options.
///
/// `--fail` makes an HTTP error a failed download, not an error page saved
/// as the file. `--retry` and `--retry-max-time` retry a transient failure
/// (a timeout, a refused connection, or an HTTP 408, 429, 500, 502, 503 or
/// 504) for up to three minutes, and the wait doubles after each attempt.
/// Release hosts such as GitHub can answer with a gateway timeout for a minute
/// or more, and without the retries one such answer stops the build. A
/// permanent failure, such as an HTTP 404, fails at once.
pub const CURL_DOWNLOAD_OPTIONS: &str =
    "--fail --silent --show-error --location --retry 10 --retry-max-time 180 --retry-connrefused";

/// Downloads `url` to `dest` with curl and [`CURL_DOWNLOAD_OPTIONS`].
///
/// Returns `true` on success. On failure, prints a cargo warning that names
/// `what` and `url`, and returns `false`.
pub fn download_file(url: &str, dest: &Path, what: &str) -> bool {
    crate::run_command(
        Command::new("curl")
            .args(CURL_DOWNLOAD_OPTIONS.split_whitespace())
            .arg(url)
            .arg("--output")
            .arg(dest),
        &format!("download {what} from {url}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;

    const GATEWAY_TIMEOUT: &str =
        "HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    const NOT_FOUND: &str =
        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    const ARCHIVE_BODY: &str = "archive contents";

    fn ok_with_archive_body() -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{ARCHIVE_BODY}",
            ARCHIVE_BODY.len()
        )
    }

    /// An HTTP server on a local port that answers each request with the next
    /// of its responses, one connection per request. When the responses are
    /// spent, it answers with an HTTP 410, which curl does not retry.
    struct ScriptedServer {
        address: SocketAddr,
        stop: Arc<AtomicBool>,
        handle: JoinHandle<Vec<String>>,
    }

    impl ScriptedServer {
        fn start(responses: Vec<String>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
            let address = listener.local_addr().expect("local address");
            let stop = Arc::new(AtomicBool::new(false));
            let server_stop = Arc::clone(&stop);
            let handle = std::thread::spawn(move || {
                let mut responses = responses.into_iter();
                let mut request_lines = Vec::new();
                for stream in listener.incoming() {
                    if server_stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let mut stream = stream.expect("accept a connection");
                    request_lines.push(read_request(&stream));
                    let response = responses.next().unwrap_or_else(|| {
                        "HTTP/1.1 410 Gone\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    });
                    stream
                        .write_all(response.as_bytes())
                        .expect("write a response");
                }
                request_lines
            });
            Self {
                address,
                stop,
                handle,
            }
        }

        fn url(&self) -> String {
            format!("http://{}/artifact.tar.gz", self.address)
        }

        /// Stops the server and returns the request line of each request it
        /// answered, in order.
        fn stop(self) -> Vec<String> {
            self.stop.store(true, Ordering::SeqCst);
            // The server blocks in `accept`: this connection wakes it, and it
            // sees the stop flag before it reads anything.
            TcpStream::connect(self.address).expect("connect to stop the server");
            self.handle.join().expect("server thread")
        }
    }

    /// Reads the request head from `stream` and returns its request line.
    fn read_request(stream: &TcpStream) -> String {
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        reader
            .read_line(&mut request_line)
            .expect("read the request line");
        let mut header = String::new();
        while reader.read_line(&mut header).expect("read a header") > 0 && header != "\r\n" {
            header.clear();
        }
        request_line.trim_end().to_string()
    }

    #[test]
    fn download_writes_the_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let dest = dir.path().join("artifact.tar.gz");
        let server = ScriptedServer::start(vec![ok_with_archive_body()]);

        assert!(download_file(&server.url(), &dest, "test archive"));
        assert_eq!(server.stop(), ["GET /artifact.tar.gz HTTP/1.1"]);
        assert_eq!(std::fs::read_to_string(&dest).expect("read"), ARCHIVE_BODY);
    }

    #[test]
    fn download_retries_a_gateway_timeout() {
        let dir = tempfile::tempdir().expect("temp dir");
        let dest = dir.path().join("artifact.tar.gz");
        let server =
            ScriptedServer::start(vec![GATEWAY_TIMEOUT.to_string(), ok_with_archive_body()]);

        assert!(download_file(&server.url(), &dest, "test archive"));
        assert_eq!(server.stop().len(), 2);
        assert_eq!(std::fs::read_to_string(&dest).expect("read"), ARCHIVE_BODY);
    }

    #[test]
    fn download_does_not_retry_a_missing_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let dest = dir.path().join("artifact.tar.gz");
        let server = ScriptedServer::start(vec![NOT_FOUND.to_string(), ok_with_archive_body()]);

        assert!(!download_file(&server.url(), &dest, "test archive"));
        assert_eq!(server.stop().len(), 1);
    }
}
