pub mod facade;
pub mod types;

pub use types::FunctionParam;

use crate::error::{Error, Result};
use capnp::message::ReaderOptions;
use capnp::schema_capnp::{field, node, type_};
use capnp::serialize;
use capnpc::codegen::GeneratorContext;
use capnpc::codegen_types::{Leaf, RustTypeInfo};
use proc_macro2::{Ident, Span, TokenStream};
use std::collections::{HashMap, HashSet};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::str::FromStr;
use tempfile::tempdir;

use crate::node::{ArraySchema, MessageFormat, SchemaType, TypeToken};
use facade::CapnpFacade;
use indexmap::IndexMap;

/// The output_dir should point to the `src` of a Rust crate. A new `capnp` module will be
/// created at the root of this directory with all the `capnp` files.
pub fn compile_capnp<P, O>(capnp_files: &[P], output_dir: O) -> Result<()>
where
    P: AsRef<Path>,
    O: AsRef<Path>,
{
    CapnpFacade::new()?.compile_files(capnp_files, output_dir)
}

/// This struct helps tuning a MessageFormat into a capnp proto and its associated Rust types
pub struct MessageFormatMapper {
    schema_name: String,
    message_format: MessageFormat,
}

impl MessageFormatMapper {
    pub fn new(schema_name: &str, message_format: MessageFormat) -> Self {
        Self {
            schema_name: schema_name.to_string(),
            message_format,
        }
    }

    /// Render this message format to a Cap'n Proto schema and resolve the
    /// matching Rust type for each field.
    ///
    /// Despite the `map_` name this is **not** a pure transform: resolving the
    /// Rust type mapping shells out to the Cap'n Proto compiler. The call
    /// creates a temporary directory, writes the generated schema into it,
    /// invokes the bundled `capnp` binary (locating/extracting it on first
    /// use), and reads back the code-generator request. Expect filesystem I/O
    /// and a subprocess, not just in-memory work.
    pub fn map_message_format_to_capnpn(&self) -> Result<CapnpSchemaArtifacts> {
        let mut generator = CapnpSchemaGenerator::default();
        let mut schema = String::new();
        let mut schema_id =
            MessageFormatMapper::compute_schema_id(&self.schema_name, &self.message_format.0);
        if schema_id == 0 {
            schema_id = 1;
        }
        schema_id |= 1; // ensure odd
        schema_id |= 1 << 63; // high bit set per capnp id recommendations

        schema.push_str(&format!("@0x{schema_id:016x};\n"));
        schema.push('\n');
        schema.push_str(&generator.render_struct("Message", &self.message_format.0, 0)?);

        if generator.timestamp_struct_needed {
            schema.push('\n');
            schema.push_str("struct Timestamp {\n  sec @0 :Int64;\n  nsec @1 :UInt32;\n}\n");
        }

        let type_mapping = self.compute_rust_type_mapping(&schema)?;
        Ok(CapnpSchemaArtifacts::new(
            self.message_format.clone(),
            schema,
            type_mapping,
        ))
    }

