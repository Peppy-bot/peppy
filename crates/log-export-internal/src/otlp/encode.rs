//! How records become one OTLP export request: one `ResourceLogs` for each
//! service the records belong to.

use super::messages::{
    AnyValue, ExportLogsServiceRequest, InstrumentationScope, KeyValue, LogRecord as OtlpLogRecord,
    Resource, ResourceLogs, ScopeLogs, any_value,
};
use crate::record::{LineOrigin, LogKind, LogRecord};
use prost::Message;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// The `service.name` of a record whose line belongs to no node.
const DAEMON_SERVICE_NAME: &str = "peppy";
/// The most bytes that precede a record in an encoded request: its tag and
/// its length.
const RECORD_FRAMING_BYTES: usize = 4;
/// The name of the instrumentation scope of every record.
const SCOPE_NAME: &str = "peppy";

/// What every record a daemon exports has in common: the machine and the
/// daemon they come from.
pub struct ExportResource {
    pub core_node_name: String,
    pub host_name: String,
    pub peppy_version: String,
}

/// The records of one export request, grouped by the service they belong to.
#[derive(Default)]
pub(crate) struct RequestBuilder {
    by_service: BTreeMap<String, Vec<OtlpLogRecord>>,
    records: u64,
    encoded_bytes: usize,
}

impl RequestBuilder {
    pub(crate) fn push(&mut self, record: LogRecord) {
        let service = service_name(&record).to_owned();
        let record = log_record(record);
        self.encoded_bytes += record.encoded_len() + RECORD_FRAMING_BYTES;
        self.records += 1;
        self.by_service.entry(service).or_default().push(record);
    }

    pub(crate) fn records(&self) -> u64 {
        self.records
    }

    /// The size of the records in the encoded request.
    pub(crate) fn encoded_bytes(&self) -> usize {
        self.encoded_bytes
    }

    /// The encoded request.
    pub(crate) fn encode(self, resource: &ExportResource) -> Vec<u8> {
        let resource_logs = self
            .by_service
            .into_iter()
            .map(|(service, log_records)| ResourceLogs {
                resource: Some(Resource {
                    attributes: resource_attributes(resource, &service),
                }),
                scope_logs: vec![ScopeLogs {
                    scope: Some(InstrumentationScope {
                        name: SCOPE_NAME.to_owned(),
                        version: resource.peppy_version.clone(),
                    }),
                    log_records,
                }],
            })
            .collect();
        ExportLogsServiceRequest { resource_logs }.encode_to_vec()
    }
}

/// The node a record's line belongs to, or the daemon.
fn service_name(record: &LogRecord) -> &str {
    record
        .identity
        .node
        .as_ref()
        .map_or(DAEMON_SERVICE_NAME, |node| node.name.as_str())
}

fn resource_attributes(resource: &ExportResource, service: &str) -> Vec<KeyValue> {
    vec![
        string("service.name", service),
        string("service.namespace", &resource.core_node_name),
        string("host.name", &resource.host_name),
        string("peppy.core_node.name", &resource.core_node_name),
        string("peppy.version", &resource.peppy_version),
    ]
}

