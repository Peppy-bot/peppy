//! What the settings that hold `null` or one string share: how they read
//! the file, and the URL rules of the ones that hold a URL.

use serde::{Deserialize, Deserializer};

/// Deserializes a setting that holds `null` or a string `parse` reads. Every
/// error names the setting and `expected`, the values it takes, as the parse
/// error of the file gives no path. The value stays out of the error: a URL
/// refused for its credentials holds them.
pub(super) fn null_or_string<'de, D, T>(
    deserializer: D,
    setting: &str,
    expected: &str,
    parse: impl FnOnce(&str) -> Result<T, String>,
) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
{
    match Option::<serde_json::Value>::deserialize(deserializer)? {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) => parse(&value).map(Some).map_err(|reason| {
            serde::de::Error::custom(format!(
                "invalid {setting}: it {reason}; expected {expected}"
            ))
        }),
        Some(_) => Err(serde::de::Error::custom(format!(
            "invalid {setting}: it is not a string; expected {expected}"
        ))),
    }
}

/// The host of a URL a setting names, with its port when the URL names one
/// that is not the default of its scheme. The URL holds no credentials, no
/// query and no fragment: `credentials_belong` says where credentials go.
pub(super) fn host_of_plain_url(
    url: &url::Url,
    credentials_belong: &str,
) -> Result<String, String> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!("has credentials, which {credentials_belong}"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("has a query or a fragment".into());
    }
    let Some(host) = url.host_str() else {
        return Err("has no host".into());
    };
    Ok(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}