    fn compute_rust_type_mapping(&self, schema: &str) -> Result<HashMap<String, String>> {
        let temp_dir = tempdir()?;
        let schema_path = temp_dir.path().join("message.capnp");
        std::fs::write(&schema_path, schema)?;
        let request_path = temp_dir.path().join("code_generator_request.bin");

        let capnp = CapnpFacade::new()?;
        let mut command = capnpc::CompilerCommand::new();
        command
            .capnp_executable(capnp.binary_path())
            .file(&schema_path)
            .output_path(temp_dir.path())
            .raw_code_generator_request_path(&request_path);

        command.run()?;

        let mut request_file = std::fs::File::open(&request_path)?;
        let message = serialize::read_message(&mut request_file, ReaderOptions::new())?;
        let ctx = GeneratorContext::new(&message)?;

        let root_struct_id = find_root_struct_id(&ctx)?;
        let mut mapping = HashMap::new();
        let mut visited = HashSet::new();
        collect_struct_fields(&ctx, root_struct_id, "", &mut mapping, &mut visited)?;

        fn array_schema_to_rust_string(array: &ArraySchema) -> Option<String> {
            let token = array.items.as_ref().as_type_token()?;
            let (_, rust_type) = type_token_strings(token);
            let rendered = match array.length {
                Some(length) => format!("[{rust_type}; {length}]"),
                None => format!("[{rust_type}]"),
            };
            Some(rendered)
        }

        fn override_array_types(
            current_key: &str,
            schema: &SchemaType,
            mapping: &mut HashMap<String, String>,
        ) {
            match schema {
                SchemaType::Array(array) => {
                    if let Some(entry) = mapping.get_mut(current_key)
                        && let Some(rendered) = array_schema_to_rust_string(array)
                    {
                        *entry = rendered;
                    }
                    if let SchemaType::Object(object) = array.items.as_ref() {
                        for (field_name, field_schema) in &object.fields {
                            let sanitized = sanitize_field_name(field_name);
                            let next_key = if current_key.is_empty() {
                                format!("[].{sanitized}")
                            } else {
                                format!("{current_key}[].{sanitized}")
                            };
                            override_array_types(&next_key, field_schema, mapping);
                        }
                    }
                }
                SchemaType::Object(object) => {
                    for (field_name, field_schema) in &object.fields {
                        let sanitized = sanitize_field_name(field_name);
                        let next_key = if current_key.is_empty() {
                            sanitized
                        } else {
                            format!("{current_key}.{sanitized}")
                        };
                        override_array_types(&next_key, field_schema, mapping);
                    }
                }
                SchemaType::Type(_) | SchemaType::Primitive(_) => {}
            }
        }

        for (field_name, schema) in &self.message_format.0 {
            let sanitized = sanitize_field_name(field_name);
            override_array_types(&sanitized, schema, &mut mapping);
        }

        Ok(mapping)
    }

    fn compute_schema_id(schema_name: &str, fields: &IndexMap<String, SchemaType>) -> u64 {
        const FNV_OFFSET: u64 = 0xcbf29ce484222325;
        const FNV_PRIME: u64 = 0x100000001b3;

        fn update(mut hash: u64, bytes: &[u8], prime: u64) -> u64 {
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(prime);
            }
            hash
        }

        fn hash_usize(hash: u64, value: usize, prime: u64) -> u64 {
            update(hash, &(value as u64).to_le_bytes(), prime)
        }

        fn hash_schema(mut hash: u64, schema: &SchemaType, prime: u64) -> u64 {
            match schema {
                SchemaType::Type(token) => {
                    hash = update(hash, b"type", prime);
                    let (capnp_discriminant, _) = type_token_strings(token);
                    hash = update(hash, capnp_discriminant.as_bytes(), prime);
                }
                SchemaType::Primitive(schema) => {
                    hash = update(hash, b"type", prime);
                    let (capnp_discriminant, _) = type_token_strings(&schema.kind);
                    hash = update(hash, capnp_discriminant.as_bytes(), prime);
                }
                SchemaType::Array(array) => {
                    hash = update(hash, b"array", prime);
                    hash = hash_schema(hash, array.items.as_ref(), prime);
                    if let Some(len) = array.length {
                        hash = hash_usize(hash, len, prime);
                    }
                }
                SchemaType::Object(object) => {
                    hash = update(hash, b"object", prime);
                    hash = hash_fields(hash, &object.fields, prime);
                }
            }
            hash
        }

        fn hash_fields(mut hash: u64, fields: &IndexMap<String, SchemaType>, prime: u64) -> u64 {
            for (key, value) in fields {
                hash = update(hash, key.as_bytes(), prime);
                hash = hash_schema(hash, value, prime);
            }
            hash
        }

