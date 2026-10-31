//! `$include` in launcher documents. In a `launcher/v1` or a
//! `launcher_fragment/v1` document, any JSON value can be written as an
//! object whose one key is `$include` and whose value is the path of a JSON5
//! file: `{ $include: "../so101/product.json5" }`. When peppy reads the
//! document from its file, it puts the whole value of that file in place of
//! the include object, and then parses the document into its type.
//!
//! An include path follows the rules of a fragment path (see
//! [`resolve_repository_path`]): it is relative to the directory of the
//! document that holds the include, and it stays inside the launcher's
//! repository. An included file includes no other file. A document that
//! holds an include is read into a JSON value in which each object writes
//! each key once, and its type is parsed from that value once each include
//! is replaced; a document that holds none is parsed from its own text, once
//! when its text cannot write the key `$include`.

use super::repository_paths::{
    LauncherPaths, PathRefusal, canonical_directory_of, resolve_repository_path,
};
use crate::error::{Error, IncludeRefusal, ParsingError, Result};
use serde::Deserialize;
use serde::de::{DeserializeOwned, Deserializer, Error as _, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use std::fmt;
use std::path::{Path, PathBuf};

/// The key of an include object.
pub(super) const INCLUDE_KEY: &str = "$include";

/// The rule an include path keeps, as a refusal of a path states it.
const INCLUDE_PATH_RULE: &str = "an include path is relative to the directory of the document \
     that holds the include, and it stays inside the launcher's repository: the directory of \
     the nearest `peppy_repository.json5` above the launcher, or the launcher's own directory \
     for a standalone launcher";

/// Where a launcher document is read from, which sets where its includes
/// resolve.
#[derive(Debug, Clone, Copy)]
pub(super) enum DocumentSource<'a> {
    /// Text with no file. The document holds no include, because an include
    /// resolves against the directory of its document.
    Text,
    /// The document at this path, inside the repository of the nearest
    /// `peppy_repository.json5` above it, or inside its own directory when
    /// there is none.
    File(&'a Path),
    /// The launcher at `file` of the repository whose root is `root`, as the
    /// walk of that repository reads it.
    RepositoryFile { file: &'a Path, root: &'a Path },
    /// The fragment at `file`, which a launcher whose paths are `paths`
    /// references.
    FragmentOf {
        file: &'a Path,
        paths: &'a LauncherPaths,
    },
}

impl DocumentSource<'_> {
    /// The name a refusal gives the document.
    fn label(&self) -> String {
        match self {
            Self::Text => "the document read from text".to_owned(),
            Self::File(file)
            | Self::RepositoryFile { file, .. }
            | Self::FragmentOf { file, .. } => file.display().to_string(),
        }
    }

    /// The canonical directory of the document and the paths of its
    /// launcher, or why no include of the document resolves.
    fn include_base(&self) -> std::result::Result<(PathBuf, LauncherPaths), String> {
        let paths = match self {
            Self::Text => {
                return Err(
                    "the document is read from text and has no file path, and an \
                            include resolves against the directory of its document"
                        .to_owned(),
                );
            }
            Self::File(file) => LauncherPaths::of_launcher(file),
            Self::RepositoryFile { file, root } => LauncherPaths::in_repository(file, root),
            Self::FragmentOf { file, paths } => {
                canonical_directory_of(file).map(|directory| LauncherPaths {
                    directory,
                    boundary: paths.boundary.clone(),
                })
            }
        }
        .map_err(|reason| format!("the launcher's directory cannot be used: {reason}"))?;
        Ok((paths.directory.clone(), paths))
    }
}

/// Parses the launcher document `content`, read from `source`, into `T`:
/// from its own text when it holds no include, else from the JSON value in
/// which each include is replaced by the value of its file.
pub(super) fn deserialize_launcher_document<T: DeserializeOwned>(
    content: &str,
    source: DocumentSource<'_>,
) -> Result<T> {
    if !may_write_include_key(content) {
        return crate::error::deserialize_json5_with_path(content);
    }
    let mut document = match written_includes(content) {
        WrittenIncludes::None => return crate::error::deserialize_json5_with_path(content),
        WrittenIncludes::Refused(message) => {
            return Err(Error::Parsing(ParsingError::CannotParseConfig(message)));
        }
        WrittenIncludes::Some(document) => document,
    };
    let reader = IncludeReader {
        document: source.label(),
        base: source.include_base(),
    };
    reader.resolve(&mut document, &mut JsonPath::default())?;
    crate::error::deserialize_value_with_path(document)
}

