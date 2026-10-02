//! Locks and loopback mirrors that the tests of the mirror step share.

use daemon_config::peppy_config::PackagesBaseUrl;
use httpmock::MockServer;

use super::lock::PYPI_PACKAGES_BASE;

/// The text of a lock with one registry package that pins one wheel on PyPI
/// per `(path, size)`, in that order. A `None` size leaves `size` out.
pub(super) fn lock_with_wheels(wheels: &[(&str, Option<u64>)]) -> String {
    let mut lock = String::from(
        "version = 1\n\n[[package]]\nname = \"a\"\nversion = \"1\"\n\
         source = { registry = \"https://pypi.org/simple\" }\nwheels = [\n",
    );
    for (path, size) in wheels {
        let size = size
            .map(|size| format!(", size = {size}"))
            .unwrap_or_default();
        lock.push_str(&format!(
            "    {{ url = \"{PYPI_PACKAGES_BASE}{path}\", hash = \"sha256:00\"{size} }},\n"
        ));
    }
    lock.push_str("]\n");
    lock
}

/// `count` distinct wheel paths.
pub(super) fn wheel_paths(count: usize) -> Vec<String> {
    (0..count).map(|n| format!("aa/bb/x/{n}.whl")).collect()
}

/// The packages base URL of a mock mirror.
pub(super) fn mirror_of(server: &MockServer) -> PackagesBaseUrl {
    PackagesBaseUrl::loopback_http_for_tests(&server.url("/packages/"))
}

/// A loopback mirror that accepts each connection and closes it before it
/// answers, so every request gets no answer at once.
pub(super) async fn closing_mirror() -> PackagesBaseUrl {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = loopback_base(&listener);
    tokio::spawn(async move {
        while let Ok((connection, _)) = listener.accept().await {
            drop(connection);
        }
    });
    base
}

/// A loopback mirror that accepts each connection and never answers on it,
/// so every request waits until it is dropped or times out.
pub(super) async fn silent_mirror() -> PackagesBaseUrl {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = loopback_base(&listener);
    tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((connection, _)) = listener.accept().await {
            open.push(connection);
        }
    });
    base
}

fn loopback_base(listener: &tokio::net::TcpListener) -> PackagesBaseUrl {
    let port = listener.local_addr().unwrap().port();
    PackagesBaseUrl::loopback_http_for_tests(&format!("http://127.0.0.1:{port}/packages/"))
}
