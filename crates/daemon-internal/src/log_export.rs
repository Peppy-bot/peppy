//! The log export of one serve generation: the exporter the log writers of
//! the core node hand their lines to, and the worker that posts those lines
//! to the `otlp_endpoint`.

use crate::error::{Error, Result};
use core_node::ExportDiscardLog;
use daemon_config::consts::{PEPPY_VERSION, PeppyDirs};
use daemon_config::peppy_config::{OtlpEndpoint, PeppyConfig};
use log_export::{
    ExportResource, ExportWorker, ExportWorkerConfig, HttpTransport, LogExporter,
    LogRecordReceiver, OtlpHeaders,
};
use std::path::{Path, PathBuf};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// The log export as the config sets it, before its worker runs.
pub(crate) struct LogExport {
    exporter: LogExporter,
    /// What the worker runs on. `None` while `otlp_endpoint` is `null`.
    worker: Option<WorkerSetup>,
}

struct WorkerSetup {
    receiver: LogRecordReceiver,
    transport: HttpTransport,
    endpoint_host: String,
    headers: OtlpHeaders,
    headers_path: PathBuf,
    discard_log: ExportDiscardLog,
}

/// A log export whose worker runs.
pub(crate) struct RunningLogExport {
    shutdown: CancellationToken,
    worker: Option<JoinHandle<()>>,
}

impl LogExport {
    /// The export `config` sets. With `otlp_endpoint: null` the exporter is
    /// disabled and there is no worker. A headers file that cannot be read
    /// is an error that names the file.
    pub(crate) fn from_config(config: &PeppyConfig, peppy_dirs: &PeppyDirs) -> Result<Self> {
        let Some(endpoint) = &config.otlp_endpoint else {
            return Ok(Self {
                exporter: LogExporter::disabled(),
                worker: None,
            });
        };
        let headers_path = peppy_dirs.otlp_headers_path();
        let headers = OtlpHeaders::load(&headers_path).map_err(Error::ExecutionFailed)?;
        let transport = HttpTransport::new(endpoint).map_err(Error::ExecutionFailed)?;
        if let Some(warning) = cleartext_headers_warning(endpoint, &headers, &headers_path) {
            warn!("{warning}");
        }
        let (exporter, receiver) = LogExporter::channel(config.otlp_min_severity);
        let discard_log = ExportDiscardLog::new(peppy_dirs, exporter.clone());
        Ok(Self {
            exporter,
            worker: Some(WorkerSetup {
                receiver,
                transport,
                endpoint_host: endpoint.host().to_owned(),
                headers,
                headers_path,
                discard_log,
            }),
        })
    }

    /// Where the log writers of the core node send their lines.
    pub(crate) fn exporter(&self) -> LogExporter {
        self.exporter.clone()
    }

    /// Starts the worker, which exports as the core node `core_node_name`.
    pub(crate) fn start(self, core_node_name: &str) -> RunningLogExport {
        let shutdown = CancellationToken::new();
        let Some(setup) = self.worker else {
            return RunningLogExport {
                shutdown,
                worker: None,
            };
        };
        info!(
            "Exporting the log files to {} over OTLP/HTTP",
            setup.endpoint_host
        );
        let config = ExportWorkerConfig {
            resource: ExportResource {
                core_node_name: core_node_name.to_owned(),
                host_name: core_node::current_host_name(),
                peppy_version: PEPPY_VERSION.to_owned(),
            },
            endpoint_host: setup.endpoint_host,
            headers: setup.headers,
            headers_path: setup.headers_path,
        };
        let discard_log = setup.discard_log;
        let worker = ExportWorker::new(setup.receiver, setup.transport, config, move |count| {
            discard_log.record(count)
        });
        RunningLogExport {
            worker: Some(tokio::spawn(worker.run(shutdown.clone()))),
            shutdown,
        }
    }
}

impl RunningLogExport {
    /// Stops the export. The worker sends what is queued for up to
    /// [`log_export::SHUTDOWN_FLUSH`], and this returns once it has ended.
    pub(crate) async fn stop(self) {
        self.shutdown.cancel();
        let Some(worker) = self.worker else {
            return;
        };
        if let Err(error) = worker.await {
            warn!("The log export worker ended abnormally: {error}");
        }
    }
}