/// Whether a document writes an include, as [`written_includes`] reads it.
enum WrittenIncludes {
    /// The document holds no include, or does not parse as JSON5: the parse
    /// of its type from its text gives its refusal.
    None,
    /// The document holds an include, and is this JSON value, in which each
    /// object writes each key once.
    Some(Value),
    /// The document holds an include, and an object of it writes a key twice
    /// or a number that is not finite, which a JSON value cannot hold.
    Refused(String),
}

/// Whether the document `content` writes an include, read once into a JSON
/// value in which each object writes each key once. That read refuses what
/// the parse of a typed document from its text can take, such as a key that
/// a free value writes twice, so its refusal counts only for a document that
/// holds an include, which a second read without the unique keys tells.
fn written_includes(content: &str) -> WrittenIncludes {
    let refusal = match parse_with_unique_keys(content) {
        Ok(document) if first_include(&document).is_some() => {
            return WrittenIncludes::Some(document);
        }
        Ok(_) => return WrittenIncludes::None,
        Err(refusal) => refusal,
    };
    match serde_json5::from_str::<Value>(content) {
        Ok(written) if first_include(&written).is_some() => WrittenIncludes::Refused(refusal),
        _ => WrittenIncludes::None,
    }
}

/// Whether the text `content` can write the key `$include`: it holds the key
/// once each escape of the text is read as the character that it writes. A
/// text that cannot write the key holds no include, so it is parsed once,
/// from its own text.
fn may_write_include_key(content: &str) -> bool {
    content.contains(INCLUDE_KEY)
        || (content.contains('\\') && unescaped(content).contains(INCLUDE_KEY))
}

/// `content` with each JSON5 escape replaced by the character that it writes,
/// wherever it stands: `\u0024` and `\x24` write `$`, `\n` a line feed, a
/// backslash before a line terminator nothing, and a backslash before any
/// other character that character. An escape outside a string, such as one
/// in a comment, only makes [`may_write_include_key`] read a text that holds
/// no include, which then parses as one that holds none.
fn unescaped(content: &str) -> String {
    let mut text = String::with_capacity(content.len());
    let mut characters = content.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\\' {
            text.push(character);
            continue;
        }
        let Some(escaped) = characters.next() else {
            break;
        };
        match escaped {
            'u' => text.push(hex_escape(&mut characters, 4)),
            'x' => text.push(hex_escape(&mut characters, 2)),
            'b' => text.push('\u{8}'),
            'f' => text.push('\u{c}'),
            'n' => text.push('\n'),
            'r' => text.push('\r'),
            't' => text.push('\t'),
            'v' => text.push('\u{b}'),
            '0' => text.push('\0'),
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
            }
            '\n' | '\u{2028}' | '\u{2029}' => {}
            other => text.push(other),
        }
    }
    text
}

/// The character of the `digits` hexadecimal digits that `characters` gives
/// next, or the replacement character when they write none, such as one half
/// of a surrogate pair, which writes no character of `$include`.
fn hex_escape(characters: &mut impl Iterator<Item = char>, digits: usize) -> char {
    let hex: String = characters.take(digits).collect();
    u32::from_str_radix(&hex, 16)
        .ok()
        .and_then(char::from_u32)
        .unwrap_or(char::REPLACEMENT_CHARACTER)
}

/// `content` as a JSON value in which each object writes each key once. The
/// error names the JSON path of the object that repeats a key.
fn parse_with_unique_keys(content: &str) -> std::result::Result<Value, String> {
    let mut deserializer =
        serde_json5::Deserializer::from_str(content).map_err(|error| error.to_string())?;
    serde_path_to_error::deserialize::<_, UniqueKeysValue>(&mut deserializer)
        .map(|value| value.0)
        .map_err(|error| {
            let path = error.path().to_string();
            let serde_json5::Error::Message { msg, .. } = error.into_inner();
            crate::error::message_at_path(&path, msg)
        })
}

/// The JSON path of the first object of `value` that holds `$include`, in
/// document order.
fn first_include(value: &Value) -> Option<JsonPath> {
    let mut at = JsonPath::default();
    find_include(value, &mut at).then_some(at)
}

/// Whether `value` holds an include object, with `at` left at its path.
fn find_include(value: &Value, at: &mut JsonPath) -> bool {
    match value {
        Value::Object(object) if object.contains_key(INCLUDE_KEY) => true,
        Value::Object(object) => object.iter().any(|(key, child)| {
            at.push(Segment::Key(key.clone()));
            let found = find_include(child, at);
            if !found {
                at.pop();
            }
            found
        }),
        Value::Array(items) => items.iter().enumerate().any(|(index, child)| {
            at.push(Segment::Index(index));
            let found = find_include(child, at);
            if !found {
                at.pop();
            }
            found
        }),
        _ => false,
    }
}

