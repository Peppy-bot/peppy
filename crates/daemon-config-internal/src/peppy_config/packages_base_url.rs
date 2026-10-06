//! The `pypi_mirror` setting: the packages base URL of a PyPI mirror.

use super::setting::{host_of_plain_url, null_or_string};
use serde::{Deserializer, Serialize};

/// The path a packages base URL ends with. PyPI keeps its files under
/// `https://files.pythonhosted.org/packages/`, and each mirror under a
/// directory of its own whose name ends the same way (`/packages/` for most,
/// `/pypi-packages/` for SJTU).
const PACKAGES_PATH_SUFFIX: &str = "packages/";

/// The packages base URL of a PyPI mirror: the URL under which the mirror
/// keeps the files of PyPI with the path layout of
/// `https://files.pythonhosted.org/packages/`, so that the path of a file
/// below one is its path below the other.
///
/// [`PackagesBaseUrl::parse`] makes one from the setting: an `https` URL
/// whose path ends with `packages/`, with no credentials, query or fragment.
/// Tests also make one for a plain HTTP server on loopback with
/// `PackagesBaseUrl::loopback_http_for_tests`. The URL is kept in the
/// normalized form the `url` crate gives it, so it always ends with `/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackagesBaseUrl {
    url: String,
    /// The host of the URL, with its port when the URL names one that is not
    /// the default of its scheme, for the build feedback.
    host: String,
}

impl PackagesBaseUrl {
    /// Parses `value` as the packages base URL of a PyPI mirror. The error
    /// says what is wrong with the value.
    pub fn parse(value: &str) -> Result<Self, String> {
        let url = url::Url::parse(value).map_err(|e| format!("is not a URL ({e})"))?;
        if url.scheme() != "https" {
            return Err(format!("uses {}, not https", url.scheme()));
        }
        Self::from_url(url)
    }

    /// A packages base URL on a loopback test server that speaks plain HTTP,
    /// which [`PackagesBaseUrl::parse`] refuses.
    ///
    /// # Panics
    ///
    /// When `value` is not an `http` URL on a loopback address that
    /// [`PackagesBaseUrl::parse`] would accept with `https`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn loopback_http_for_tests(value: &str) -> Self {
        let url = url::Url::parse(value).expect("a test mirror URL");
        assert_eq!(url.scheme(), "http", "a test mirror speaks plain HTTP");
        let on_loopback = match url.host() {
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            _ => false,
        };
        assert!(on_loopback, "a test mirror is on a loopback address: {url}");
        Self::from_url(url).expect("a valid test mirror URL")
    }

    fn from_url(url: url::Url) -> Result<Self, String> {
        let host = host_of_plain_url(&url, "every rewritten uv.lock copies")?;
        if !url.path().ends_with(PACKAGES_PATH_SUFFIX) {
            return Err(format!(
                "has a path that does not end with {PACKAGES_PATH_SUFFIX:?}, so it is not the \
                 packages base URL of a mirror (the index URL of a mirror ends with /simple/)"
            ));
        }
        Ok(Self {
            url: url.into(),
            host,
        })
    }

    /// The URL, which ends with `/`.
    pub fn as_str(&self) -> &str {
        &self.url
    }

    /// The host of the URL, with its port when it is not the default one.
    pub fn host(&self) -> &str {
        &self.host
    }
}

impl std::fmt::Display for PackagesBaseUrl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.url)
    }
}

impl Serialize for PackagesBaseUrl {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.url)
    }
}

/// Deserializes the `pypi_mirror` setting: `null` or the packages base URL
/// of a PyPI mirror.
pub(super) fn deserialize_pypi_mirror<'de, D>(
    deserializer: D,
) -> Result<Option<PackagesBaseUrl>, D::Error>
where
    D: Deserializer<'de>,
{
    null_or_string(
        deserializer,
        "pypi_mirror",
        "the packages base URL of a PyPI mirror, such as \
         \"https://pypi.tuna.tsinghua.edu.cn/packages/\", or null",
        PackagesBaseUrl::parse,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_packages_base_urls_parse() {
        for (value, host) in [
            (
                "https://pypi.tuna.tsinghua.edu.cn/packages/",
                "pypi.tuna.tsinghua.edu.cn",
            ),
            (
                "https://mirror.yandex.ru/pypi/web/packages/",
                "mirror.yandex.ru",
            ),
            (
                "https://mirror.sjtu.edu.cn/pypi-packages/",
                "mirror.sjtu.edu.cn",
            ),
            (
                "https://mirror.internal:8443/pypi/packages/",
                "mirror.internal:8443",
            ),
        ] {
            let mirror = PackagesBaseUrl::parse(value).expect(value);
            assert_eq!(mirror.as_str(), value);
            assert_eq!(mirror.host(), host);
        }
    }

    #[test]
    fn the_url_is_kept_in_normal_form() {
        let mirror = PackagesBaseUrl::parse("HTTPS://PyPI.Tuna.Tsinghua.edu.cn:443/packages/")
            .expect("a valid mirror");
        assert_eq!(
            mirror.as_str(),
            "https://pypi.tuna.tsinghua.edu.cn/packages/"
        );
        assert_eq!(mirror.host(), "pypi.tuna.tsinghua.edu.cn");
    }

    #[test]
    fn values_that_are_not_a_packages_base_url_fail() {
        for (value, reason) in [
            ("http://pypi.tuna.tsinghua.edu.cn/packages/", "not https"),
            ("ftp://mirror.example/packages/", "not https"),
            (
                "https://pypi.tuna.tsinghua.edu.cn/simple/",
                "does not end with",
            ),
            (
                "https://pypi.tuna.tsinghua.edu.cn/packages",
                "does not end with",
            ),
            ("https://pypi.tuna.tsinghua.edu.cn/", "does not end with"),
            (
                "https://user:secret@mirror.example/packages/",
                "credentials",
            ),
            ("https://token@mirror.example/packages/", "credentials"),
            (
                "https://mirror.example/packages/?token=1",
                "query or a fragment",
            ),
            (
                "https://mirror.example/packages/#top",
                "query or a fragment",
            ),
            ("pypi.tuna.tsinghua.edu.cn/packages/", "not a URL"),
            ("", "not a URL"),
        ] {
            let error = PackagesBaseUrl::parse(value).expect_err(value);
            assert!(error.contains(reason), "{value}: {error}");
        }
    }

    #[test]
    fn serializes_as_its_url() {
        let mirror = PackagesBaseUrl::parse("https://pypi.osso.nl/packages/").unwrap();
        assert_eq!(
            serde_json::to_value(&mirror).unwrap(),
            serde_json::json!("https://pypi.osso.nl/packages/")
        );
        assert_eq!(mirror.to_string(), "https://pypi.osso.nl/packages/");
    }

    #[test]
    fn loopback_test_mirrors_speak_plain_http() {
        let mirror = PackagesBaseUrl::loopback_http_for_tests("http://127.0.0.1:4321/packages/");
        assert_eq!(mirror.as_str(), "http://127.0.0.1:4321/packages/");
        assert_eq!(mirror.host(), "127.0.0.1:4321");

        let mirror = PackagesBaseUrl::loopback_http_for_tests("http://[::1]:4321/packages/");
        assert_eq!(mirror.host(), "[::1]:4321");
    }
}
