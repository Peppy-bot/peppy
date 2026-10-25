//! The Cap'n Proto schema of a message format, and its compile.
//!
//! [`MessageSchema::render`] writes the schema that peppy compiles for a
//! [`MessageFormat`]: the code generator of peppy builds the accessors of
//! generated nodes from it, and the codec lays out the wire from it. Both
//! sides render the same text for the same format, so a codec and a
//! generated node agree on every field. [`MessageSchema::compile`] runs the
//! Cap'n Proto compiler (see [`crate::compiler`]) on the text and keeps the
//! code generator request that the compiler gives.

use crate::compiler::{self, CompilerError};
use capnp::message::{Reader, ReaderOptions};
use capnp::schema_capnp::{code_generator_request, node};
use capnp::serialize::{self, OwnedSegments};
use config::node::{MessageFormat, PrimitiveSchema, SchemaType, TypeToken};
use indexmap::IndexMap;
use std::collections::HashMap;

/// The result of the schema functions.
pub type Result<T> = std::result::Result<T, SchemaError>;

/// A schema did not compile, or its compile is not what the renderer wrote.
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    /// The process has no usable Cap'n Proto compiler.
    #[error(transparent)]
    Compiler(#[from] CompilerError),
    /// The schema or the request of the compiler could not be written or
    /// read.
    #[error("cannot write or read the files of the schema compile: {0}")]
    Io(#[from] std::io::Error),
    /// The compiler refused the schema, or its request does not read.
    #[error("the Cap'n Proto compiler failed: {0}")]
    Capnp(#[from] capnp::Error),
    /// The compiled schema does not hold what the renderer wrote.
    #[error("{0}")]
    Layout(String),
}

/// The Cap'n Proto schema text of one message format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageSchema {
    text: String,
}

impl MessageSchema {
    /// Renders `format` as a schema whose root struct is `Message`.
    /// `schema_name` seeds the file id of the schema; the id does not appear
    /// in messages.
    pub fn render(schema_name: &str, format: &MessageFormat) -> Self {
        let mut renderer = SchemaRenderer::default();
        let mut text = String::new();
        let mut schema_id = schema_id(schema_name, &format.0);
        schema_id |= 1; // ensure odd
        schema_id |= 1 << 63; // high bit set per capnp id recommendations

        text.push_str(&format!("@0x{schema_id:016x};\n"));
        text.push('\n');
        text.push_str(&renderer.render_struct("Message", &format.0, 0));

        if renderer.timestamp_struct_needed {
            text.push('\n');
            text.push_str("struct Timestamp {\n  sec @0 :Int64;\n  nsec @1 :UInt32;\n}\n");
        }
        Self { text }
    }

    /// The schema text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Writes the schema to a temporary directory, runs the Cap'n Proto
    /// compiler on it and reads back its code generator request.
    pub fn compile(&self) -> Result<CompiledSchema> {
        let temp_dir = tempfile::tempdir()?;
        let schema_path = temp_dir.path().join("message.capnp");
        std::fs::write(&schema_path, &self.text)?;
        let request_path = temp_dir.path().join("code_generator_request.bin");

        let capnp = compiler::binary_path()?;
        let mut command = capnpc::CompilerCommand::new();
        command
            .capnp_executable(capnp)
            .file(&schema_path)
            .output_path(temp_dir.path())
            .raw_code_generator_request_path(&request_path);
        command.run()?;

        let mut request_file = std::fs::File::open(&request_path)?;
        let message = serialize::read_message(&mut request_file, ReaderOptions::new())?;
        Ok(CompiledSchema { message })
    }
}

/// The compiler's view of one rendered schema: the code generator request
/// that the `capnp` binary gives for it, read fully into memory.
pub struct CompiledSchema {
    message: Reader<OwnedSegments>,
}

impl CompiledSchema {
    /// The raw message of the request, as code generators read it.
    pub fn message(&self) -> &Reader<OwnedSegments> {
        &self.message
    }

    /// The code generator request.
    pub fn request(&self) -> Result<code_generator_request::Reader<'_>> {
        Ok(self
            .message
            .get_root::<code_generator_request::Reader<'_>>()?)
    }

    /// The id of the file-level `Message` struct of the schema.
    pub fn root_message_id(&self) -> Result<u64> {
        SchemaNodes::from_request(self.request()?)?.root_message_id()
    }
}

/// The nodes of one compiled schema, by id, plus the id of the schema file.
pub(crate) struct SchemaNodes<'a> {
    by_id: HashMap<u64, node::Reader<'a>>,
    file_id: u64,
}

