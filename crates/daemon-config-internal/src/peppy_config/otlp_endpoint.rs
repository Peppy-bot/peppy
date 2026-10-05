//! The `otlp_endpoint` setting: the OTLP/HTTP URL the daemon exports its log
//! files to.

use super::setting::{host_of_plain_url, null_or_string};
use crate::internal::local_host::is_local;
use serde::{Deserializer, Serialize};

/// The path OTLP/HTTP serves log records on, below the base URL of a
/// receiver.
const LOGS_PATH: &str = "/v1/logs";

/// The OTLP/HTTP endpoint of a collector or an intake: an `http` or `https`
/// URL with a host and with no credentials, query or fragment.
///
/// The setting is the base URL of the receiver, and the daemon posts log
/// records to its `/v1/logs`. A URL whose path already ends with `/v1/logs`
/// is the URL the daemon posts to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtlpEndpoint {
    /// The setting, in the normal form the `url` crate gives it.
    url: url::Url,
    logs_url: String,
    /// The host of the URL, with its port when the URL names one that is not
    /// the default of its scheme, for the output of the daemon.
    host: String,
}

impl OtlpEndpoint {
    /// Parses `value` as an OTLP/HTTP endpoint. The error says what is wrong
    /// with the value.
    pub fn parse(value: &str) -> Result<Self, String> {
        let url = url::Url::parse(value).map_err(|e| format!("is not a URL ({e})"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!(
                "starts with {}:, and an OTLP/HTTP URL starts with http:// or https://",
                url.scheme()
            ));
        }
        let host = host_of_plain_url(&url, "go in otlp_headers.json5 as request headers")?;
        let base = url.as_str().trim_end_matches('/');
        let logs_url = if url.path().trim_end_matches('/').ends_with(LOGS_PATH) {
            base.to_owned()
        } else {
            format!("{base}{LOGS_PATH}")
        };
        Ok(Self {
            url,
            logs_url,
            host,
        })
    }

    /// The URL the daemon posts log records to.
    pub fn logs_url(&self) -> &str {
        &self.logs_url
    }

    /// The host of the URL, with its port when it is not the default one.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Whether requests cross the network unencrypted: the URL is plain
    /// `http` and its host is another machine.
    pub fn is_cleartext_to_another_machine(&self) -> bool {
        self.url.scheme() == "http" && !is_local(&self.url)
    }
}

impl std::fmt::Display for OtlpEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.url.as_str())
    }
}

impl Serialize for OtlpEndpoint {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.url.as_str())
    }
}

/// Deserializes the `otlp_endpoint` setting: `null` or an OTLP/HTTP URL.
pub(super) fn deserialize_otlp_endpoint<'de, D>(
    deserializer: D,
) -> Result<Option<OtlpEndpoint>, D::Error>
where
    D: Deserializer<'de>,
{
    null_or_string(
        deserializer,
        "otlp_endpoint",
        "an OTLP/HTTP URL, such as \"http://localhost:4318\", or null",
        OtlpEndpoint::parse,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_base_url_gets_the_logs_path() {
        for (value, logs_url, host) in [
            (
                "http://localhost:4318",
                "http://localhost:4318/v1/logs",
                "localhost:4318",
            ),
            (
                "http://localhost:4318/",
                "http://localhost:4318/v1/logs",
                "localhost:4318",
            ),
            (
                "https://otlp.example.com/otlp",
                "https://otlp.example.com/otlp/v1/logs",
                "otlp.example.com",
            ),
            ("http://v1/logs", "http://v1/logs/v1/logs", "v1"),
            (
                "HTTPS://OTLP.Example.com:443/otlp/",
                "https://otlp.example.com/otlp/v1/logs",
                "otlp.example.com",
            ),
        ] {
            let endpoint = OtlpEndpoint::parse(value).expect(value);
            assert_eq!(endpoint.logs_url(), logs_url);
            assert_eq!(endpoint.host(), host);
        }
    }

    #[test]
    fn a_url_that_ends_with_the_logs_path_is_used_as_written() {
        for value in [
            "https://http-intake.logs.example.com/v1/logs",
            "https://otlp.example.com/otlp/v1/logs",
            "http://localhost:4318/v1/logs/",
        ] {
            let endpoint = OtlpEndpoint::parse(value).expect(value);
            assert_eq!(endpoint.logs_url(), value.trim_end_matches('/'));
            assert_eq!(endpoint.to_string(), value);
        }
    }

    #[test]
    fn each_invalid_url_says_what_is_wrong() {
        for (value, reason) in [
            (
                "localhost:4318",
                "starts with localhost:, and an OTLP/HTTP URL starts with http:// or https://",
            ),
            ("not a url", "is not a URL (relative URL without a base)"),
            (
                "grpc://localhost:4317",
                "starts with grpc:, and an OTLP/HTTP URL starts with http:// or https://",
            ),
            (
                "https://user:key@otlp.example.com",
                "has credentials, which go in otlp_headers.json5 as request headers",
            ),
            (
                "https://otlp.example.com/?key=1",
                "has a query or a fragment",
            ),
            (
                "https://otlp.example.com/#logs",
                "has a query or a fragment",
            ),
        ] {
            assert_eq!(OtlpEndpoint::parse(value).unwrap_err(), reason, "{value}");
        }
    }

    #[test]
    fn plain_http_is_cleartext_only_toward_another_machine() {
        for (value, cleartext) in [
            ("http://localhost:4318", false),
            ("http://127.0.0.1:4318", false),
            ("http://collector.lan:4318", true),
            ("https://collector.lan:4318", false),
        ] {
            let endpoint = OtlpEndpoint::parse(value).expect(value);
            assert_eq!(
                endpoint.is_cleartext_to_another_machine(),
                cleartext,
                "{value}"
            );
        }
    }
}