/// Replaces each include of one document by the value of its file.
struct IncludeReader {
    /// The name a refusal gives the document.
    document: String,
    /// The canonical directory of the document and the paths of its
    /// launcher, or why no include of the document resolves.
    base: std::result::Result<(PathBuf, LauncherPaths), String>,
}

impl IncludeReader {
    fn resolve(&self, value: &mut Value, at: &mut JsonPath) -> std::result::Result<(), Error> {
        match value {
            Value::Object(object) if object.contains_key(INCLUDE_KEY) => {
                *value = self.read_include(object, at)?;
            }
            Value::Object(object) => {
                for (key, child) in object.iter_mut() {
                    at.push(Segment::Key(key.clone()));
                    self.resolve(child, at)?;
                    at.pop();
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter_mut().enumerate() {
                    at.push(Segment::Index(index));
                    self.resolve(child, at)?;
                    at.pop();
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The value of the file that the include object `object` at `at`
    /// names.
    fn read_include(
        &self,
        object: &Map<String, Value>,
        at: &JsonPath,
    ) -> std::result::Result<Value, Error> {
        let written = &object[INCLUDE_KEY];
        let refuse = |reason: String| {
            Error::Parsing(ParsingError::Include(Box::new(IncludeRefusal {
                document: self.document.clone(),
                at: at.to_string(),
                include: written
                    .as_str()
                    .map_or_else(|| written.to_string(), str::to_owned),
                reason,
            })))
        };
        if object.len() > 1 {
            return Err(refuse(format!(
                "an include object holds `$include` alone, and this one also holds {}",
                crate::error::format_quoted_list(
                    object.keys().filter(|key| key.as_str() != INCLUDE_KEY)
                )
            )));
        }
        let Some(raw) = written.as_str() else {
            return Err(refuse(
                "`$include` takes the path of a JSON5 file, as a string".to_owned(),
            ));
        };
        let (directory, paths) = self.base.as_ref().map_err(|why| refuse(why.clone()))?;
        let file =
            resolve_repository_path(paths, directory, raw).map_err(|refusal| match refusal {
                PathRefusal::Rule(reason) => refuse(format!("{reason}; {INCLUDE_PATH_RULE}")),
                PathRefusal::Unresolved(error) => refuse(format!("it cannot be read: {error}")),
            })?;
        let content = std::fs::read_to_string(&file)
            .map_err(|error| refuse(format!("it cannot be read: {error}")))?;
        if content.trim().is_empty() {
            return Err(refuse("the file is empty".to_owned()));
        }
        let included = parse_with_unique_keys(&content)
            .map_err(|error| refuse(format!("it does not parse as JSON5: {error}")))?;
        if let Some(nested) = first_include(&included) {
            return Err(refuse(format!(
                "the included file holds an `$include` at `{nested}`, and an included file \
                 includes no other file"
            )));
        }
        Ok(included)
    }
}

/// The JSON path of a value in a document, as the parse errors write it:
/// `adjustments[2].set_daemon_scopes.stack`, and `.` for the whole document.
#[derive(Debug, Default)]
struct JsonPath(Vec<Segment>);

#[derive(Debug)]
enum Segment {
    Key(String),
    Index(usize),
}

impl JsonPath {
    fn push(&mut self, segment: Segment) {
        self.0.push(segment);
    }

    fn pop(&mut self) {
        self.0.pop();
    }
}

impl fmt::Display for JsonPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str(".");
        }
        for (position, segment) in self.0.iter().enumerate() {
            match segment {
                Segment::Key(key) if position == 0 => f.write_str(key)?,
                Segment::Key(key) => write!(f, ".{key}")?,
                Segment::Index(index) => write!(f, "[{index}]")?,
            }
        }
        Ok(())
    }
}

/// A JSON value read from JSON5 text in which each object writes each key
/// once. A value that holds an include is read into this form, because a
/// JSON object keeps one value of each key: a repeated key, which the parse
/// of the typed document refuses, would otherwise pass unseen.
struct UniqueKeysValue(Value);

impl<'de> Deserialize<'de> for UniqueKeysValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        deserializer
            .deserialize_any(UniqueKeysVisitor)
            .map(UniqueKeysValue)
    }
}

struct UniqueKeysVisitor;