        let hash = update(FNV_OFFSET, schema_name.as_bytes(), FNV_PRIME);
        hash_fields(hash, fields, FNV_PRIME)
    }
}

pub struct CapnpSchemaArtifacts {
    message_format: MessageFormat,
    schema: String,
    type_mapping: HashMap<String, String>,
}

impl CapnpSchemaArtifacts {
    fn new(
        message_format: MessageFormat,
        schema: String,
        type_mapping: HashMap<String, String>,
    ) -> Self {
        Self {
            message_format,
            schema,
            type_mapping,
        }
    }

    pub fn build_function_params(&self) -> Result<Vec<FunctionParam>> {
        let mut params = Vec::with_capacity(self.message_format.0.len());

        for field_name in self.message_format.0.keys() {
            let sanitized = sanitize_field_name(field_name);
            let type_string = self.type_mapping.get(&sanitized).ok_or_else(|| {
            Error::Encoding(format!(
                "missing type mapping for field `{sanitized}` while building function parameters"
            ))
        })?;

            let ident = Ident::new(&sanitized, Span::call_site());
            let ty = TokenStream::from_str(type_string).map_err(|err| {
                Error::Encoding(format!(
                    "failed to parse rust type `{type_string}` for field `{sanitized}`: {err}"
                ))
            })?;

            params.push(FunctionParam::new(ident, ty));
        }

        Ok(params)
    }

    pub fn encoding_schema(&self) -> &str {
        &self.schema
    }

    pub fn type_mapping(&self) -> &HashMap<String, String> {
        &self.type_mapping
    }

    pub fn message_format(&self) -> &MessageFormat {
        &self.message_format
    }
}

#[derive(Default)]
struct CapnpSchemaGenerator {
    timestamp_struct_needed: bool,
}

impl CapnpSchemaGenerator {
    fn render_struct(
        &mut self,
        struct_name: &str,
        fields: &IndexMap<String, SchemaType>,
        depth: usize,
    ) -> Result<String> {
        let indent = "  ".repeat(depth);
        let mut buffer = String::new();
        buffer.push_str(&format!("{indent}struct {struct_name} {{\n"));

        let mut nested_defs = Vec::new();

        for (index, (field_name, schema_type)) in fields.iter().enumerate() {
            let sanitized_field = sanitize_field_name(field_name);
            let field_indent = "  ".repeat(depth + 1);
            let TypeResolution { type_name, nested } =
                self.resolve_type(struct_name, &sanitized_field, schema_type, depth + 1)?;

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
        Ok(buffer)
    }

    fn resolve_type(
        &mut self,
        parent_struct: &str,
        field_name: &str,
        schema: &SchemaType,
        depth: usize,
    ) -> Result<TypeResolution> {
        match schema {
            SchemaType::Type(token) => Ok(TypeResolution {
                type_name: self.capnp_type_for_token(token).to_string(),
                nested: Vec::new(),
            }),
            SchemaType::Primitive(primitive) => Ok(TypeResolution {
                type_name: self.capnp_type_for_token(&primitive.kind).to_string(),
                nested: Vec::new(),
            }),
            SchemaType::Array(array) => {
                if matches!(array.items.as_ref().as_type_token(), Some(TypeToken::U8)) {
                    return Ok(TypeResolution {
                        type_name: "Data".to_string(),
                        nested: Vec::new(),
                    });
                }

                let mut item_resolution = self.resolve_type(
                    parent_struct,
                    &format!("{field_name}_item"),
                    array.items.as_ref(),
                    depth,
                )?;
                let nested = std::mem::take(&mut item_resolution.nested);

                Ok(TypeResolution {
                    type_name: format!("List({})", item_resolution.type_name),
                    nested,
                })
            }
            SchemaType::Object(object) => {
                let struct_name = self.nested_struct_name(parent_struct, field_name);
                let nested = self.render_struct(&struct_name, &object.fields, depth)?;

                Ok(TypeResolution {
                    type_name: struct_name.clone(),
                    nested: vec![nested],
                })
            }
        }
    }

    fn nested_struct_name(&self, parent_struct: &str, field_name: &str) -> String {
        let mut name = to_pascal_case(field_name);
        if name.is_empty() {
            name = format!("{parent_struct}Field");
        }

        if name.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
            name.insert(0, '_');
        }

        name
    }