impl<'a> SchemaNodes<'a> {
    pub(crate) fn from_request(request: code_generator_request::Reader<'a>) -> Result<Self> {
        let requested_files = request.get_requested_files()?;
        if requested_files.is_empty() {
            return Err(SchemaError::Layout(
                "capnp request did not include any files".to_string(),
            ));
        }
        let file_id = requested_files.get(0).get_id();
        let by_id = request
            .get_nodes()?
            .iter()
            .map(|node| (node.get_id(), node))
            .collect();
        Ok(Self { by_id, file_id })
    }

    /// The id of the file-level `Message` struct every rendered schema roots at.
    pub(crate) fn root_message_id(&self) -> Result<u64> {
        for (id, node) in &self.by_id {
            if node.get_scope_id() != self.file_id {
                continue;
            }
            if !matches!(node.which(), Ok(node::Struct(_))) {
                continue;
            }
            let display_name = node
                .get_display_name()?
                .to_str()
                .map_err(|err| SchemaError::Layout(err.to_string()))?;
            let simple_name = display_name
                .rsplit(|ch| [':', '.'].contains(&ch))
                .next()
                .unwrap_or(display_name);
            if simple_name == "Message" {
                return Ok(*id);
            }
        }
        Err(SchemaError::Layout(
            "capnp request missing root Message struct".to_string(),
        ))
    }

    pub(crate) fn struct_node(&self, id: u64) -> Result<node::struct_::Reader<'a>> {
        let node = self.by_id.get(&id).ok_or_else(|| {
            SchemaError::Layout(format!(
                "capnp request missing node definition for struct id {id:#x}"
            ))
        })?;
        match node.which() {
            Ok(node::Struct(struct_reader)) => Ok(struct_reader),
            _ => Err(SchemaError::Layout(format!("node {id:#x} is not a struct"))),
        }
    }
}

/// The path of the field `name` inside the field at `path`, with a `.`
/// between the two: the path that names a field in the errors of a layout
/// and in the type mapping of the code generator.
pub fn join_path(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{path}.{name}")
    }
}

/// The Cap'n Proto type a token is rendered as. `time` names the file-level
/// `Timestamp` struct the renderer appends when any field uses it.
pub(crate) fn capnp_type_name(token: &TypeToken) -> &'static str {
    match token {
        TypeToken::Bool => "Bool",
        TypeToken::String => "Text",
        TypeToken::Bytes => "Data",
        TypeToken::Time => "Timestamp",
        TypeToken::U8 => "UInt8",
        TypeToken::U16 => "UInt16",
        TypeToken::U32 => "UInt32",
        TypeToken::U64 => "UInt64",
        TypeToken::I8 => "Int8",
        TypeToken::I16 => "Int16",
        TypeToken::I32 => "Int32",
        TypeToken::I64 => "Int64",
        TypeToken::F32 => "Float32",
        TypeToken::F64 => "Float64",
    }
}

/// The name a field of a message format has in the rendered schema: the
/// words of `input`, in camel case, with a leading `_` before a digit.
pub fn capnp_field_name(input: &str) -> String {
    let mut output = to_pascal_case(input);
    if output.is_empty() {
        output.push_str("Field");
    }

    let mut chars = output.chars();
    let mut camel = String::with_capacity(output.len());
    if let Some(first) = chars.next() {
        camel.push(first.to_ascii_lowercase());
        camel.extend(chars);
    }

    if camel.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
        camel.insert(0, '_');
    }

    if camel.is_empty() {
        "_field".to_string()
    } else {
        camel
    }
}

fn to_pascal_case(input: &str) -> String {
    let mut result = String::new();

    for segment in input
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
    {
        let mut chars = segment.chars();
        if let Some(first) = chars.next() {
            result.push(first.to_ascii_uppercase());
            for ch in chars {
                result.push(ch.to_ascii_lowercase());
            }
        }
    }

    result
}

