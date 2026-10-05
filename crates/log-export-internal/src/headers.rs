//! The request headers of the export, read from the headers file.

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::path::Path;

/// The headers the daemon adds to each export request, such as an API key.
/// Their values are secrets: `Debug` shows the names alone, and no error
/// holds a value.
#[derive(Default, PartialEq, Eq)]
pub struct OtlpHeaders {
    map: HeaderMap,
}

impl OtlpHeaders {
    /// Reads the headers file at `path`: a JSON5 object of header name to
    /// value. A missing file holds no headers. A file that exists is set to
    /// owner-only. The error names the file and what is wrong with it.
    pub fn load(path: &Path) -> Result<Self, String> {
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        };
        daemon_config::fs_perms::restrict_file(path).map_err(|error| {
            format!(
                "cannot make {} readable by its owner alone: {error}",
                path.display()
            )
        })?;
        Self::parse(&content).map_err(|reason| format!("{}: {reason}", path.display()))
    }

    fn parse(content: &str) -> Result<Self, String> {
        const EXPECTED: &str = "expected an object of header names to values, such as \
                                { \"x-api-key\": \"<key>\" }";
        // The parser's own message quotes the file, so only the position of
        // the fault is taken from it.
        let Entries(entries) = serde_json5::from_str(content).map_err(|error| match error {
            serde_json5::Error::Message {
                location: Some(location),
                ..
            } => format!(
                "line {}, column {} does not parse; {EXPECTED}",
                location.line, location.column
            ),
            serde_json5::Error::Message { location: None, .. } => {
                format!("the file does not parse; {EXPECTED}")
            }
        })?;
        // A name that is no header name stays out of the error: it may be a
        // secret written in the wrong place.
        entries
            .into_iter()
            .zip(1..)
            .try_fold(HeaderMap::new(), |mut map, ((name, value), position)| {
                let header = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                    format!(
                        "entry {position} has a name that is no header name; a header name \
                         holds letters, digits and `-`, with no space or `:`"
                    )
                })?;
                let mut value = HeaderValue::from_str(&value).map_err(|_| {
                    format!(
                        "the value of {header} holds a character no header value can carry, \
                         such as a newline or a control character"
                    )
                })?;
                value.set_sensitive(true);
                match map.insert(header.clone(), value) {
                    Some(_) => Err(format!("two entries name the header {header}")),
                    None => Ok(map),
                }
            })
            .map(|map| Self { map })
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub(crate) fn as_map(&self) -> &HeaderMap {
        &self.map
    }
}

/// The entries of the headers file in the order written, with every entry
/// kept: a name written twice is an error, whatever its values.
struct Entries(Vec<(String, String)>);

impl<'de> serde::Deserialize<'de> for Entries {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Entries;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an object of header names to values")
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Entries, M::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry::<String, String>()? {
                    entries.push(entry);
                }
                Ok(Entries(entries))
            }
        }

        deserializer.deserialize_map(Visitor)
    }
}

impl std::fmt::Debug for OtlpHeaders {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_set()
            .entries(self.map.keys().map(HeaderName::as_str))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::headers_file;

    const SECRET: &str = "s3cr3t-key-value";

    #[test]
    fn a_missing_file_holds_no_headers() {
        let dir = tempfile::tempdir().unwrap();
        let headers = OtlpHeaders::load(&dir.path().join("otlp_headers.json5")).unwrap();
        assert!(headers.is_empty());
    }

    #[test]
    fn a_file_gives_its_headers_and_becomes_owner_only() {
        let (_dir, path) = headers_file(&format!(
            "{{\n  // the intake key\n  \"x-api-key\": \"{SECRET}\",\n  'X-Scope-OrgID': 'robots',\n}}\n"
        ));
        let headers = OtlpHeaders::load(&path).unwrap();

        let mut sent: Vec<(&str, &str)> = headers
            .as_map()
            .iter()
            .map(|(name, value)| (name.as_str(), value.to_str().unwrap()))
            .collect();
        sent.sort_unstable();
        assert_eq!(sent, [("x-api-key", SECRET), ("x-scope-orgid", "robots")]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn debug_shows_the_names_alone() {
        let (_dir, path) = headers_file(&format!("{{ \"x-api-key\": \"{SECRET}\" }}"));
        let shown = format!("{:?}", OtlpHeaders::load(&path).unwrap());
        assert_eq!(shown, "{\"x-api-key\"}");
    }

    #[test]
    fn each_invalid_file_says_what_is_wrong_without_a_value() {
        for (content, reason) in [
            (
                format!("{{ \"x-api-key\": \"{SECRET}\""),
                "line 1, column 16 does not parse; expected an object of header names to values, \
                 such as { \"x-api-key\": \"<key>\" }"
                    .to_owned(),
            ),
            (
                format!("[\"{SECRET}\"]"),
                "line 1, column 1 does not parse; expected an object of header names to values, \
                 such as { \"x-api-key\": \"<key>\" }"
                    .to_owned(),
            ),
            (
                format!("{{ \"x-api-key\": 7, \"x-token\": \"{SECRET}\" }}"),
                "line 1, column 16 does not parse; expected an object of header names to values, \
                 such as { \"x-api-key\": \"<key>\" }"
                    .to_owned(),
            ),
            (
                format!("{{ \"a\": \"1\", \"Bearer {SECRET}\": \"\" }}"),
                "entry 2 has a name that is no header name; a header name holds letters, \
                 digits and `-`, with no space or `:`"
                    .to_owned(),
            ),
            (
                format!("{{ \"x-api-key\": \"{SECRET}\\n\" }}"),
                "the value of x-api-key holds a character no header value can carry, such as \
                 a newline or a control character"
                    .to_owned(),
            ),
            (
                format!("{{ \"X-Api-Key\": \"{SECRET}\", \"x-api-key\": \"other\" }}"),
                "two entries name the header x-api-key".to_owned(),
            ),
            (
                format!("{{ \"x-api-key\": \"{SECRET}\", \"x-api-key\": \"{SECRET}\" }}"),
                "two entries name the header x-api-key".to_owned(),
            ),
        ] {
            let (_dir, path) = headers_file(&content);
            let error = OtlpHeaders::load(&path).unwrap_err();
            assert_eq!(error, format!("{}: {reason}", path.display()));
            assert!(!error.contains(SECRET), "{error}");
        }
    }

    #[test]
    fn headers_compare_by_name_and_value() {
        let (_dir, first) = headers_file("{ \"x-api-key\": \"one\" }");
        let (_dir, same) = headers_file("{ 'x-api-key': 'one' }");
        let (_dir, rotated) = headers_file("{ \"x-api-key\": \"two\" }");
        let first = OtlpHeaders::load(&first).unwrap();
        assert_eq!(first, OtlpHeaders::load(&same).unwrap());
        assert_ne!(first, OtlpHeaders::load(&rotated).unwrap());
    }
}