    fn capnp_type_for_token(&mut self, token: &TypeToken) -> &'static str {
        match token {
            TypeToken::Bool => "Bool",
            TypeToken::String => "Text",
            TypeToken::Bytes => "Data",
            TypeToken::Time => {
                self.timestamp_struct_needed = true;
                "Timestamp"
            }
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
}

struct TypeResolution {
    type_name: String,
    nested: Vec<String>,
}

fn sanitize_field_name(input: &str) -> String {
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

/// Returns both the canonical Cap'n Proto discriminant string and the Rust-facing type string
/// for a given `TypeToken`. The first element is used when hashing schema identifiers, while the
/// second drives the Rust type mapping overrides.
fn type_token_strings(token: &TypeToken) -> (&'static str, &'static str) {
    match token {
        TypeToken::Bool => ("bool", "bool"),
        TypeToken::String => ("string", "String"),
        TypeToken::Bytes => ("bytes", "Vec<u8>"),
        TypeToken::Time => ("timestamp", "std::time::SystemTime"),
        TypeToken::U8 => ("u8", "u8"),
        TypeToken::U16 => ("u16", "u16"),
        TypeToken::U32 => ("u32", "u32"),
        TypeToken::U64 => ("u64", "u64"),
        TypeToken::I8 => ("i8", "i8"),
        TypeToken::I16 => ("i16", "i16"),
        TypeToken::I32 => ("i32", "i32"),
        TypeToken::I64 => ("i64", "i64"),
        TypeToken::F32 => ("f32", "f32"),
        TypeToken::F64 => ("f64", "f64"),
    }
}

fn find_root_struct_id(ctx: &GeneratorContext<'_>) -> Result<u64> {
    let requested_files = ctx.request.get_requested_files()?;
    if requested_files.is_empty() {
        return Err(Error::Encoding(
            "capnp request did not include any files".to_string(),
        ));
    }

    let file_id = requested_files.get(0).get_id();

    for (id, node_reader) in &ctx.node_map {
        if node_reader.get_scope_id() != file_id {
            continue;
        }

        if let Ok(node::Struct(_)) = node_reader.which() {
            let display_name = node_reader
                .get_display_name()?
                .to_str()
                .map_err(|err| Error::Encoding(err.to_string()))?;
            let simple_name = display_name
                .rsplit(|ch| [':', '.'].contains(&ch))
                .next()
                .unwrap_or(display_name);
            if simple_name == "Message" {
                return Ok(*id);
            }
        }
    }

    Err(Error::Encoding(
        "capnp request missing root Message struct".to_string(),
    ))
}

fn collect_struct_fields(
    ctx: &GeneratorContext<'_>,
    struct_id: u64,
    prefix: &str,
    mapping: &mut HashMap<String, String>,
    visited: &mut HashSet<u64>,
) -> Result<()> {
    if !visited.insert(struct_id) {
        return Ok(());
    }

    let node_reader = ctx.node_map.get(&struct_id).ok_or_else(|| {
        Error::Encoding(format!(
            "capnp request missing node definition for struct id {struct_id:#x}"
        ))
    })?;

    let struct_reader = match node_reader
        .which()
        .map_err(|err| Error::Encoding(format!("failed to inspect node {struct_id:#x}: {err}")))?
    {
        node::Struct(struct_reader) => struct_reader,
        _ => {
            return Err(Error::Encoding(format!(
                "node {struct_id:#x} is not a struct"
            )));
        }
    };

    for field in struct_reader.get_fields()? {
        let name = field
            .get_name()?
            .to_string()
            .map_err(|err| Error::Encoding(format!("invalid field name encoding: {err}")))?;
        let key = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };

        let field_type = match field
            .which()
            .map_err(|err| Error::Encoding(format!("failed to inspect field `{key}`: {err}")))?
        {
            field::Slot(slot) => slot.get_type()?,
            field::Group(_) => {
                return Err(Error::Encoding(format!(
                    "group fields are not supported for field `{key}`"
                )));
            }
        };
        let rust_type = field_type
            .type_string(ctx, Leaf::Reader("'a"))
            .map_err(|err| {
                Error::Encoding(format!("failed to render type for field `{key}`: {err}"))
            })?;
        mapping.insert(key.clone(), rust_type);

        match field_type
            .which()
            .map_err(|err| Error::Encoding(format!("failed to classify field `{key}`: {err}")))?
        {
            type_::Struct(struct_type) => {
                collect_struct_fields(ctx, struct_type.get_type_id(), &key, mapping, visited)?;
            }
            type_::List(list_reader) => {
                collect_list_type(ctx, &key, list_reader, mapping, visited)?;
            }
            _ => {}
        }
    }

    visited.remove(&struct_id);
    Ok(())
}

fn collect_list_type(
    ctx: &GeneratorContext<'_>,
    prefix: &str,
    list_reader: type_::list::Reader<'_>,
    mapping: &mut HashMap<String, String>,
    visited: &mut HashSet<u64>,
) -> Result<()> {
    let element_type = list_reader.get_element_type()?;
    let element_key = format!("{prefix}[]");

    let element_rust_type = element_type
        .type_string(ctx, Leaf::Reader("'a"))
        .map_err(|err| {
            Error::Encoding(format!(
                "failed to render list element type for `{element_key}`: {err}"
            ))
        })?;
    mapping.insert(element_key.clone(), element_rust_type);

    match element_type.which().map_err(|err| {
        Error::Encoding(format!(
            "failed to classify list element `{element_key}`: {err}"
        ))
    })? {
        type_::Struct(struct_type) => {
            collect_struct_fields(
                ctx,
                struct_type.get_type_id(),
                &element_key,
                mapping,
                visited,
            )?;
        }
        type_::List(nested) => {
            collect_list_type(ctx, &element_key, nested, mapping, visited)?;
        }
        _ => {}
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::MessageFormat;
    use proc_macro2::TokenStream;

    fn canonicalize_tokens(tokens: &TokenStream) -> String {
        tokens
            .to_string()
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect()
    }

    fn canonicalize_literal(value: &str) -> String {
        value.chars().filter(|ch| !ch.is_whitespace()).collect()
    }

    fn params_to_map(params: &[FunctionParam]) -> std::collections::HashMap<String, String> {
        params
            .iter()
            .map(|param| (param.ident().to_string(), canonicalize_tokens(param.ty())))
            .collect()
    }

    #[test]
    fn test_map_message_format_to_capnpn_proto() {
        let msg_format: MessageFormat = serde_json5::from_str(
            r#"
            {
              header: {
                $type: "object",
                stamp: "time",
                frame_id: "u32",
              },
              encoding: "string", // "rgb8", "bgr8", "yuyv", "mjpeg"
              width: "u32",
              height: "u32",
              image: {
                $type: "array",
                $items: "u8",
                $length: 3
              }
            }
            "#,
        )
        .expect("valid format");

        let format_mapper = MessageFormatMapper::new("test_image", msg_format.clone());
        let artifacts = format_mapper
            .map_message_format_to_capnpn()
            .expect("artifacts generation succeeds");
        let schema = artifacts.encoding_schema();
        let params = artifacts
            .build_function_params()
            .expect("params generation succeeds");

        assert_eq!(params.len(), 5, "expected entries for top-level fields");

        let param_map = params_to_map(&params);
        assert!(
            param_map
                .get("encoding")
                .expect("encoding entry missing")
                .ends_with("text::Reader<'a>"),
            "encoding should map to a capnp text reader",
        );
        assert_eq!(
            param_map.get("width"),
            Some(&canonicalize_literal("u32")),
            "width should map to u32"
        );
        assert_eq!(
            param_map.get("height"),
            Some(&canonicalize_literal("u32")),
            "height should map to u32"
        );
        assert_eq!(
            param_map.get("image"),
            Some(&canonicalize_literal("[u8; 3]")),
            "image should map to a fixed-size array"
        );
        assert!(
            param_map
                .get("header")
                .expect("header entry missing")
                .ends_with("header::Reader<'a>"),
            "header should resolve to the generated header reader type"
        );

        let canonical_type_map = artifacts
            .type_mapping()
            .iter()
            .map(|(key, value)| (key.clone(), canonicalize_literal(value)))
            .collect::<std::collections::HashMap<_, _>>();

        assert_eq!(
            canonical_type_map.len(),
            9,
            "expected entries for nested fields"
        );
        assert_eq!(
            canonical_type_map.get("header.frameId"),
            Some(&canonicalize_literal("u32")),
            "header.frameId should map to u32"
        );
        assert!(
            canonical_type_map
                .get("header.stamp")
                .expect("header.stamp entry missing")
                .ends_with("timestamp::Reader<'a>"),
            "header.stamp should map to the generated timestamp reader type"
        );
        assert_eq!(
            canonical_type_map.get("header.stamp.sec"),
            Some(&canonicalize_literal("i64")),
            "header.stamp.sec should map to i64"
        );
        assert_eq!(
            canonical_type_map.get("header.stamp.nsec"),
            Some(&canonicalize_literal("u32")),
            "header.stamp.nsec should map to u32"
        );

        assert!(
            schema.starts_with("@0x"),
            "schema should start with a capnp file id, got {schema:?}"
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
                schema.contains(expected),
                "schema missing expected segment {expected:?}.\nSchema:\n{schema}"
            );
        }
    }

    #[test]
    fn test_map_message_format_to_capnpn_proto_variable_array() {
        let msg_format: MessageFormat = serde_json5::from_str(
            r#"
            {
              image: {
                $type: "array",
                $items: "u8"
              }
            }
            "#,
        )
        .expect("valid format");

        let artifacts = MessageFormatMapper::new("test_variable_array", msg_format)
            .map_message_format_to_capnpn()
            .expect("artifacts generation succeeds");
        let params = artifacts
            .build_function_params()
            .expect("params generation succeeds");

        let param_map = params_to_map(&params);
        assert_eq!(
            param_map.get("image"),
            Some(&canonicalize_literal("[u8]")),
            "image should map to a dynamically sized array"
        );
    }

    #[test]
    fn test_map_message_format_to_capnpn_proto_string_array() {
        let msg_format: MessageFormat = serde_json5::from_str(
            r#"
            {
              labels: {
                $type: "array",
                $items: "string",
                $length: 3
              }
            }
            "#,
        )
        .expect("valid format");

        let artifacts = MessageFormatMapper::new("test_string_array", msg_format)
            .map_message_format_to_capnpn()
            .expect("artifacts generation succeeds");
        let params = artifacts
            .build_function_params()
            .expect("params generation succeeds");

        let param_map = params_to_map(&params);
        assert_eq!(
            param_map.get("labels"),
            Some(&canonicalize_literal("[String; 3]")),
            "labels should map to a fixed-size array of Strings"
        );
    }

    #[test]
    fn different_schema_names_produce_different_ids() {
        let format: MessageFormat = serde_json5::from_str(
            r#"
            {
              arm_id: "u16",
              desired_position: {
                $type: "array",
                $items: "i32"
              }
            }
            "#,
        )
        .expect("valid format");

        let schema_a = MessageFormatMapper::new("move_right_arm_goal_request", format.clone())
            .map_message_format_to_capnpn()
            .expect("schema a");
        let schema_b = MessageFormatMapper::new("move_left_arm_goal_request", format)
            .map_message_format_to_capnpn()
            .expect("schema b");

        let id_a = schema_a
            .encoding_schema()
            .lines()
            .next()
            .expect("first line");
        let id_b = schema_b
            .encoding_schema()
            .lines()
            .next()
            .expect("first line");

        assert_ne!(
            id_a, id_b,
            "identical field structures with different names must produce different schema IDs"
        );
    }

    #[test]
    fn same_name_and_fields_produce_same_id() {
        let format: MessageFormat = serde_json5::from_str(
            r#"
            {
              arm_id: "u16",
              desired_position: {
                $type: "array",
                $items: "i32"
              }
            }
            "#,
        )
        .expect("valid format");

        let schema_a = MessageFormatMapper::new("move_arm_goal_request", format.clone())
            .map_message_format_to_capnpn()
            .expect("schema a");
        let schema_b = MessageFormatMapper::new("move_arm_goal_request", format)
            .map_message_format_to_capnpn()
            .expect("schema b");

        let id_a = schema_a
            .encoding_schema()
            .lines()
            .next()
            .expect("first line");
        let id_b = schema_b
            .encoding_schema()
            .lines()
            .next()
            .expect("first line");

        assert_eq!(
            id_a, id_b,
            "same name and fields must produce identical schema IDs"
        );
    }

    #[test]
    fn test_compile_capnp_schema() {
        let schema_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("schemas")
            .join("frame.capnp");

        let temp_dir = tempfile::tempdir().expect("Failed to create temp directory");
        let output_dir = temp_dir.path();

        assert!(
            schema_path.exists(),
            "Schema file should exist at {:?}",
            schema_path
        );

        compile_capnp(&[schema_path], output_dir).expect("capnp compilation succeeds");

        let expected_output = output_dir.join("capnp").join("frame_capnp.rs");
        assert!(
            expected_output.exists(),
            "Compiled output file should exist at {:?}",
            expected_output
        );

        let generated_content =
            std::fs::read_to_string(&expected_output).expect("Failed to read generated file");

        // Check that crate::capnp::frame_capnp:: is used for nested types
        assert!(
            generated_content.contains("crate::capnp::frame_capnp::"),
            "Generated code should use 'crate::capnp::frame_capnp::' for nested types"
        );

        let capnp_module_file = output_dir.join("capnp.rs");
        assert!(
            capnp_module_file.exists(),
            "capnp.rs module file should exist at {:?}",
            capnp_module_file
        );

        let capnp_module_content =
            std::fs::read_to_string(&capnp_module_file).expect("Failed to read capnp.rs file");

        assert!(
            capnp_module_content.contains("pub mod frame_capnp;"),
            "capnp.rs should contain 'pub mod frame_capnp;'"
        );
    }

    #[test]
    fn test_override_array_types_for_nested_object_arrays() {
        let msg_format: MessageFormat = serde_json5::from_str(
            r#"
            {
              points: {
                $type: "array",
                $items: {
                  $type: "object",
                  coords: {
                    $type: "array",
                    $items: "f32",
                    $length: 3
                  },
                  label: "string"
                }
              }
            }
            "#,
        )
        .expect("valid format");

        let artifacts = MessageFormatMapper::new("test_nested_object_array", msg_format)
            .map_message_format_to_capnpn()
            .expect("artifacts generation succeeds");

        let canonical_type_map = artifacts
            .type_mapping()
            .iter()
            .map(|(key, value)| (key.clone(), canonicalize_literal(value)))
            .collect::<std::collections::HashMap<_, _>>();

        assert_eq!(
            canonical_type_map.get("points[].coords"),
            Some(&canonicalize_literal("[f32; 3]")),
            "coords inside array-of-objects should be overridden to a fixed-size array via the [] key path"
        );
    }
}
