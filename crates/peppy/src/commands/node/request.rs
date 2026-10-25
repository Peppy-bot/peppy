//! The request or the goal that `peppy node call` sends, built from its
//! files and flags against the message format of the member.
//!
//! `--args <file.json5>` gives the whole message, then each `--set` and
//! `--file` gives one field by its path. A field that two flags name, or a
//! field inside another that a flag names, is refused. The values follow the
//! canonical JSON of the codec: 64-bit integers as decimal strings, bytes as
//! base64 and times as RFC 3339 strings.

use config::node::{MessageFormat, SchemaType, TypeToken};
use indexmap::IndexMap;
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// A dotted path to a field of a message: `pose.position.x`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldPath(Vec<String>);

impl FieldPath {
    /// Whether `self` names `other` or a field inside it, or the reverse:
    /// two flags that name such a pair would give one field twice.
    fn overlaps(&self, other: &FieldPath) -> bool {
        let shorter = self.0.len().min(other.0.len());
        self.0[..shorter] == other.0[..shorter]
    }
}

impl FromStr for FieldPath {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let segments: Vec<String> = text.split('.').map(str::to_owned).collect();
        if segments.iter().any(String::is_empty) {
            return Err(format!(
                "`{text}` is not a field path: write the names of the fields from the root of \
                 the message, joined by `.`"
            ));
        }
        Ok(Self(segments))
    }
}

impl fmt::Display for FieldPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("."))
    }
}

/// One `--set <field>=<value>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSetting {
    pub field: FieldPath,
    pub value: String,
}

impl FromStr for FieldSetting {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let (field, value) = text
            .split_once('=')
            .ok_or_else(|| format!("`{text}` is not `<field>=<value>`"))?;
        Ok(Self {
            field: field.parse()?,
            value: value.to_owned(),
        })
    }
}

/// One `--file <field>=@<path>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldFile {
    pub field: FieldPath,
    pub path: PathBuf,
}

impl FromStr for FieldFile {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let (field, source) = text
            .split_once('=')
            .ok_or_else(|| format!("`{text}` is not `<field>=@<path>`"))?;
        let path = source
            .strip_prefix('@')
            .filter(|path| !path.is_empty())
            .ok_or_else(|| {
                format!("`{text}` is not `<field>=@<path>`: the path starts with `@`")
            })?;
        Ok(Self {
            field: field.parse()?,
            path: PathBuf::from(path),
        })
    }
}

/// What the command line gives of one message.
#[derive(Debug, Clone, Default)]
pub struct MessageArguments {
    /// `--args`: a JSON5 file that holds the whole message.
    pub args: Option<PathBuf>,
    /// `--set`, in command-line order.
    pub settings: Vec<FieldSetting>,
    /// `--file`, in command-line order.
    pub files: Vec<FieldFile>,
}

impl MessageArguments {
    /// The message these arguments give, for a member whose message has the
    /// format `format`. A member whose message carries no payload (`format`
    /// `None` or empty) takes an empty object: an `--args` file that gives
    /// it a field, a `--set` or a `--file` is refused.
    pub fn build(&self, format: Option<&MessageFormat>) -> Result<Value, String> {
        self.refuse_overlapping_fields()?;
        let mut message = match &self.args {
            Some(path) => read_args_file(path)?,
            None => Value::Object(Map::new()),
        };
        let Some(format) = message_codec::payload_format(format) else {
            return no_payload_message(&message, self);
        };
        for setting in &self.settings {
            let schema = field_schema(format, &setting.field)?;
            let value = setting_value(&setting.field, schema, &setting.value)?;
            insert_field(&mut message, &setting.field, value)?;
        }
        for file in &self.files {
            let schema = field_schema(format, &file.field)?;
            let value = file_value(&file.field, schema, &file.path)?;
            insert_field(&mut message, &file.field, value)?;
        }
        Ok(message)
    }

