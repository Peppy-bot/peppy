#[cfg(test)]
mod frame_capnp;

use std::fmt::Write;
use std::path::Path;

use crate::node::{MessageFormat, SchemaType, TypeToken};

/// The output_dir should point to the `src` of a Rust crate. A new `capnp` module will be
/// created at the root of this directory with all the `capnp` files.
pub fn compile_capnp(capnp_files: &[impl AsRef<Path>], output_dir: impl AsRef<Path>) {
    let output_dir = output_dir.as_ref().to_path_buf();

    // Create capnp subdirectory
    let capnp_output_dir = output_dir.join("capnp");
    std::fs::create_dir_all(&capnp_output_dir).expect("Failed to create capnp output directory");

    let capnp_executable = {
        let binary_name = match std::env::consts::OS {
            "linux" if std::env::consts::ARCH == "x86_64" => "capnp_linux_x86_64",
            "macos" if std::env::consts::ARCH == "aarch64" => "capnp_macos_aarch64",
            _ => panic!(
                "unsupported platform: {}-{}",
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
        };

        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tools")
            .join(binary_name)
    };

    let mut command = capnpc::CompilerCommand::new();
    command.capnp_executable(capnp_executable);
    command.output_path(&capnp_output_dir);

    // Set the default parent module to "capnp" so generated code references
    // crate::capnp::module_name instead of crate::module_name
    command.default_parent_module(vec!["capnp".to_string()]);

    // Determine common src_prefix if all files share a parent directory
    let common_parent = capnp_files
        .first()
        .and_then(|f| f.as_ref().parent())
        .filter(|p| !p.as_os_str().is_empty());

    if let Some(parent) = common_parent {
        command.src_prefix(parent);
    }

    // Add all files to the command
    for capnp_file in capnp_files {
        command.file(capnp_file.as_ref());
    }

    command.run().expect("capnp schema compilation failed");

    // Create capnp.rs module file that exports all generated modules
    let module_exports: Vec<String> = capnp_files
        .iter()
        .filter_map(|file| {
            let path = file.as_ref();
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(|name| format!("pub mod {}_capnp;", name))
        })
        .collect();

    let capnp_rs_path = output_dir.join("capnp.rs");
    let capnp_rs_content = module_exports.join("\n") + "\n";
    std::fs::write(&capnp_rs_path, capnp_rs_content).expect("Failed to write capnp.rs module file");
}

/// Given a textual type descriptor from a node message format, return the corresponding Cap'n Proto type.
pub fn map_node_type_to_capnp_proto(node_type: &str) -> String {
    match node_type.to_ascii_lowercase().as_str() {
        "bool" => "Bool".to_string(),
        "string" | "str" => "Text".to_string(),
        "bytes" => "Data".to_string(),
        "time" => "Timestamp".to_string(),
        "u8" => "UInt8".to_string(),
        "u16" => "UInt16".to_string(),
        "u32" => "UInt32".to_string(),
        "u64" => "UInt64".to_string(),
        "i8" => "Int8".to_string(),
        "i16" => "Int16".to_string(),
        "i32" => "Int32".to_string(),
        "i64" => "Int64".to_string(),
        "f32" => "Float32".to_string(),
        "f64" => "Float64".to_string(),
        other => other.to_string(),
    }
}

pub fn map_message_format_to_capnpn_proto(message_format: MessageFormat) -> String {
    let mut generator = CapnpSchemaGenerator::default();
    let mut schema = String::new();
    let schema_id = compute_schema_id(&message_format.0).max(1);

    writeln!(&mut schema, "@0x{schema_id:016x};").expect("writing schema id should not fail");
    schema.push('\n');
    schema.push_str(&generator.render_struct("Message", &message_format.0, 0));

    if generator.timestamp_struct_needed {
        schema.push('\n');
        schema.push_str("struct Timestamp {\n  sec @0 :Int64;\n  nsec @1 :UInt32;\n}\n");
    }

    schema
}

#[derive(Default)]
struct CapnpSchemaGenerator {
    timestamp_struct_needed: bool,
}

impl CapnpSchemaGenerator {
    fn render_struct(
        &mut self,
        struct_name: &str,
        fields: &std::collections::BTreeMap<String, SchemaType>,
        depth: usize,
    ) -> String {
        let indent = "  ".repeat(depth);
        let mut buffer = String::new();
        writeln!(&mut buffer, "{indent}struct {struct_name} {{")
            .expect("writing struct header should not fail");

        let mut nested_defs = Vec::new();

        for (index, (field_name, schema_type)) in fields.iter().enumerate() {
            let sanitized_field = sanitize_field_name(field_name);
            let field_indent = "  ".repeat(depth + 1);
            let TypeResolution { type_name, nested } =
                self.resolve_type(struct_name, &sanitized_field, schema_type, depth + 1);

            writeln!(
                &mut buffer,
                "{field_indent}{sanitized_field} @{index} :{type_name};"
            )
            .expect("writing field should not fail");

            nested_defs.extend(nested);
        }

        if !nested_defs.is_empty() {
            buffer.push('\n');
            for nested in nested_defs {
                buffer.push_str(&nested);
            }
        }

        writeln!(&mut buffer, "{indent}}}").expect("writing struct closing brace should not fail");
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
            SchemaType::Type(token) => TypeResolution {
                type_name: self.capnp_type_for_token(token).to_string(),
                nested: Vec::new(),
            },
            SchemaType::Array(array) => {
                if matches!(array.items.as_ref(), SchemaType::Type(TypeToken::U8)) {
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

                TypeResolution {
                    type_name: format!("List({})", item_resolution.type_name),
                    nested: std::mem::take(&mut item_resolution.nested),
                }
            }
            SchemaType::Object(object) => {
                let struct_name = self.nested_struct_name(parent_struct, field_name);
                let nested = self.render_struct(&struct_name, &object.fields, depth);

                TypeResolution {
                    type_name: struct_name,
                    nested: vec![nested],
                }
            }
        }
    }

    fn nested_struct_name(&self, parent_struct: &str, field_name: &str) -> String {
        let mut name = to_pascal_case(field_name);
        if name.is_empty() {
            name = format!("{parent_struct}Field");
        }

        if name.chars().next().map_or(false, |ch| ch.is_ascii_digit()) {
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
    let mut output = String::with_capacity(input.len());

    for (idx, ch) in input.chars().enumerate() {
        let replacement = match ch {
            'a'..='z' | '0'..='9' => Some(ch),
            'A'..='Z' => Some(ch.to_ascii_lowercase()),
            '_' => Some('_'),
            _ => None,
        };

        if let Some(char) = replacement {
            if idx == 0 && char.is_ascii_digit() {
                output.push('_');
            }
            output.push(char);
        } else {
            output.push('_');
        }
    }

    if output.is_empty() {
        "_field".to_string()
    } else {
        output
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

fn compute_schema_id(fields: &std::collections::BTreeMap<String, SchemaType>) -> u64 {
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
                hash = update(hash, capnp_token_discriminant(token).as_bytes(), prime);
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

    fn hash_fields(
        mut hash: u64,
        fields: &std::collections::BTreeMap<String, SchemaType>,
        prime: u64,
    ) -> u64 {
        for (key, value) in fields {
            hash = update(hash, key.as_bytes(), prime);
            hash = hash_schema(hash, value, prime);
        }
        hash
    }

    hash_fields(FNV_OFFSET, fields, FNV_PRIME)
}

fn capnp_token_discriminant(token: &TypeToken) -> &'static str {
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

// TODO 1: Convert the json5 types representation to a capn proto file
// TODO 2: Compile that file to a rust

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::frame_capnp::image_message;
    use crate::node::MessageFormat;

    #[test]
    fn test_map_message_format_to_capnpn_proto() {
        let msg_format: MessageFormat = serde_json5::from_str(
            r#"
            {
              header: {
                type: "object",
                stamp: "time",
                frame_id: "u32",
              },
              encoding: "string", // "rgb8", "bgr8", "yuyv", "mjpeg"
              width: "u32",
              height: "u32",
              image: {
                type: "array",
                items: "u8",
                length: 3
              }
            }
            "#,
        )
        .expect("valid format");

        let schema = map_message_format_to_capnpn_proto(msg_format);

        assert!(
            schema.starts_with("@0x"),
            "schema should start with a capnp file id, got {schema:?}"
        );
        for expected in [
            "struct Message {",
            "  encoding @0 :Text;",
            "  header @1 :Header;",
            "  height @2 :UInt32;",
            "  image @3 :Data;",
            "  width @4 :UInt32;",
            "  struct Header {",
            "    frame_id @0 :UInt32;",
            "    stamp @1 :Timestamp;",
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
    fn test_compile_capnp_schema() {
        let schema_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("schemas")
            .join("frame.capnp");

        let temp_dir = tempfile::tempdir().expect("Failed to create temp directory");
        let output_dir = temp_dir.path();

        assert!(
            schema_path.exists(),
            "Schema file should exist at {:?}",
            schema_path
        );

        compile_capnp(&[schema_path], output_dir);

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
    fn test_use_compiled_schema() {
        // Create a message builder
        let mut message = capnp::message::Builder::new_default();
        let mut img_msg = message.init_root::<image_message::Builder>();

        // Set some fields
        img_msg.set_width(1920);
        img_msg.set_height(1080);
        img_msg.set_encoding("rgb8");

        // Populate the header struct to ensure nested structs can be set
        let mut header = img_msg.reborrow().init_header();
        header.set_frame_id(42);
        let mut stamp = header.reborrow().init_stamp();
        stamp.set_sec(1_234);
        stamp.set_nsec(567);

        // Verify we can read it back
        let reader = img_msg.reborrow_as_reader();
        assert_eq!(reader.get_width(), 1920);
        assert_eq!(reader.get_height(), 1080);
        let header_reader = reader
            .get_header()
            .expect("header should be present after initialization");
        assert_eq!(header_reader.get_frame_id(), 42);
        let stamp_reader = header_reader
            .get_stamp()
            .expect("stamp should be present after initialization");
        assert_eq!(stamp_reader.get_sec(), 1_234);
        assert_eq!(stamp_reader.get_nsec(), 567);
    }

    #[test]
    fn test_extract_compiled_schema_types() {
        use capnp::introspect::{Introspect, TypeVariant};
        use capnp::schema::StructSchema;

        let schema = match <image_message::Owned as Introspect>::introspect().which() {
            TypeVariant::Struct(raw) => StructSchema::new(raw),
            _ => panic!("image_message should be a struct"),
        };

        let field_types: Vec<(String, String)> = schema
            .get_fields()
            .expect("failed to read field list")
            .into_iter()
            .map(|field| {
                let name = field
                    .get_proto()
                    .get_name()
                    .expect("field has a name")
                    .to_string()
                    .expect("field name is utf-8");
                let ty = match field.get_type().which() {
                    TypeVariant::Struct(inner) => {
                        let schema = StructSchema::new(inner);
                        let mut display = schema
                            .get_proto()
                            .get_display_name()
                            .expect("struct field has a display name")
                            .to_string()
                            .expect("struct field display name is utf-8");
                        let prefix_len =
                            schema.get_proto().get_display_name_prefix_length() as usize;
                        if prefix_len <= display.len() {
                            display.replace_range(..prefix_len, "");
                        }
                        display
                    }
                    TypeVariant::Text => String::from("Text"),
                    TypeVariant::UInt32 => String::from("UInt32"),
                    TypeVariant::Data => String::from("Data"),
                    _ => panic!("unexpected type for field {name}"),
                };
                (name, ty)
            })
            .collect();

        println!("image_message field types: {field_types:?}");

        assert_eq!(
            field_types,
            vec![
                ("header".to_string(), "Header".to_string()),
                ("encoding".to_string(), "Text".to_string()),
                ("width".to_string(), "UInt32".to_string()),
                ("height".to_string(), "UInt32".to_string()),
                ("image".to_string(), "Data".to_string()),
            ]
        );
    }

    #[test]
    fn test_extract_field_types_from_generated_rust() {
        let schema_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("schemas")
            .join("frame.capnp");
        // TODO: test extract_field_types_from_generated_rust
    }
}
