//! Rust code generation for the Cap'n Proto schemas of Peppy node message
//! formats.
//!
//! [`MessageFormatMapper`] maps a [`config::node::MessageFormat`] to its
//! schema artifacts: the schema text that `peppy-codec` renders, and the
//! Rust type of each field, which this crate reads from the compile of the
//! schema. [`compile_capnp`] compiles schema files into the Rust modules of
//! a generated crate. Both run the Cap'n Proto compiler of `peppy-codec`
//! (see [`peppy_codec::compiler`]). The crate lives outside `peppy-shared`
//! because only the code generator of peppy depends on it.

mod types;

pub use types::FunctionParam;

pub(crate) use config::ConfigError as Error;
/// Crate-wide result type, propagating [`config::ConfigError`] so callers in
/// `peppy` keep their existing `ConfigError`-based error handling.
pub type Result<T> = std::result::Result<T, config::ConfigError>;

use capnp::schema_capnp::{field, node, type_};
use capnpc::CompilerCommand;
use capnpc::codegen::GeneratorContext;
use capnpc::codegen_types::{Leaf, RustTypeInfo};
use proc_macro2::{Ident, Span, TokenStream};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;

use config::node::{ArraySchema, MessageFormat, SchemaType, TypeToken};
use peppy_codec::schema::{MessageSchema, SchemaError, capnp_field_name, join_path};

/// A failure of the schema functions of `peppy-codec`, as an error of this
/// crate.
fn schema_error(error: impl Into<SchemaError>) -> Error {
    Error::Encoding(error.into().to_string())
}

/// Compiles the Cap'n Proto schemas `capnp_files` into Rust modules.
/// `output_dir` is the `src` directory of a Rust crate: the call writes a
/// `capnp.rs` module that declares every generated module, and a `capnp/`
/// directory with the generated files.
pub fn compile_capnp<P, O>(capnp_files: &[P], output_dir: O) -> Result<()>
where
    P: AsRef<Path>,
    O: AsRef<Path>,
{
    let capnp = peppy_codec::compiler::binary_path().map_err(schema_error)?;
    let output_dir = output_dir.as_ref();
    let capnp_output_dir = output_dir.join("capnp");
    std::fs::create_dir_all(&capnp_output_dir)?;

    let mut module_exports: Vec<String> = Vec::with_capacity(capnp_files.len());
    for capnp_file in capnp_files {
        let capnp_path = capnp_file.as_ref();

        let mut command = CompilerCommand::new();
        command.capnp_executable(&capnp);
        command.output_path(&capnp_output_dir);
        command.default_parent_module(vec!["capnp".to_string()]);

        if let Some(parent) = capnp_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            command.src_prefix(parent);
        }

        command.file(capnp_path);

        command
            .run()
            .map_err(|err| Error::Encoding(format!("failed to run capnp compiler: {err}")))?;

        if let Some(module_name) = capnp_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(|name| format!("pub mod {}_capnp;", name))
        {
            module_exports.push(module_name);
        }
    }

    module_exports.sort();
    module_exports.dedup();

    let capnp_rs_path = output_dir.join("capnp.rs");
    let capnp_rs_content = module_exports.join("\n") + "\n";
    std::fs::write(&capnp_rs_path, capnp_rs_content)?;
    Ok(())
}

/// This struct turns a MessageFormat into a capnp proto and its associated Rust types
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
    /// Rust type mapping runs the Cap'n Proto compiler of `peppy-codec` on the
    /// schema (see [`peppy_codec::compiler`]) and reads back its code
    /// generator request. Expect filesystem I/O and a subprocess, not just
    /// in-memory work.
    pub fn map_message_format_to_capnpn(&self) -> Result<CapnpSchemaArtifacts> {
        let schema = MessageSchema::render(&self.schema_name, &self.message_format);
        let type_mapping = self.compute_rust_type_mapping(&schema)?;
        Ok(CapnpSchemaArtifacts::new(
            self.message_format.clone(),
            schema.text().to_string(),
            type_mapping,
        ))
    }

    fn compute_rust_type_mapping(&self, schema: &MessageSchema) -> Result<HashMap<String, String>> {
        let compiled = schema.compile().map_err(schema_error)?;
        let ctx = GeneratorContext::new(compiled.message())?;

        let root_struct_id = compiled.root_message_id().map_err(schema_error)?;
        let mut mapping = HashMap::new();
        let mut visited = HashSet::new();
        collect_struct_fields(&ctx, root_struct_id, "", &mut mapping, &mut visited)?;

        fn array_schema_to_rust_string(array: &ArraySchema) -> Option<String> {
            let token = array.items.as_ref().as_type_token()?;
            let rust_type = rust_type_name(token);
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
                        let element_key = format!("{current_key}[]");
                        for (field_name, field_schema) in &object.fields {
                            let sanitized = capnp_field_name(field_name);
                            let next_key = join_path(&element_key, &sanitized);
                            override_array_types(&next_key, field_schema, mapping);
                        }
                    }
                }
                SchemaType::Object(object) => {
                    for (field_name, field_schema) in &object.fields {
                        let sanitized = capnp_field_name(field_name);
                        let next_key = join_path(current_key, &sanitized);
                        override_array_types(&next_key, field_schema, mapping);
                    }
                }
                SchemaType::Type(_) | SchemaType::Primitive(_) => {}
            }
        }

        for (field_name, schema) in &self.message_format.0 {
            let sanitized = capnp_field_name(field_name);
            override_array_types(&sanitized, schema, &mut mapping);
        }

        Ok(mapping)
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
            let sanitized = capnp_field_name(field_name);
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

/// The Rust type of a token in the generated function parameters.
fn rust_type_name(token: &TypeToken) -> &'static str {
    match token {
        TypeToken::Bool => "bool",
        TypeToken::String => "String",
        TypeToken::Bytes => "Vec<u8>",
        TypeToken::Time => "std::time::SystemTime",
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
        let key = join_path(prefix, &name);

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
    use config::node::MessageFormat;
    use proc_macro2::TokenStream;
    use std::path::PathBuf;

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
            .map(|param| (param.ident.to_string(), canonicalize_tokens(&param.ty)))
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