    /// Refuses a field that two flags name, or a field inside one that
    /// another flag names.
    fn refuse_overlapping_fields(&self) -> Result<(), String> {
        let named: Vec<(&FieldPath, &'static str)> = self
            .settings
            .iter()
            .map(|setting| (&setting.field, "--set"))
            .chain(self.files.iter().map(|file| (&file.field, "--file")))
            .collect();
        for (index, (field, flag)) in named.iter().enumerate() {
            if let Some((other, other_flag)) = named[index + 1..]
                .iter()
                .find(|(other, _)| field.overlaps(other))
            {
                return Err(format!(
                    "`{flag} {field}` and `{other_flag} {other}` give the same field: name each \
                     field once"
                ));
            }
        }
        Ok(())
    }
}

/// The empty message of a member whose message carries no payload, unless
/// `arguments` give it a field: its `--args` file `message`, or a `--set`
/// or a `--file`.
fn no_payload_message(message: &Value, arguments: &MessageArguments) -> Result<Value, String> {
    let args_gives_fields = message.as_object().is_some_and(|fields| !fields.is_empty());
    if args_gives_fields || !arguments.settings.is_empty() || !arguments.files.is_empty() {
        return Err(
            "the member takes no message, so no `--args` field, `--set` or `--file` applies"
                .to_owned(),
        );
    }
    Ok(Value::Object(Map::new()))
}

/// The whole message held by the JSON5 file at `path`, which is an object.
fn read_args_file(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read `--args` file {}: {e}", path.display()))?;
    let value: Value = serde_json5::from_str(&text)
        .map_err(|e| format!("`--args` file {} is not JSON5: {e}", path.display()))?;
    if !value.is_object() {
        return Err(format!(
            "`--args` file {} holds no object: it gives the whole message, an object of its \
             fields",
            path.display()
        ));
    }
    Ok(value)
}

/// The schema of the field `path` names in `format`. A path goes through
/// objects only.
fn field_schema<'f>(format: &'f MessageFormat, path: &FieldPath) -> Result<&'f SchemaType, String> {
    let mut fields: &IndexMap<String, SchemaType> = &format.0;
    let (last, parents) = path.0.split_last().expect("a field path has a segment");
    for (depth, segment) in parents.iter().enumerate() {
        let schema = fields
            .get(segment)
            .ok_or_else(|| unknown_field(path, &path.0[..=depth], fields))?;
        let SchemaType::Object(object) = schema else {
            return Err(format!(
                "`{path}`: `{}` is not an object, so it has no field `{}`",
                path.0[..=depth].join("."),
                path.0[depth + 1]
            ));
        };
        fields = &object.fields;
    }
    fields
        .get(last)
        .ok_or_else(|| unknown_field(path, &path.0, fields))
}

fn unknown_field(
    path: &FieldPath,
    reached: &[String],
    fields: &IndexMap<String, SchemaType>,
) -> String {
    let names: Vec<&str> = fields.keys().map(String::as_str).collect();
    format!(
        "`{path}`: the message has no field `{}`; the fields there: {}",
        reached.join("."),
        names.join(", ")
    )
}

/// Whether the canonical JSON of `schema` is a string: a string, bytes in
/// base64, a time in RFC 3339, or a 64-bit integer as a decimal string.
fn is_string_valued(schema: &SchemaType) -> bool {
    matches!(
        schema.as_type_token(),
        Some(
            TypeToken::String
                | TypeToken::Bytes
                | TypeToken::Time
                | TypeToken::U64
                | TypeToken::I64
        )
    )
}

/// The value of a `--set`: the text as written for a field whose canonical
/// JSON is a string, and the text read as JSON5 for any other field.
fn setting_value(path: &FieldPath, schema: &SchemaType, text: &str) -> Result<Value, String> {
    if is_string_valued(schema) {
        return Ok(Value::String(text.to_owned()));
    }
    serde_json5::from_str(text)
        .map_err(|e| format!("`--set {path}`: `{text}` is not a JSON5 value: {e}"))
}

/// The value of a `--file`: the text of the file for a string field, its
/// bytes for a bytes field, and for a directory the list of
/// `{ path, content }` its files give, for a list of objects with those two
/// fields.
fn file_value(path: &FieldPath, schema: &SchemaType, source: &Path) -> Result<Value, String> {
    if source.is_dir() {
        let content = directory_content_schema(schema).ok_or_else(|| {
            format!(
                "`--file {path}=@{}` names a directory, and `{path}` is not a list of \
                 {{ path, content }}",
                source.display()
            )
        })?;
        return directory_value(path, source, content);
    }
    file_content(path, schema, source)
}

