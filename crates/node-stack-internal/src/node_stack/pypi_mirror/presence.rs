//! Which PyPI files a mirror has: one `HEAD` request per file.

use std::collections::HashSet;
use std::time::Duration;

use daemon_config::peppy_config::PackagesBaseUrl;
use futures::StreamExt;
use reqwest::StatusCode;
use reqwest::header::CONTENT_LENGTH;

use super::lock::{PYPI_FILES_HOST, PypiFile};

/// Requests in flight at once. A large lock pins several hundred files (779
/// for openarm_ai_brain_vla, which a mirror near the host answers in about
/// ten seconds), while few enough requests stay open that a mirror does not
/// take the check for abuse.
const CONCURRENT_REQUESTS: usize = 8;

/// The longest one request may take, redirects included. A file whose
/// request takes longer stays on PyPI.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The most redirects one request follows. Some mirrors answer with a
/// redirect to their object storage (SJTU does), so the check follows
/// redirects, and the limit ends a redirect loop.
const MAX_REDIRECTS: usize = 10;

/// Requests in a row that get no answer (no connection, a timeout, a
/// connection closed before the answer) after which the check stops. As
/// many as the requests in flight: a mirror that answers nothing stops the
/// check after one round of timeouts, not after one timeout per file.
pub(super) const UNANSWERED_REQUESTS_TO_STOP: usize = CONCURRENT_REQUESTS;

/// The HTTP client of the check.
pub(super) fn client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
        .user_agent(format!("peppy/{}", daemon_config::consts::PEPPY_VERSION))
        .build()
}

/// The mirror answered none of [`UNANSWERED_REQUESTS_TO_STOP`] requests in a
/// row, so the check stopped.
#[derive(Debug)]
pub(super) struct MirrorUnreachable {
    pub unanswered: usize,
    pub last_error: reqwest::Error,
}

impl std::fmt::Display for MirrorUnreachable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} requests in a row got no answer, the last one with: {}",
            self.unanswered,
            error_with_causes(&self.last_error)
        )
    }
}

/// What the mirror answered for one file.
enum Answer {
    /// `200`, with the size of the lock when the lock records one.
    Present,
    /// Any other answer.
    Absent,
    /// No answer at all.
    None(reqwest::Error),
}

/// The files of `files` that `mirror` has: the ones it answers `200` for,
/// with a `Content-Length` equal to the size the lock records, if any, from
/// a host other than PyPI at the end of the redirects.
/// Calls `on_checked` with the number of files checked so far after each
/// answer.
pub(super) async fn files_on_mirror(
    client: &reqwest::Client,
    mirror: &PackagesBaseUrl,
    files: &[PypiFile],
    mut on_checked: impl FnMut(usize),
) -> Result<HashSet<PypiFile>, MirrorUnreachable> {
    // Each request owns what it needs: a stream of futures that borrow from
    // a closure argument does not prove `Send` to the spawned build task.
    let requests = files.iter().cloned().map(|file| {
        let url = file.url_on(mirror);
        ask(client.clone(), url, file)
    });
    let mut answers = futures::stream::iter(requests).buffer_unordered(CONCURRENT_REQUESTS);
    let mut on_mirror = HashSet::new();
    let mut unanswered_in_a_row = 0;
    let mut checked = 0;
    while let Some((file, answer)) = answers.next().await {
        checked += 1;
        match answer {
            Answer::Present => {
                on_mirror.insert(file);
                unanswered_in_a_row = 0;
            }
            Answer::Absent => unanswered_in_a_row = 0,
            Answer::None(last_error) => {
                unanswered_in_a_row += 1;
                if unanswered_in_a_row >= UNANSWERED_REQUESTS_TO_STOP {
                    return Err(MirrorUnreachable {
                        unanswered: unanswered_in_a_row,
                        last_error,
                    });
                }
            }
        }
        on_checked(checked);
    }
    Ok(on_mirror)
}

/// Asks the mirror for `file` at `url`, its URL on the mirror, and returns
/// the file with the answer.
async fn ask(client: reqwest::Client, url: String, file: PypiFile) -> (PypiFile, Answer) {
    let answer = answer_for(&client, url, file.size()).await;
    (file, answer)
}