/// The FNV-1a hash of the schema name and the shape of the format, which
/// seeds the file id of the schema.
fn schema_id(schema_name: &str, fields: &IndexMap<String, SchemaType>) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    fn update(mut hash: u64, bytes: &[u8]) -> u64 {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash
    }

    fn hash_usize(hash: u64, value: usize) -> u64 {
        update(hash, &(value as u64).to_le_bytes())
    }

    fn hash_schema(mut hash: u64, schema: &SchemaType) -> u64 {
        match schema {
            SchemaType::Type(token)
            | SchemaType::Primitive(PrimitiveSchema { kind: token, .. }) => {
                hash = update(hash, b"type");
                hash = update(hash, schema_id_token(token).as_bytes());
            }
            SchemaType::Array(array) => {
                hash = update(hash, b"array");
                hash = hash_schema(hash, array.items.as_ref());
                if let Some(len) = array.length {
                    hash = hash_usize(hash, len);
                }
            }
            SchemaType::Object(object) => {
                hash = update(hash, b"object");
                hash = hash_fields(hash, &object.fields);
            }
        }
        hash
    }

    fn hash_fields(mut hash: u64, fields: &IndexMap<String, SchemaType>) -> u64 {
        for (key, value) in fields {
            hash = update(hash, key.as_bytes());
            hash = hash_schema(hash, value);
        }
        hash
    }

    let hash = update(FNV_OFFSET, schema_name.as_bytes());
    hash_fields(hash, fields)
}

/// The text of a token in the hash of [`schema_id`].
fn schema_id_token(token: &TypeToken) -> &'static str {
    match token {
        TypeToken::Bool => "bool",
        TypeToken::String => "string",
        TypeToken::Bytes => "bytes",
        TypeToken::Time => "timestamp",
        TypeToken::U8 => "u8",
        TypeToken::U16 => "u16",
        TypeToken::U32 => "u32",
        TypeToken::U64 => "u64",
        TypeToken::I8 => "i8",
        TypeToken::I16 => "i16",
        TypeToken::I32 => "i32",
        TypeToken::I64 => "i64",
        TypeToken::F32 => "f32",
        TypeToken::F64 => "f64",
    }
}

#[derive(Default)]
struct SchemaRenderer {
    timestamp_struct_needed: bool,
}

struct TypeResolution {
    type_name: String,
    nested: Vec<String>,
}

impl SchemaRenderer {
    fn render_struct(
        &mut self,
        struct_name: &str,
        fields: &IndexMap<String, SchemaType>,
        depth: usize,
    ) -> String {
        let indent = "  ".repeat(depth);
        let mut buffer = String::new();
        buffer.push_str(&format!("{indent}struct {struct_name} {{\n"));

        let mut nested_defs = Vec::new();

        for (index, (field_name, schema_type)) in fields.iter().enumerate() {
            let sanitized_field = capnp_field_name(field_name);
            let field_indent = "  ".repeat(depth + 1);
            let TypeResolution { type_name, nested } =
                self.resolve_type(struct_name, &sanitized_field, schema_type, depth + 1);

            buffer.push_str(&format!(
                "{field_indent}{sanitized_field} @{index} :{type_name};\n"
            ));

            nested_defs.extend(nested);
        }

        if !nested_defs.is_empty() {
            buffer.push('\n');
            for nested in nested_defs {
                buffer.push_str(&nested);
            }
        }

        buffer.push_str(&format!("{indent}}}\n"));
        buffer
    }

    fn resolve_type(
        &mut self,
        parent_struct: &str,
        field_name: &str,
        schema: &SchemaType,
        depth: usize,
    ) -> TypeResolution {
        match schema {
            SchemaType::Type(token)
            | SchemaType::Primitive(PrimitiveSchema { kind: token, .. }) => TypeResolution {
                type_name: self.capnp_type_for_token(token).to_string(),
                nested: Vec::new(),
            },
            SchemaType::Array(array) => {
                if matches!(array.items.as_ref().as_type_token(), Some(TypeToken::U8)) {
                    return TypeResolution {
                        type_name: "Data".to_string(),
                        nested: Vec::new(),
                    };
                }

                let mut item_resolution = self.resolve_type(
                    parent_struct,
                    &format!("{field_name}_item"),
                    array.items.as_ref(),
                    depth,
                );
                let nested = std::mem::take(&mut item_resolution.nested);

                TypeResolution {
                    type_name: format!("List({})", item_resolution.type_name),
                    nested,
                }
            }
            SchemaType::Object(object) => {
                let struct_name = nested_struct_name(parent_struct, field_name);
                let nested = self.render_struct(&struct_name, &object.fields, depth);

                TypeResolution {
                    type_name: struct_name,
                    nested: vec![nested],
                }
            }
        }
    }