/// The content of one file as the field `schema` takes it.
fn file_content(path: &FieldPath, schema: &SchemaType, source: &Path) -> Result<Value, String> {
    let bytes = std::fs::read(source)
        .map_err(|e| format!("`--file {path}`: cannot read {}: {e}", source.display()))?;
    match schema.as_type_token() {
        Some(TypeToken::String) => String::from_utf8(bytes).map(Value::String).map_err(|_| {
            format!(
                "`--file {path}`: {} is not UTF-8 text, and `{path}` is a string",
                source.display()
            )
        }),
        Some(TypeToken::Bytes) => Ok(Value::String(peppy_mcp_runtime::bridge::bytes_to_base64(
            &bytes,
        ))),
        _ => Err(format!(
            "`--file {path}`: `{path}` is neither a string nor bytes, so no file gives it"
        )),
    }
}

/// The schema of the `content` field of the items of `schema`, when
/// `schema` is a list of objects with a string `path` and a string or bytes
/// `content`, and nothing else.
fn directory_content_schema(schema: &SchemaType) -> Option<&SchemaType> {
    let SchemaType::Array(array) = schema else {
        return None;
    };
    let SchemaType::Object(item) = array.items.as_ref() else {
        return None;
    };
    let path = item.fields.get("path")?;
    let content = item.fields.get("content")?;
    let fits = item.fields.len() == 2
        && path.as_type_token() == Some(&TypeToken::String)
        && matches!(
            content.as_type_token(),
            Some(TypeToken::String | TypeToken::Bytes)
        );
    fits.then_some(content)
}

/// One `{ path, content }` for each file under `directory`, in the order of
/// their paths, each path relative to `directory` with `/` between its
/// names.
fn directory_value(
    path: &FieldPath,
    directory: &Path,
    content: &SchemaType,
) -> Result<Value, String> {
    let mut files = Vec::new();
    collect_files(directory, directory, &mut HashSet::new(), &mut files)
        .map_err(|e| format!("`--file {path}`: cannot read {}: {e}", directory.display()))?;
    files.sort();
    files
        .into_iter()
        .map(|(relative, file)| {
            let mut item = Map::new();
            item.insert("path".to_owned(), Value::String(relative));
            item.insert("content".to_owned(), file_content(path, content, &file)?);
            Ok(Value::Object(item))
        })
        .collect::<Result<Vec<Value>, String>>()
        .map(Value::Array)
}

/// Every file under `directory`, with its path relative to `root`. A
/// symbolic link is followed to what it names, and a link back to a
/// directory on the way is refused: it would list the same files without
/// end. An entry that is neither a file nor a directory, such as a pipe, a
/// socket or a broken link, is refused: it has no content to read, and a
/// read of a pipe would wait for a writer.
fn collect_files(
    root: &Path,
    directory: &Path,
    on_the_way: &mut HashSet<PathBuf>,
    files: &mut Vec<(String, PathBuf)>,
) -> std::io::Result<()> {
    let canonical = directory.canonicalize()?;
    if !on_the_way.insert(canonical.clone()) {
        return Err(std::io::Error::other(format!(
            "{} links back to a directory that holds it",
            directory.display()
        )));
    }
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(root, &path, on_the_way, files)?;
            continue;
        }
        if !path.is_file() {
            return Err(std::io::Error::other(format!(
                "{} is neither a file nor a directory",
                path.display()
            )));
        }
        let relative = path
            .strip_prefix(root)
            .expect("a file under the directory is under its root")
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        files.push((relative, path));
    }
    on_the_way.remove(&canonical);
    Ok(())
}

