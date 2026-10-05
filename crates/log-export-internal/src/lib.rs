#![forbid(unsafe_code)]
//! Export of the daemon's log lines as OpenTelemetry log records.
//!
//! A log writer hands each line it writes to a [`LogExporter`], with the
//! [`LogIdentity`] of its log file. The exporter turns the line into a
//! [`LogRecord`] and queues it for the [`LogRecordReceiver`]. The queue is
//! bounded: a record that arrives while it is full is discarded and counted.
//!
//! An [`ExportWorker`] takes the records off the queue and posts them to an
//! OTLP/HTTP endpoint through a [`Transport`], with the request headers of
//! an [`OtlpHeaders`] file.

mod exporter;
mod headers;
mod level;
mod otlp;
mod record;
#[cfg(feature = "test-support")]
pub mod test_support;
mod transport;
mod worker;

pub use exporter::{LogExporter, LogRecordReceiver};
pub use headers::OtlpHeaders;
pub use otlp::encode::ExportResource;
pub use record::{Iostream, LineOrigin, LogIdentity, LogKind, LogRecord, StackAction};
pub use transport::HttpTransport;
pub use worker::{ExportWorker, ExportWorkerConfig, SHUTDOWN_FLUSH};

/// What the tests of this crate share.
#[cfg(test)]
mod test_fixtures {
    use crate::otlp::encode::ExportResource;
    use std::path::PathBuf;

    /// The resource of a test daemon.
    pub(crate) fn resource() -> ExportResource {
        ExportResource {
            core_node_name: "cn-quiet-otter".to_owned(),
            host_name: "robot-7".to_owned(),
            peppy_version: "1.2.3".to_owned(),
        }
    }

    /// A headers file holding `content`, and the directory that keeps it.
    pub(crate) fn headers_file(content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("otlp_headers.json5");
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }
}