/// The warning for request headers that cross the network unencrypted: the
/// endpoint is plain `http` on another machine and the headers file holds
/// headers.
fn cleartext_headers_warning(
    endpoint: &OtlpEndpoint,
    headers: &OtlpHeaders,
    headers_path: &Path,
) -> Option<String> {
    (endpoint.is_cleartext_to_another_machine() && !headers.is_empty()).then(|| {
        format!(
            "otlp_endpoint {endpoint} is plain http to another machine, so the request headers \
             of {} cross the network unencrypted; use an https endpoint",
            headers_path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use daemon_config::peppy_config::Severity;
    use log_export::test_support::{OtlpReceiver, ReceivedRecord};
    use log_export::{Iostream, LogIdentity, LogKind};
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    fn config(otlp_endpoint: Option<&str>) -> PeppyConfig {
        PeppyConfig {
            otlp_endpoint: otlp_endpoint.map(|url| OtlpEndpoint::parse(url).unwrap()),
            ..PeppyConfig::default()
        }
    }

    fn dirs_with_headers(content: Option<&str>) -> (tempfile::TempDir, PeppyDirs) {
        let root = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(root.path());
        if let Some(content) = content {
            std::fs::create_dir_all(dirs.conf_dir()).unwrap();
            std::fs::write(dirs.otlp_headers_path(), content).unwrap();
        }
        (root, dirs)
    }

    #[test]
    fn a_null_endpoint_exports_nothing_and_reads_no_headers() {
        // A headers file that does not parse stays unread while export is off.
        let (_root, dirs) = dirs_with_headers(Some("not json5 {"));

        let export = LogExport::from_config(&config(None), &dirs).unwrap();

        assert!(export.worker.is_none());
    }

    #[test]
    fn an_endpoint_turns_the_exporter_on() {
        let (_root, dirs) = dirs_with_headers(Some("{ \"x-api-key\": \"key\" }"));

        let export = LogExport::from_config(&config(Some("http://localhost:4318")), &dirs).unwrap();

        let worker = export.worker.expect("a worker is set up");
        assert_eq!(worker.endpoint_host, "localhost:4318");
        assert!(!worker.headers.is_empty());
    }

    #[test]
    fn a_headers_file_that_does_not_parse_is_an_error_that_names_it() {
        let (_root, dirs) = dirs_with_headers(Some("{ \"x-api-key\": \"s3cr3t\""));

        let error = LogExport::from_config(&config(Some("http://localhost:4318")), &dirs)
            .err()
            .expect("the headers file does not parse")
            .to_string();

        assert!(
            error.starts_with(&dirs.otlp_headers_path().display().to_string()),
            "{error}"
        );
        assert!(!error.contains("s3cr3t"), "{error}");
    }

    #[test]
    fn headers_over_plain_http_to_another_machine_get_a_warning() {
        let (_root, dirs) = dirs_with_headers(Some("{ \"x-scope-orgid\": \"robots\" }"));
        let path = dirs.otlp_headers_path();
        let headers = OtlpHeaders::load(&path).unwrap();
        let warning = |url: &str, headers: &OtlpHeaders| {
            cleartext_headers_warning(&OtlpEndpoint::parse(url).unwrap(), headers, &path)
        };

        assert_eq!(
            warning("http://collector.lan:4318", &headers),
            Some(format!(
                "otlp_endpoint http://collector.lan:4318/ is plain http to another machine, so \
                 the request headers of {} cross the network unencrypted; use an https endpoint",
                path.display()
            ))
        );
        assert_eq!(
            warning("http://collector.lan:4318", &OtlpHeaders::default()),
            None
        );
        assert_eq!(warning("http://localhost:4318", &headers), None);
        assert_eq!(warning("https://collector.lan:4318", &headers), None);
    }

    /// An endpoint that is down while a node prints more than the queue
    /// holds. Once it is back, the export delivers what the queue held, the
    /// `failed` record of an instance that exited meanwhile, and one line of
    /// the stack log with the number of records it discarded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_outage_longer_than_the_queue_ends_with_one_discard_report() {
        // The queue holds 16 MiB, each record its line and 128 bytes: 2016 of
        // these lines fit, and the other 84 are discarded.
        const LINE_BYTES: usize = 8 * 1024;
        const PRINTED: usize = 2100;
        const QUEUED: usize = 2016;
        const FAILED: &str = "Instance 'arm_inst' of node 'openarm_arm:v1' failed: process exit \
                              status: 3";
        let discard_report = format!(
            "Log export discarded {} records while the queue was full or the endpoint refused \
             requests",
            PRINTED - QUEUED
        );

        let mut receiver = OtlpReceiver::start("127.0.0.1:0").await;
        receiver.answer_with(503);
        let (_root, dirs) = dirs_with_headers(None);
        let export = LogExport::from_config(&config(Some(&receiver.endpoint())), &dirs).unwrap();
        let exporter = export.exporter();
        let running = export.start("cn-test");

        let run_log = Arc::new(
            LogIdentity::new(LogKind::Run, "/logs/run/arm_inst.log")
                .with_node("openarm_arm", "v1")
                .with_instance("arm_inst"),
        );
        let line = "x".repeat(LINE_BYTES);
        for _ in 0..PRINTED {
            exporter.export_captured_line(&run_log, SystemTime::now(), Iostream::Stdout, &line);
        }
        let stack_log = Arc::new(
            LogIdentity::new(LogKind::Stack, dirs.stack_log_path())
                .with_node("openarm_arm", "v1")
                .with_instance("arm_inst"),
        );
        exporter.export_daemon_line(&stack_log, SystemTime::now(), Severity::Error, FAILED);

        receiver.answer_with(200);
        let mut delivered: Vec<ReceivedRecord> = Vec::new();
        while !delivered.iter().any(|record| record.body == discard_report) {
            let request = tokio::time::timeout(Duration::from_secs(120), receiver.next_request())
                .await
                .expect("the export resumes");
            if request.answered == 200 {
                delivered.extend(request.records);
            }
        }
        running.stop().await;
        delivered.extend(receiver.take_records());

        let bodies = |log: &str| -> Vec<&str> {
            delivered
                .iter()
                .filter(|record| record.attributes["peppy.log"] == log)
                .map(|record| record.body.as_str())
                .collect()
        };
        assert_eq!(bodies("run").len(), QUEUED);
        assert_eq!(bodies("stack"), [FAILED, discard_report.as_str()]);
        let report = delivered
            .iter()
            .find(|record| record.body == discard_report)
            .expect("checked above");
        assert_eq!(report.severity_text, "WARN");
        assert_eq!(report.resource["service.name"], "peppy");
        let stack_file = std::fs::read_to_string(dirs.stack_log_path()).unwrap();
        assert_eq!(stack_file.matches("Log export discarded").count(), 1);
        assert!(stack_file.contains(&discard_report), "{stack_file}");
    }
}