    fn capnp_type_for_token(&mut self, token: &TypeToken) -> &'static str {
        if *token == TypeToken::Time {
            self.timestamp_struct_needed = true;
        }
        capnp_type_name(token)
    }
}

fn nested_struct_name(parent_struct: &str, field_name: &str) -> String {
    let mut name = to_pascal_case(field_name);
    if name.is_empty() {
        name = format!("{parent_struct}Field");
    }

    if name.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
        name.insert(0, '_');
    }

    name
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(text: &str) -> MessageFormat {
        serde_json5::from_str(text).expect("valid format")
    }

    #[test]
    fn renders_nested_structs_and_the_timestamp_struct() {
        let schema = MessageSchema::render(
            "test_image",
            &format(
                r#"{
                  header: { $type: "object", stamp: "time", frame_id: "u32" },
                  encoding: "string",
                  width: "u32",
                  height: "u32",
                  image: { $type: "array", $items: "u8", $length: 3 },
                }"#,
            ),
        );
        let text = schema.text();
        assert!(
            text.starts_with("@0x"),
            "schema should start with a capnp file id, got {text:?}"
        );
        for expected in [
            "struct Message {",
            "  header @0 :Header;",
            "  encoding @1 :Text;",
            "  width @2 :UInt32;",
            "  height @3 :UInt32;",
            "  image @4 :Data;",
            "  struct Header {",
            "    stamp @0 :Timestamp;",
            "    frameId @1 :UInt32;",
            "struct Timestamp {",
            "  sec @0 :Int64;",
            "  nsec @1 :UInt32;",
        ] {
            assert!(
                text.contains(expected),
                "schema missing expected segment {expected:?}.\nSchema:\n{text}"
            );
        }
    }

    #[test]
    fn different_schema_names_produce_different_ids() {
        let fields =
            format(r#"{ arm_id: "u16", desired_position: { $type: "array", $items: "i32" } }"#);
        let id = |name: &str| {
            MessageSchema::render(name, &fields)
                .text()
                .lines()
                .next()
                .expect("first line")
                .to_owned()
        };
        assert_ne!(
            id("move_right_arm_goal_request"),
            id("move_left_arm_goal_request"),
            "identical field structures with different names must produce different schema IDs"
        );
        assert_eq!(
            id("move_arm_goal_request"),
            id("move_arm_goal_request"),
            "same name and fields must produce identical schema IDs"
        );
    }

    #[test]
    fn an_enum_field_is_text_and_marks_change_nothing() {
        let plain = MessageSchema::render("marked", &format(r#"{ mode: "string", item: "u32" }"#));
        let marked = MessageSchema::render(
            "marked",
            &format(
                r#"{
                    mode: { $type: "string", $enum: ["idle", "busy"] },
                    item: { $type: "u32", $marks: ["item_id"] },
                }"#,
            ),
        );
        assert_eq!(plain, marked);
    }

    #[test]
    fn the_compiled_schema_roots_at_message() {
        let compiled = MessageSchema::render("root", &format(r#"{ count: "u32" }"#))
            .compile()
            .expect("the schema compiles");
        compiled.root_message_id().expect("the root is found");
    }

    #[test]
    fn field_names_are_camel_case_with_a_guard_before_a_digit() {
        assert_eq!(capnp_field_name("frame_id"), "frameId");
        assert_eq!(capnp_field_name("2d_pose"), "_2dPose");
        assert_eq!(capnp_field_name("__"), "field");
        assert_eq!(join_path("", "pose"), "pose");
        assert_eq!(join_path("pose", "x"), "pose.x");
    }
}