async fn answer_for(client: &reqwest::Client, url: String, size: Option<u64>) -> Answer {
    let response = match client.head(url).send().await {
        Ok(response) => response,
        Err(error) => return Answer::None(error),
    };
    if response.status() != StatusCode::OK || served_by_pypi(response.url()) {
        return Answer::Absent;
    }
    let Some(size) = size else {
        return Answer::Present;
    };
    // `Response::content_length` is the length of the body, and the answer
    // to a `HEAD` has none: the header holds the size of the file.
    let content_length = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if content_length == Some(size) {
        Answer::Present
    } else {
        Answer::Absent
    }
}

/// Whether the last answer of a request came from PyPI. Some mirrors
/// redirect every file to PyPI (XJTU does): a file the mirror sends there
/// downloads from PyPI after more hops, so it keeps its PyPI URL.
fn served_by_pypi(final_url: &reqwest::Url) -> bool {
    final_url.host_str() == Some(PYPI_FILES_HOST)
}

/// `error` followed by each of its causes, as the message of a `reqwest`
/// error alone names the request, not why it failed.
fn error_with_causes(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut cause = error.source();
    while let Some(error) = cause {
        message.push_str(": ");
        message.push_str(&error.to_string());
        cause = error.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::Method::HEAD;
    use httpmock::MockServer;

    use crate::node_stack::pypi_mirror::lock::UvLock;
    use crate::node_stack::pypi_mirror::test_support::{
        closing_mirror, lock_with_wheels, mirror_of, wheel_paths,
    };

    /// The PyPI files of a lock that pins one wheel per `(path, size)`, in
    /// that order.
    fn pypi_files(wheels: &[(&str, Option<u64>)]) -> Vec<PypiFile> {
        UvLock::parse(&lock_with_wheels(wheels))
            .unwrap()
            .pypi_files()
            .to_vec()
    }

    /// The PyPI files of `count` distinct wheels of 40 bytes.
    fn many_files(count: usize) -> Vec<PypiFile> {
        let paths = wheel_paths(count);
        let wheels: Vec<(&str, Option<u64>)> =
            paths.iter().map(|path| (path.as_str(), Some(40))).collect();
        pypi_files(&wheels)
    }

    async fn check(
        mirror: &PackagesBaseUrl,
        files: &[PypiFile],
    ) -> Result<HashSet<PypiFile>, MirrorUnreachable> {
        files_on_mirror(&client().unwrap(), mirror, files, |_| {}).await
    }

    #[tokio::test]
    async fn a_200_with_the_size_of_the_lock_is_present_and_anything_else_is_not() {
        let server = MockServer::start_async().await;
        for (path, status, length) in [
            ("aa/bb/x/right.whl", 200, 40),
            ("aa/bb/x/wrong-length.whl", 200, 39),
            ("aa/bb/x/missing.whl", 404, 0),
            ("aa/bb/x/broken.whl", 500, 40),
        ] {
            server
                .mock_async(|when, then| {
                    when.method(HEAD).path(format!("/packages/{path}"));
                    then.status(status)
                        .header("content-length", length.to_string());
                })
                .await;
        }
        let files = pypi_files(&[
            ("aa/bb/x/right.whl", Some(40)),
            ("aa/bb/x/wrong-length.whl", Some(40)),
            ("aa/bb/x/missing.whl", Some(40)),
            ("aa/bb/x/broken.whl", Some(40)),
        ]);

        let on_mirror = check(&mirror_of(&server), &files).await.unwrap();

        assert_eq!(on_mirror, HashSet::from([files[0].clone()]));
    }

    #[tokio::test]
    async fn a_file_with_no_size_in_the_lock_is_present_on_200() {
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(HEAD).path("/packages/aa/bb/x/a.tar.gz");
                then.status(200).header("content-length", "12345");
            })
            .await;
        let files = pypi_files(&[("aa/bb/x/a.tar.gz", None)]);

        let on_mirror = check(&mirror_of(&server), &files).await.unwrap();

        assert_eq!(on_mirror, HashSet::from([files[0].clone()]));
    }

    #[tokio::test]
    async fn a_200_without_a_content_length_is_not_present_when_the_lock_has_a_size() {
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(HEAD).path("/packages/aa/bb/x/a.whl");
                then.status(200);
            })
            .await;
        let files = pypi_files(&[("aa/bb/x/a.whl", Some(40))]);

        let on_mirror = check(&mirror_of(&server), &files).await.unwrap();

        assert!(on_mirror.is_empty());
    }

    #[tokio::test]
    async fn a_redirect_is_followed_and_its_target_decides() {
        let server = MockServer::start_async().await;
        for (from, to) in [
            ("aa/bb/x/kept.whl", "/storage/kept.whl"),
            ("aa/bb/x/gone.whl", "/storage/gone.whl"),
        ] {
            let location = server.url(to);
            server
                .mock_async(|when, then| {
                    when.method(HEAD).path(format!("/packages/{from}"));
                    then.status(301).header("location", location);
                })
                .await;
        }
        server
            .mock_async(|when, then| {
                when.method(HEAD).path("/storage/kept.whl");
                then.status(200).header("content-length", "40");
            })
            .await;
        server
            .mock_async(|when, then| {
                when.method(HEAD).path("/storage/gone.whl");
                then.status(404);
            })
            .await;
        let files = pypi_files(&[
            ("aa/bb/x/kept.whl", Some(40)),
            ("aa/bb/x/gone.whl", Some(40)),
        ]);

        let on_mirror = check(&mirror_of(&server), &files).await.unwrap();

        assert_eq!(on_mirror, HashSet::from([files[0].clone()]));
    }

    #[tokio::test]
    async fn a_closed_connection_is_not_present() {
        let mirror = closing_mirror().await;
        let files = pypi_files(&[("aa/bb/x/a.whl", Some(40))]);

        let on_mirror = check(&mirror, &files).await.unwrap();

        assert!(on_mirror.is_empty());
    }

    #[tokio::test]
    async fn a_mirror_that_answers_no_request_stops_the_check() {
        let mirror = closing_mirror().await;
        let files = many_files(UNANSWERED_REQUESTS_TO_STOP * 4);
        let mut checked = Vec::new();

        let error = files_on_mirror(&client().unwrap(), &mirror, &files, |n| checked.push(n))
            .await
            .expect_err("a mirror that answers nothing stops the check");

        assert_eq!(error.unanswered, UNANSWERED_REQUESTS_TO_STOP);
        assert_eq!(
            checked,
            (1..UNANSWERED_REQUESTS_TO_STOP).collect::<Vec<_>>(),
            "the check stops at the request that reaches the limit"
        );
        assert!(
            error
                .to_string()
                .contains("requests in a row got no answer"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn answers_reset_the_unanswered_count() {
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(HEAD);
                then.status(404);
            })
            .await;
        let files = many_files(UNANSWERED_REQUESTS_TO_STOP * 4);
        let mut checked = 0;

        let on_mirror = files_on_mirror(&client().unwrap(), &mirror_of(&server), &files, |n| {
            checked = n
        })
        .await
        .expect("a mirror that answers 404 is reachable");

        assert!(on_mirror.is_empty());
        assert_eq!(checked, files.len());
    }

    #[tokio::test]
    async fn requests_name_peppy_as_their_user_agent() {
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(HEAD).header_prefix("user-agent", "peppy/");
                then.status(200);
            })
            .await;
        let files = pypi_files(&[("aa/bb/x/a.whl", None)]);

        check(&mirror_of(&server), &files).await.unwrap();

        mock.assert_async().await;
    }

    #[test]
    fn an_answer_from_pypi_at_the_end_of_the_redirects_is_not_a_mirror_hit() {
        let url = |url: &str| reqwest::Url::parse(url).unwrap();
        assert!(served_by_pypi(&url(
            "https://files.pythonhosted.org/packages/76/c6/x/idna-3.10-py3-none-any.whl"
        )));
        assert!(!served_by_pypi(&url(
            "https://pypi.tuna.tsinghua.edu.cn/packages/76/c6/x/idna-3.10-py3-none-any.whl"
        )));
        assert!(!served_by_pypi(&url(
            "https://s3.jcloud.sjtu.edu.cn/pypi/packages/76/c6/x/idna-3.10-py3-none-any.whl"
        )));
    }

    #[test]
    fn an_error_names_its_causes() {
        #[derive(Debug)]
        struct RequestFailed(std::io::Error);
        impl std::fmt::Display for RequestFailed {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("error sending request")
            }
        }
        impl std::error::Error for RequestFailed {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let error = RequestFailed(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "connection refused",
        ));

        assert_eq!(
            error_with_causes(&error),
            "error sending request: connection refused"
        );
    }
}