impl<'de> Visitor<'de> for UniqueKeysVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> std::result::Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> std::result::Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_u64<E>(self, value: u64) -> std::result::Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_f64<E: serde::de::Error>(self, value: f64) -> std::result::Result<Value, E> {
        Number::from_f64(value).map(Value::Number).ok_or_else(|| {
            E::custom(format!(
                "`{value}` is not a finite number, and a JSON value holds finite numbers only"
            ))
        })
    }

    fn visit_str<E>(self, value: &str) -> std::result::Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> std::result::Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<Value, D::Error> {
        UniqueKeysValue::deserialize(deserializer).map(|value| value.0)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(UniqueKeysValue(item)) = seq.next_element()? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Value, A::Error> {
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if object.contains_key(&key) {
                return Err(A::Error::custom(format!(
                    "the key `{key}` is written twice in one object"
                )));
            }
            let UniqueKeysValue(value) = map.next_value()?;
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_json_path_is_written_as_the_parse_errors_write_it() {
        let mut at = JsonPath::default();
        assert_eq!(at.to_string(), ".");
        for segment in [
            Segment::Key("adjustments".to_owned()),
            Segment::Index(2),
            Segment::Key("set_daemon_scopes".to_owned()),
            Segment::Key("stack".to_owned()),
            Segment::Key("options".to_owned()),
            Segment::Index(1),
            Segment::Key("product".to_owned()),
        ] {
            at.push(segment);
        }
        assert_eq!(
            at.to_string(),
            "adjustments[2].set_daemon_scopes.stack.options[1].product"
        );
    }

    #[test]
    fn the_first_include_is_found_in_document_order() {
        let value: Value = serde_json5::from_str(
            r#"{ a: [1, { b: { $include: "x.json5" } }], c: { $include: "y.json5" } }"#,
        )
        .unwrap();
        assert_eq!(
            first_include(&value).map(|at| at.to_string()),
            Some("a[1].b".to_owned())
        );
        let plain: Value = serde_json5::from_str(r#"{ a: [1, { b: "$include" }] }"#).unwrap();
        assert!(first_include(&plain).is_none());
    }

    #[test]
    fn only_a_text_that_writes_the_include_key_once_unescaped_can_write_an_include() {
        assert!(may_write_include_key(r#"{ a: { $include: "x.json5" } }"#));
        assert!(may_write_include_key(
            r#"{ a: { "\u0024include": "x.json5" } }"#
        ));
        assert!(may_write_include_key(
            r#"{ a: { "\x24inc\lude": "x.json5" } }"#
        ));
        assert!(may_write_include_key(
            "{ a: { \"$incl\\\nude\": \"x.json5\" } }"
        ));
        assert!(may_write_include_key(
            "{ a: { \"$incl\\\r\nude\": \"x.json5\" } }"
        ));
        assert!(!may_write_include_key(
            r#"{ a: { include: "x.json5" }, b: "$inc", c: "lude" }"#
        ));
        assert!(
            !may_write_include_key(r#"{ description: "A \"quoted\" word", pattern: "^a\\d+$" }"#),
            "escapes that write no `$include`"
        );
        assert!(!may_write_include_key(r#"{ a: "$incl\nude" }"#));
        assert!(!may_write_include_key(
            r#"{ a: "\uD83D\uDE00", b: "trailing \"#
        ));
    }

    #[test]
    fn a_document_writes_an_include_only_with_an_include_object() {
        assert!(matches!(
            written_includes(r#"{ a: { $include: "x.json5" } }"#),
            WrittenIncludes::Some(_)
        ));
        assert!(matches!(
            written_includes(r#"{ a: "the key $include", b: { c: 1, c: 2 } }"#),
            WrittenIncludes::None
        ));
        assert!(matches!(
            written_includes(r#"{ a: { $include: "x.json5" }, b: { c: 1, c: 2 } }"#),
            WrittenIncludes::Refused(message) if message == "b: the key `c` is written twice in one object"
        ));
        assert!(matches!(
            written_includes(r#"{ a: { $include: "x.json5" "#),
            WrittenIncludes::None
        ));
    }

    #[test]
    fn a_value_with_unique_keys_refuses_a_repeated_key_at_its_object() {
        let error = parse_with_unique_keys(r#"{ a: [{ b: 1, b: 2 }] }"#).unwrap_err();
        assert_eq!(error, "a[0]: the key `b` is written twice in one object");
        let error = parse_with_unique_keys("{ a: Infinity }").unwrap_err();
        assert!(
            error.starts_with("a: `inf` is not a finite number"),
            "{error}"
        );
        assert_eq!(
            parse_with_unique_keys(r#"{ a: [1, 2.5, "x", null, true], b: { c: -3 } }"#).unwrap(),
            serde_json::json!({ "a": [1, 2.5, "x", null, true], "b": { "c": -3 } })
        );
    }
}