fn log_record(record: LogRecord) -> OtlpLogRecord {
    let identity = &record.identity;
    let stack_action = match identity.kind {
        LogKind::Launch { action } => Some(action.name()),
        LogKind::Add | LogKind::Build | LogKind::Run | LogKind::Stack => None,
    };
    let iostream = match record.origin {
        LineOrigin::Captured(stream) => Some(stream.name()),
        LineOrigin::Daemon => None,
    };
    let node = identity.node.as_ref();
    let file_path = identity.file_path.to_string_lossy();
    let attributes = [
        Some(string("peppy.log", identity.kind.name())),
        stack_action.map(|action| string("peppy.stack.action", action)),
        node.map(|node| string("peppy.node.name", &node.name)),
        node.map(|node| string("peppy.node.tag", &node.tag)),
        identity
            .instance_id
            .as_deref()
            .map(|id| string("peppy.instance.id", id)),
        identity
            .launch_id
            .as_deref()
            .map(|id| string("peppy.launch.id", id)),
        iostream.map(|stream| string("log.iostream", stream)),
        Some(string("log.file.path", &file_path)),
        record.truncated.then(|| KeyValue {
            key: "peppy.body.truncated".to_owned(),
            value: Some(AnyValue {
                value: Some(any_value::Value::BoolValue(true)),
            }),
        }),
    ]
    .into_iter()
    .flatten()
    .collect();

    let observed = unix_nanos(record.time);
    OtlpLogRecord {
        // A captured line was printed at a time only its writer knows.
        time_unix_nano: match record.origin {
            LineOrigin::Daemon => observed,
            LineOrigin::Captured(_) => 0,
        },
        observed_time_unix_nano: observed,
        severity_number: record.severity.map_or(0, |severity| severity.number()),
        severity_text: record.severity_text.unwrap_or_default(),
        body: Some(AnyValue {
            value: Some(any_value::Value::StringValue(record.body)),
        }),
        attributes,
    }
}

fn string(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.to_owned())),
        }),
    }
}

