//! The `publish_repository` setting: the OCI repository that `peppy node
//! publish` and `peppy stack build --publish` push the `.sif` of container
//! nodes to.

use super::setting::null_or_string;
use core_node_api::encoding::{OciRepository, OciRepositoryError};
use serde::Deserializer;

/// What the setting takes, for every error it gives.
const EXPECTED: &str = "the URL of an OCI repository, such as \
                        \"oci://ghcr.io/acme/peppy-nodes\", or null";

/// Deserializes the `publish_repository` setting: `null` or the URL of an
/// OCI repository.
pub(super) fn deserialize_publish_repository<'de, D>(
    deserializer: D,
) -> Result<Option<OciRepository>, D::Error>
where
    D: Deserializer<'de>,
{
    null_or_string(deserializer, "publish_repository", EXPECTED, |value| {
        OciRepository::parse(value).map_err(|error| reason(&error))
    })
}

/// What is wrong with the value, without the value: a URL refused for its
/// credentials holds them.
fn reason(error: &OciRepositoryError) -> String {
    match error {
        OciRepositoryError::NotAUrl(_) => "is not a URL".to_owned(),
        OciRepositoryError::NotOciScheme(_) => "does not start with oci://".to_owned(),
        OciRepositoryError::NoHost(_) => "names no registry host".to_owned(),
        OciRepositoryError::NoPath(_) => "names no repository path".to_owned(),
        OciRepositoryError::Credentials => {
            "has credentials, which peppy reads from the Docker style auth file".to_owned()
        }
        OciRepositoryError::QueryOrFragment(_) => "has a query or a fragment".to_owned(),
        OciRepositoryError::InvalidPathComponent { component, .. } => format!(
            "has the repository path component `{component}`, which a registry refuses (a \
             component holds lowercase letters and digits, joined by `.`, `_`, `__` or `-`)"
        ),
        OciRepositoryError::TooLong { length, .. } => format!(
            "is {length} characters from the registry host to the end of the path, more than a \
             registry accepts"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Deserialize)]
    struct Holder {
        #[serde(default, deserialize_with = "deserialize_publish_repository")]
        publish_repository: Option<OciRepository>,
    }

    fn read(document: &str) -> Result<Option<OciRepository>, String> {
        serde_json5::from_str::<Holder>(document)
            .map(|holder| holder.publish_repository)
            .map_err(|e| e.to_string())
    }

    #[test]
    fn null_and_absent_publish_nothing() {
        assert_eq!(read("{ publish_repository: null }"), Ok(None));
        assert_eq!(read("{}"), Ok(None));
    }

    #[test]
    fn a_valid_url_is_read_as_an_oci_repository() {
        let repository = read(r#"{ publish_repository: "oci://ghcr.io/acme/peppy-nodes" }"#)
            .expect("valid")
            .expect("set");
        assert_eq!(repository.registry(), "ghcr.io");
        assert_eq!(repository.path(), "acme/peppy-nodes");
    }

    #[test]
    fn invalid_values_name_the_setting_and_the_reason_without_the_value() {
        for (value, reason) in [
            (
                r#""https://ghcr.io/acme/peppy-nodes""#,
                "does not start with oci://",
            ),
            (r#""oci://ghcr.io""#, "names no repository path"),
            (r#""oci://ghcr.io/""#, "names no repository path"),
            (r#""ghcr.io/acme/nodes""#, "is not a URL"),
            (
                r#""oci://ghcr.io/Acme/nodes""#,
                "has the repository path component `Acme`",
            ),
            (
                r#""oci://user:hunter2@ghcr.io/acme/nodes""#,
                "has credentials",
            ),
            (
                r#""oci://ghcr.io/acme/nodes?tag=x""#,
                "has a query or a fragment",
            ),
            ("7", "is not a string"),
        ] {
            let error = read(&format!("{{ publish_repository: {value} }}")).expect_err(value);
            assert!(
                error.contains("invalid publish_repository"),
                "{value}: {error}"
            );
            assert!(error.contains(reason), "{value}: {error}");
            assert!(
                error.contains("oci://ghcr.io/acme/peppy-nodes"),
                "{value}: {error}"
            );
            assert!(!error.contains("hunter2"), "{value}: {error}");
        }
    }
}