/// Puts `value` at `path` in `message`, adding the objects on the way that
/// the message does not hold.
fn insert_field(message: &mut Value, path: &FieldPath, value: Value) -> Result<(), String> {
    let (last, parents) = path.0.split_last().expect("a field path has a segment");
    let mut object = message
        .as_object_mut()
        .expect("a message is an object: `--args` refuses any other value");
    for (depth, segment) in parents.iter().enumerate() {
        let child = object
            .entry(segment.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        object = child.as_object_mut().ok_or_else(|| {
            format!(
                "`{path}`: the `--args` file gives `{}` a value that is not an object",
                path.0[..=depth].join(".")
            )
        })?;
    }
    object.insert(last.clone(), value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn format(json5: &str) -> MessageFormat {
        serde_json5::from_str(json5).expect("the format parses")
    }

    fn arguments(settings: &[&str], files: &[&str]) -> MessageArguments {
        MessageArguments {
            args: None,
            settings: settings.iter().map(|s| s.parse().unwrap()).collect(),
            files: files.iter().map(|f| f.parse().unwrap()).collect(),
        }
    }

    const POSE: &str = r#"{
        name: "string",
        count: "u64",
        gain: "i32",
        enabled: "bool",
        stamp: "time",
        pose: { $type: "object", x: "f64", y: "f64" },
        joints: { $type: "array", $items: "f64" },
        blob: "bytes",
    }"#;

    #[test]
    fn a_setting_takes_a_string_valued_field_as_written_and_any_other_as_json5() {
        let message = arguments(
            &[
                "name=arm: left",
                "count=18446744073709551615",
                "stamp=2026-10-25T10:00:00Z",
                "gain=-3",
                "enabled=true",
                "pose={x: 1.5, y: 2}",
                "joints=[0.1, 0.2]",
                "blob=aGVsbG8=",
            ],
            &[],
        )
        .build(Some(&format(POSE)))
        .expect("the message builds");
        assert_eq!(
            message,
            json!({
                "name": "arm: left",
                "count": "18446744073709551615",
                "stamp": "2026-10-25T10:00:00Z",
                "gain": -3,
                "enabled": true,
                "pose": {"x": 1.5, "y": 2},
                "joints": [0.1, 0.2],
                "blob": "aGVsbG8=",
            })
        );
    }

    #[test]
    fn a_setting_reaches_a_field_inside_an_object_and_adds_the_objects_on_the_way() {
        let message = arguments(&["pose.x=3"], &[])
            .build(Some(&format(POSE)))
            .expect("the message builds");
        assert_eq!(message, json!({"pose": {"x": 3}}));
    }

    #[test]
    fn the_flags_complete_the_message_of_the_args_file() {
        let dir = tempfile::tempdir().unwrap();
        let args = dir.path().join("goal.json5");
        std::fs::write(&args, "{ name: 'from the file', pose: { x: 1, y: 1 } }").unwrap();
        let mut given = arguments(&["pose.y=9"], &[]);
        given.args = Some(args);
        let message = given
            .build(Some(&format(POSE)))
            .expect("the message builds");
        assert_eq!(
            message,
            json!({"name": "from the file", "pose": {"x": 1, "y": 9}})
        );
    }

    #[test]
    fn a_field_that_two_flags_name_is_refused() {
        for (settings, files) in [
            (&["name=a", "name=b"][..], &[][..]),
            (&["pose={x: 1, y: 1}", "pose.x=2"][..], &[][..]),
            (&["blob=aGVsbG8="][..], &["blob=@/dev/null"][..]),
        ] {
            let error = arguments(settings, files)
                .build(Some(&format(POSE)))
                .expect_err("two flags give one field");
            assert!(error.contains("give the same field"), "{error}");
        }
    }

    #[test]
    fn a_field_the_message_does_not_have_is_refused_with_the_fields_it_has() {
        let error = arguments(&["pose.z=1"], &[])
            .build(Some(&format(POSE)))
            .expect_err("no such field");
        assert_eq!(
            error,
            "`pose.z`: the message has no field `pose.z`; the fields there: x, y"
        );
        let error = arguments(&["name.first=a"], &[])
            .build(Some(&format(POSE)))
            .expect_err("a string has no field");
        assert!(error.contains("`name` is not an object"), "{error}");
    }

    #[test]
    fn a_member_with_no_message_takes_no_field() {
        assert_eq!(arguments(&[], &[]).build(None), Ok(json!({})));
        assert_eq!(
            arguments(&[], &[]).build(Some(&MessageFormat::default())),
            Ok(json!({})),
            "an empty format carries no payload either"
        );
        assert!(arguments(&["name=a"], &[]).build(None).is_err());
        assert!(arguments(&[], &["blob=@/dev/null"]).build(None).is_err());
    }

    /// An `--args` file that gives a field to a member with no message is
    /// refused, as a `--set` is: the field would go nowhere. An empty object
    /// is the message itself.
    #[test]
    fn an_args_file_with_a_field_is_refused_for_a_member_with_no_message() {
        let dir = tempfile::tempdir().unwrap();
        let args = dir.path().join("goal.json5");
        std::fs::write(&args, "{ name: 'arm' }").unwrap();
        let mut given = arguments(&[], &[]);
        given.args = Some(args.clone());
        let error = given.build(None).expect_err("the field goes nowhere");
        assert!(error.contains("takes no message"), "{error}");

        std::fs::write(&args, "{}").unwrap();
        assert_eq!(given.build(None), Ok(json!({})));
    }

    #[test]
    fn a_file_gives_text_to_a_string_field_and_base64_to_a_bytes_field() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("note.txt");
        std::fs::write(&file, "hello").unwrap();
        let message = arguments(
            &[],
            &[
                &format!("name=@{}", file.display()),
                &format!("blob=@{}", file.display()),
            ],
        )
        .build(Some(&format(POSE)))
        .expect("the message builds");
        assert_eq!(message, json!({"name": "hello", "blob": "aGVsbG8="}));
    }

    #[test]
    fn a_directory_gives_one_path_and_content_per_file_in_path_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("b.txt"), "two").unwrap();
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
        std::fs::write(dir.path().join("sub/c.txt"), "three").unwrap();
        let format = format(
            r#"{ files: { $type: "array", $items: { $type: "object", path: "string", content: "string" } } }"#,
        );
        let message = arguments(&[], &[&format!("files=@{}", dir.path().display())])
            .build(Some(&format))
            .expect("the message builds");
        assert_eq!(
            message,
            json!({"files": [
                {"path": "a.txt", "content": "one"},
                {"path": "b.txt", "content": "two"},
                {"path": "sub/c.txt", "content": "three"},
            ]})
        );
    }

    #[test]
    fn a_directory_needs_a_list_of_path_and_content() {
        let dir = tempfile::tempdir().unwrap();
        let error = arguments(&[], &[&format!("name=@{}", dir.path().display())])
            .build(Some(&format(POSE)))
            .expect_err("a string field takes no directory");
        assert!(
            error.contains("is not a list of { path, content }"),
            "{error}"
        );
    }

    #[test]
    fn a_link_back_to_a_directory_on_the_way_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::os::unix::fs::symlink(dir.path(), dir.path().join("sub/loop")).unwrap();
        let format = format(
            r#"{ files: { $type: "array", $items: { $type: "object", path: "string", content: "bytes" } } }"#,
        );
        let error = arguments(&[], &[&format!("files=@{}", dir.path().display())])
            .build(Some(&format))
            .expect_err("the loop is refused");
        assert!(error.contains("links back to a directory"), "{error}");
    }

    #[test]
    fn an_entry_that_is_neither_a_file_nor_a_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(dir.path().join("control.sock"))
            .expect("the socket binds");
        let format = format(
            r#"{ files: { $type: "array", $items: { $type: "object", path: "string", content: "string" } } }"#,
        );
        let error = arguments(&[], &[&format!("files=@{}", dir.path().display())])
            .build(Some(&format))
            .expect_err("a socket has no content");
        assert!(
            error.contains("is neither a file nor a directory"),
            "{error}"
        );
    }

    #[test]
    fn the_flags_parse_their_syntax() {
        assert!("name".parse::<FieldSetting>().is_err());
        assert!("=x".parse::<FieldSetting>().is_err());
        assert!("a..b=x".parse::<FieldSetting>().is_err());
        assert_eq!(
            "text=a=b".parse::<FieldSetting>().unwrap().value,
            "a=b",
            "the value runs from the first `=`"
        );
        assert!("blob=/tmp/x".parse::<FieldFile>().is_err());
        assert!("blob=@".parse::<FieldFile>().is_err());
        assert_eq!(
            "blob=@/tmp/x".parse::<FieldFile>().unwrap().path,
            PathBuf::from("/tmp/x")
        );
    }
}