fn unix_nanos(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| u64::try_from(since.as_nanos()).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exporter::LogExporter;
    use crate::record::{Iostream, LogIdentity, StackAction};
    use crate::test_fixtures::resource;
    use daemon_config::peppy_config::Severity;
    use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest as OfficialRequest;
    use opentelemetry_proto::tonic::common::v1::any_value::Value as OfficialValue;
    use opentelemetry_proto::tonic::common::v1::{
        AnyValue as OfficialAnyValue, KeyValue as OfficialKeyValue,
    };
    use std::sync::Arc;
    use std::time::Duration;

    /// The records the exporter makes of a run log line, a launch log line
    /// and a stack log line.
    fn records() -> Vec<LogRecord> {
        let (exporter, mut receiver) = LogExporter::channel(None);
        let time = UNIX_EPOCH + Duration::from_nanos(1_700_000_000_123_456_789);
        let run = Arc::new(
            LogIdentity::new(LogKind::Run, "/logs/run/arm.log")
                .with_node("openarm_arm", "v1")
                .with_instance("arm_left")
                .with_launch(Some("launch-7")),
        );
        let launch = Arc::new(
            LogIdentity::new(
                LogKind::Launch {
                    action: StackAction::Join,
                },
                "/logs/launch/l.log",
            )
            .with_launch(Some("launch-7")),
        );
        let stack = Arc::new(
            LogIdentity::new(LogKind::Stack, "/stack_log.log")
                .with_node("openarm_arm", "v1")
                .with_instance("arm_left"),
        );
        exporter.export_captured_line(
            &run,
            time,
            Iostream::Stderr,
            &format!("WARN joint 3 is hot {}", "x".repeat(20_000)),
        );
        exporter.export_daemon_line(&launch, time, Severity::Info, "Starting arm_left");
        exporter.export_daemon_line(&stack, time, Severity::Error, "Instance failed");
        std::iter::from_fn(|| receiver.try_recv())
            .map(|queued| queued.into_parts().0)
            .collect()
    }

    fn official_text(value: Option<OfficialAnyValue>) -> String {
        match value.and_then(|value| value.value) {
            Some(OfficialValue::StringValue(text)) => text,
            Some(OfficialValue::BoolValue(flag)) => flag.to_string(),
            other => panic!("an unexpected value: {other:?}"),
        }
    }

    fn official_attributes(attributes: Vec<OfficialKeyValue>) -> Vec<(String, String)> {
        attributes
            .into_iter()
            .map(|attribute| (attribute.key, official_text(attribute.value)))
            .collect()
    }

    fn pairs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn the_official_otlp_messages_read_an_encoded_request() {
        let mut builder = RequestBuilder::default();
        for record in records() {
            builder.push(record);
        }
        assert_eq!(builder.records(), 3);

        let request = OfficialRequest::decode(builder.encode(&resource()).as_slice())
            .expect("the official messages decode the request");

        let [node, daemon]: [_; 2] = request
            .resource_logs
            .try_into()
            .expect("one resource for the node and one for the daemon");
        assert_eq!(
            official_attributes(node.resource.unwrap().attributes),
            pairs(&[
                ("service.name", "openarm_arm"),
                ("service.namespace", "cn-quiet-otter"),
                ("host.name", "robot-7"),
                ("peppy.core_node.name", "cn-quiet-otter"),
                ("peppy.version", "1.2.3"),
            ])
        );
        assert_eq!(
            official_attributes(daemon.resource.unwrap().attributes)[0],
            ("service.name".to_owned(), "peppy".to_owned())
        );

        let [node_scope]: [_; 1] = node.scope_logs.try_into().expect("one scope");
        let scope = node_scope.scope.unwrap();
        assert_eq!(
            (scope.name.as_str(), scope.version.as_str()),
            ("peppy", "1.2.3")
        );
        let [run, stack]: [_; 2] = node_scope.log_records.try_into().expect("two records");

        assert_eq!(run.time_unix_nano, 0);
        assert_eq!(run.observed_time_unix_nano, 1_700_000_000_123_456_789);
        assert_eq!(run.severity_number, 13);
        assert_eq!(run.severity_text, "WARN");
        let body = official_text(run.body);
        assert!(body.starts_with("WARN joint 3 is hot xxx"), "{body}");
        assert_eq!(body.len(), 16 * 1024);
        assert_eq!(
            official_attributes(run.attributes),
            pairs(&[
                ("peppy.log", "run"),
                ("peppy.node.name", "openarm_arm"),
                ("peppy.node.tag", "v1"),
                ("peppy.instance.id", "arm_left"),
                ("peppy.launch.id", "launch-7"),
                ("log.iostream", "stderr"),
                ("log.file.path", "/logs/run/arm.log"),
                ("peppy.body.truncated", "true"),
            ])
        );

        assert_eq!(stack.time_unix_nano, 1_700_000_000_123_456_789);
        assert_eq!(stack.observed_time_unix_nano, 1_700_000_000_123_456_789);
        assert_eq!(stack.severity_number, 17);
        assert_eq!(stack.severity_text, "ERROR");
        assert_eq!(official_text(stack.body), "Instance failed");
        assert_eq!(
            official_attributes(stack.attributes),
            pairs(&[
                ("peppy.log", "stack"),
                ("peppy.node.name", "openarm_arm"),
                ("peppy.node.tag", "v1"),
                ("peppy.instance.id", "arm_left"),
                ("log.file.path", "/stack_log.log"),
            ])
        );

        let [daemon_scope]: [_; 1] = daemon.scope_logs.try_into().expect("one scope");
        let [launch]: [_; 1] = daemon_scope.log_records.try_into().expect("one record");
        assert_eq!(launch.severity_number, 9);
        assert_eq!(official_text(launch.body), "Starting arm_left");
        assert_eq!(
            official_attributes(launch.attributes),
            pairs(&[
                ("peppy.log", "launch"),
                ("peppy.stack.action", "join"),
                ("peppy.launch.id", "launch-7"),
                ("log.file.path", "/logs/launch/l.log"),
            ])
        );
    }

    #[test]
    fn the_counted_bytes_are_close_to_the_encoded_request() {
        let (exporter, mut receiver) = LogExporter::channel(None);
        let identity = Arc::new(LogIdentity::new(LogKind::Build, "/logs/build/b.log"));
        let mut builder = RequestBuilder::default();
        for index in 0..100 {
            exporter.export_captured_line(
                &identity,
                UNIX_EPOCH,
                Iostream::Stdout,
                &format!("Compiling crate {index}"),
            );
            builder.push(receiver.try_recv().unwrap().into_parts().0);
        }

        let counted = builder.encoded_bytes();
        let encoded = builder.encode(&resource()).len();

        assert!(
            counted.abs_diff(encoded) < 512,
            "{counted} bytes counted, {encoded} encoded"
        );
    }
}
