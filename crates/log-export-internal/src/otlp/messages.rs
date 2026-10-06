//! The protobuf messages of an OTLP logs export, with the fields the daemon
//! writes and reads. Field numbers are those of `opentelemetry-proto`
//! (`collector/logs/v1/logs_service.proto` and the messages it imports).

#[derive(prost::Message)]
pub struct ExportLogsServiceRequest {
    #[prost(message, repeated, tag = "1")]
    pub resource_logs: Vec<ResourceLogs>,
}

#[derive(prost::Message)]
pub struct ResourceLogs {
    #[prost(message, optional, tag = "1")]
    pub resource: Option<Resource>,
    #[prost(message, repeated, tag = "2")]
    pub scope_logs: Vec<ScopeLogs>,
}

#[derive(prost::Message)]
pub struct Resource {
    #[prost(message, repeated, tag = "1")]
    pub attributes: Vec<KeyValue>,
}

#[derive(prost::Message)]
pub struct ScopeLogs {
    #[prost(message, optional, tag = "1")]
    pub scope: Option<InstrumentationScope>,
    #[prost(message, repeated, tag = "2")]
    pub log_records: Vec<LogRecord>,
}

#[derive(prost::Message)]
pub struct InstrumentationScope {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub version: String,
}

#[derive(prost::Message)]
pub struct LogRecord {
    /// When the line was written, or 0 when only the time it was read is
    /// known.
    #[prost(fixed64, tag = "1")]
    pub time_unix_nano: u64,
    /// A `SeverityNumber`, or 0 for a line with no severity.
    #[prost(int32, tag = "2")]
    pub severity_number: i32,
    #[prost(string, tag = "3")]
    pub severity_text: String,
    #[prost(message, optional, tag = "5")]
    pub body: Option<AnyValue>,
    #[prost(message, repeated, tag = "6")]
    pub attributes: Vec<KeyValue>,
    /// When the daemon wrote or read the line.
    #[prost(fixed64, tag = "11")]
    pub observed_time_unix_nano: u64,
}

#[derive(prost::Message)]
pub struct KeyValue {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(message, optional, tag = "2")]
    pub value: Option<AnyValue>,
}

#[derive(prost::Message)]
pub struct AnyValue {
    #[prost(oneof = "any_value::Value", tags = "1, 2")]
    pub value: Option<any_value::Value>,
}

pub mod any_value {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Value {
        #[prost(string, tag = "1")]
        StringValue(String),
        #[prost(bool, tag = "2")]
        BoolValue(bool),
    }
}

#[derive(prost::Message)]
pub struct ExportLogsServiceResponse {
    #[prost(message, optional, tag = "1")]
    pub partial_success: Option<ExportLogsPartialSuccess>,
}

#[derive(prost::Message)]
pub struct ExportLogsPartialSuccess {
    #[prost(int64, tag = "1")]
    pub rejected_log_records: i64,
    #[prost(string, tag = "2")]
    pub error_message: String,
}
