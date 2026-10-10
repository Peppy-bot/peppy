use super::PythonSchemaInfo;
use super::code_builder::{
    PythonCodeBuilder, container_name, emit_fixed_length_check, next_temp_index,
};
use super::identifiers::{is_python_keyword, sanitize_python_identifier};
use super::topics::capnp_loader_fn_name;
use crate::generator::naming::{array_item_type_name, nested_type_name, sanitize_capnp_field_name};
use config::node::{MessageFormat, SchemaType, TypeToken};
use indexmap::IndexMap;

fn capnp_read_expr(reader_var: &str, capnp_name: &str) -> String {
    if is_python_keyword(capnp_name) {
        format!("getattr({reader_var}, \"{capnp_name}\")")
    } else {
        format!("{reader_var}.{capnp_name}")
    }
}

/// Generates the reader statements of one field, wrapping the read in a
/// presence check when the field is optional.
///
/// `reader_var` is the pycapnp reader variable (e.g., `"capnp_msg"` or `"reader_0"`).
/// Returns the variable name holding the deserialized value.
pub fn generate_field_reader_statements(
    builder: &mut PythonCodeBuilder,
    reader_var: &str,
    field_name: &str,
    schema: &SchemaType,
    struct_prefix: &str,
    counter: &mut u32,
) -> String {
    if !schema.is_optional() {
        return read_field_value(
            builder,
            reader_var,
            field_name,
            schema,
            struct_prefix,
            counter,
        );
    }

    // An absent optional field is not read at all. Reading it first and
    // overriding afterwards decodes what Cap'n Proto returns for a field that
    // was never set, and a fixed-length array inside such a field is an empty
    // list, which fails its length check before the presence check is reached.
    let capnp_name = sanitize_capnp_field_name(field_name);
    let var = builder.block(
        &format!("if {reader_var}._has(\"{capnp_name}\"):"),
        |builder| {
            read_field_value(
                builder,
                reader_var,
                field_name,
                schema,
                struct_prefix,
                counter,
            )
        },
    );
    builder.block("else:", |builder| builder.line(&format!("{var} = None")));
    var
}

/// Reads one present field, dispatching on the schema type. Returns the
/// variable name holding the deserialized value.
fn read_field_value(
    builder: &mut PythonCodeBuilder,
    reader_var: &str,
    field_name: &str,
    schema: &SchemaType,
    struct_prefix: &str,
    counter: &mut u32,
) -> String {
    match schema {
        SchemaType::Type(TypeToken::Time) => {
            generate_time_reader(builder, reader_var, field_name, counter)
        }
        SchemaType::Primitive(prim) if prim.kind == TypeToken::Time => {
            generate_time_reader(builder, reader_var, field_name, counter)
        }
        SchemaType::Type(_) | SchemaType::Primitive(_) => {
            generate_primitive_reader(builder, reader_var, field_name, counter)
        }
        SchemaType::Array(array) => {
            let items = array.items.as_ref();
            let var = match items {
                SchemaType::Object(object) => generate_object_array_reader(
                    builder,
                    reader_var,
                    field_name,
                    &object.fields,
                    struct_prefix,
                    counter,
                ),
                _ => generate_array_reader(
                    builder,
                    reader_var,
                    field_name,
                    container_name(items),
                    counter,
                ),
            };
            // A fixed-length array is checked against its declared length
            // once, whatever its items are, so a payload carrying any other
            // count fails to decode the way it does in Rust, where the field
            // is a fixed-size array.
            if let Some(len) = array.length {
                emit_fixed_length_check(builder, &var, field_name, container_name(items), len);
            }
            var
        }
        SchemaType::Object(object) => generate_object_reader(
            builder,
            reader_var,
            field_name,
            &object.fields,
            struct_prefix,
            counter,
        ),
    }
}

/// Generates a complete `deserialize_payload` helper function that reads a Cap'n Proto
/// payload and constructs a `Message` dataclass.
///
/// Emits:
/// ```python
/// def deserialize_payload(payload):
///     with _<file_stem>_capnp().StructName.from_bytes(payload) as capnp_msg:
///         ...field reads...
///         return Message(field1=var1, ...)
/// ```
pub fn build_deserialize_fn(
    builder: &mut PythonCodeBuilder,
    schema_info: &PythonSchemaInfo,
    format: &MessageFormat,
    struct_prefix: &str,
    fn_name: &str,
) {
    let loader_fn_name = capnp_loader_fn_name(schema_info);
    let with_header = format!(
        "with {loader_fn_name}().{}.from_bytes(payload) as capnp_msg:",
        schema_info.struct_name
    );
    builder.blank_line();
    builder.block(
        &format!("def {fn_name}(payload: bytes) -> {struct_prefix}:"),
        |builder| {
            builder.block(&with_header, |builder| {
                let field_bindings =
                    deserialize_format_fields(builder, "capnp_msg", format, struct_prefix);

                let kwargs_str = keyword_arguments(&field_bindings);
                builder.line(&format!("return {struct_prefix}({kwargs_str})"));
            });
        },
    );
}

/// The `name=var` keyword arguments of a dataclass constructor call, one per
/// `(python_field_name, variable_name)` binding.
fn keyword_arguments(bindings: &[(String, String)]) -> String {
    bindings
        .iter()
        .map(|(name, var)| format!("{name}={var}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Deserializes all fields from a `MessageFormat`, iterating directly over format fields.
///
/// Returns a list of `(python_field_name, variable_name)` pairs for constructing a dataclass.
pub fn deserialize_format_fields(
    builder: &mut PythonCodeBuilder,
    reader_var: &str,
    format: &MessageFormat,
    struct_prefix: &str,
) -> Vec<(String, String)> {
    let mut counter = 0u32;
    read_struct_fields(builder, reader_var, &format.0, struct_prefix, &mut counter)
}

/// Reads every field of a struct from `reader_var`, the message itself or a
/// nested struct reader. Returns the `(python_field_name, variable_name)`
/// pairs the struct's dataclass is constructed from.
fn read_struct_fields(
    builder: &mut PythonCodeBuilder,
    reader_var: &str,
    fields: &IndexMap<String, SchemaType>,
    struct_prefix: &str,
    counter: &mut u32,
) -> Vec<(String, String)> {
    let mut field_bindings = Vec::new();
    for (field_name, schema) in fields {
        let var_name = generate_field_reader_statements(
            builder,
            reader_var,
            field_name,
            schema,
            struct_prefix,
            counter,
        );
        field_bindings.push((sanitize_python_identifier(field_name), var_name));
    }
    field_bindings
}

fn generate_primitive_reader(
    builder: &mut PythonCodeBuilder,
    reader_var: &str,
    field_name: &str,
    counter: &mut u32,
) -> String {
    let capnp_name = sanitize_capnp_field_name(field_name);
    let python_name = sanitize_python_identifier(field_name);
    let idx = next_temp_index(counter);
    let var = format!("{python_name}_{idx}");
    builder.line(&format!(
        "{var} = {}",
        capnp_read_expr(reader_var, &capnp_name)
    ));
    var
}

fn generate_time_reader(
    builder: &mut PythonCodeBuilder,
    reader_var: &str,
    field_name: &str,
    counter: &mut u32,
) -> String {
    let capnp_name = sanitize_capnp_field_name(field_name);
    let python_name = sanitize_python_identifier(field_name);
    let ts_idx = next_temp_index(counter);
    let ts_var = format!("timestamp_{ts_idx}");
    let result_idx = next_temp_index(counter);
    let result_var = format!("{python_name}_{result_idx}");
    builder.line(&format!(
        "{ts_var} = {}",
        capnp_read_expr(reader_var, &capnp_name)
    ));
    builder.line(&format!(
        "{result_var} = peppylib.encoding.convert_time_from_capnp({ts_var}.sec, {ts_var}.nsec)"
    ));
    result_var
}

/// Reads an array of primitives into `container` — `bytes` for `u8` items,
/// a `list` otherwise.
fn generate_array_reader(
    builder: &mut PythonCodeBuilder,
    reader_var: &str,
    field_name: &str,
    container: &str,
    counter: &mut u32,
) -> String {
    let capnp_name = sanitize_capnp_field_name(field_name);
    let python_name = sanitize_python_identifier(field_name);
    let idx = next_temp_index(counter);
    let var = format!("{python_name}_{idx}");
    builder.line(&format!(
        "{var} = {container}({})",
        capnp_read_expr(reader_var, &capnp_name)
    ));
    var
}

fn generate_object_reader(
    builder: &mut PythonCodeBuilder,
    reader_var: &str,
    field_name: &str,
    fields: &IndexMap<String, SchemaType>,
    struct_prefix: &str,
    counter: &mut u32,
) -> String {
    let capnp_name = sanitize_capnp_field_name(field_name);
    let python_name = sanitize_python_identifier(field_name);
    let nested_prefix = nested_type_name(struct_prefix, field_name);

    let reader_idx = next_temp_index(counter);
    let sub_reader = format!("reader_{reader_idx}");
    builder.line(&format!(
        "{sub_reader} = {}",
        capnp_read_expr(reader_var, &capnp_name)
    ));

    let nested_bindings = read_struct_fields(builder, &sub_reader, fields, &nested_prefix, counter);

    let result_idx = next_temp_index(counter);
    let result_var = format!("{python_name}_{result_idx}");
    let kwargs_str = keyword_arguments(&nested_bindings);
    builder.line(&format!("{result_var} = {nested_prefix}({kwargs_str})"));
    result_var
}

fn generate_object_array_reader(
    builder: &mut PythonCodeBuilder,
    reader_var: &str,
    field_name: &str,
    fields: &IndexMap<String, SchemaType>,
    struct_prefix: &str,
    counter: &mut u32,
) -> String {
    let capnp_name = sanitize_capnp_field_name(field_name);
    let python_name = sanitize_python_identifier(field_name);

    let nested_prefix = array_item_type_name(struct_prefix, field_name);

    let list_idx = next_temp_index(counter);
    let list_var = format!("list_{list_idx}");
    builder.line(&format!(
        "{list_var} = {}",
        capnp_read_expr(reader_var, &capnp_name)
    ));

    let result_idx = next_temp_index(counter);
    let result_var = format!("{python_name}_{result_idx}");
    builder.line(&format!("{result_var} = []"));

    let elem_idx = next_temp_index(counter);
    let elem_var = format!("elem_{elem_idx}");
    builder.block(&format!("for {elem_var} in {list_var}:"), |builder| {
        let nested_bindings =
            read_struct_fields(builder, &elem_var, fields, &nested_prefix, counter);
        let kwargs_str = keyword_arguments(&nested_bindings);
        builder.line(&format!(
            "{result_var}.append({nested_prefix}({kwargs_str}))"
        ));
    });

    result_var
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::node::{ArrayKind, ArraySchema, ObjectKind, ObjectSchema};

    /// Both readers are driven through the dispatcher, which is where the
    /// length check is emitted, so the tests exercise the path the renderers
    /// actually take.
    fn read_field(field_name: &str, schema: &SchemaType) -> String {
        let mut builder = PythonCodeBuilder::new();
        let mut counter = 0u32;
        generate_field_reader_statements(
            &mut builder,
            "reader",
            field_name,
            schema,
            "Test",
            &mut counter,
        );
        builder.build()
    }

    fn array_of(items: SchemaType, length: Option<usize>) -> SchemaType {
        SchemaType::Array(ArraySchema {
            kind: ArrayKind::Array,
            items: Box::new(items),
            length,
            optional: false,
        })
    }

    fn call_array_reader(is_u8: bool, length: Option<usize>) -> String {
        let item = if is_u8 { TypeToken::U8 } else { TypeToken::F64 };
        read_field("joint_positions", &array_of(SchemaType::Type(item), length))
    }

    #[test]
    fn primitive_array_reader_emits_length_check() {
        let code = call_array_reader(false, Some(7));
        assert!(
            code.contains("joint_positions_0 = list(reader.jointPositions)"),
            "primitive arrays read as lists, got:\n{code}"
        );
        assert!(
            code.contains("if len(joint_positions_0) != 7:")
                && code.contains(
                    "invalid fixed list length for field 'joint_positions': expected 7, got "
                ),
            "fixed-length path must check the declared length, got:\n{code}"
        );
    }

    #[test]
    fn u8_array_reader_emits_bytes_length_check() {
        let code = call_array_reader(true, Some(4));
        assert!(
            code.contains("joint_positions_0 = bytes(reader.jointPositions)")
                && code.contains(
                    "invalid fixed bytes length for field 'joint_positions': expected 4, got "
                ),
            "fixed u8 arrays read as bytes and check their length, got:\n{code}"
        );
    }

    #[test]
    fn primitive_array_reader_no_length_check_when_dynamic() {
        for is_u8 in [false, true] {
            let code = call_array_reader(is_u8, None);
            assert!(
                !code.contains("raise ValueError"),
                "dynamic-length path must not raise ValueError, got:\n{code}"
            );
        }
    }

    fn call_object_array_reader(length: Option<usize>) -> String {
        let mut fields = IndexMap::new();
        fields.insert("x".to_string(), SchemaType::Type(TypeToken::I32));
        read_field(
            "frames",
            &array_of(
                SchemaType::Object(ObjectSchema {
                    kind: ObjectKind::Object,
                    fields,
                    optional: false,
                }),
                length,
            ),
        )
    }

    #[test]
    fn object_array_reader_emits_length_check() {
        let code = call_object_array_reader(Some(3));
        assert!(
            code.contains("raise ValueError"),
            "fixed-length path must raise ValueError, got:\n{code}"
        );
        assert!(
            code.contains("expected 3"),
            "error message must mention expected length, got:\n{code}"
        );
    }

    #[test]
    fn object_array_reader_no_length_check_when_dynamic() {
        let code = call_object_array_reader(None);
        assert!(
            !code.contains("raise ValueError"),
            "dynamic-length path must not raise ValueError, got:\n{code}"
        );
    }

    /// The items of the array in the full-reader test: a scalar and a nested
    /// struct, so both reader shapes run inside the element loop.
    fn object_items_format() -> MessageFormat {
        serde_json5::from_str(
            r#"{
                frames: {
                    $type: "array",
                    $items: {
                        $type: "object",
                        x: "i32",
                        pose: { $type: "object", yaw: "f64" }
                    }
                }
            }"#,
        )
        .unwrap()
    }

    /// The object-array reader in full: one loop reads each element's
    /// fields, nested struct included, into the item dataclass, every
    /// temporary numbered in reading order.
    #[test]
    fn object_array_reader_reads_each_element_into_its_item_class() {
        let format = object_items_format();
        let code = read_field("frames", &format.0["frames"]);
        assert_eq!(
            code,
            [
                "list_0 = reader.frames",
                "frames_1 = []",
                "for elem_2 in list_0:",
                "    x_3 = elem_2.x",
                "    reader_4 = elem_2.pose",
                "    yaw_5 = reader_4.yaw",
                "    pose_6 = TestFramesItemPose(yaw=yaw_5)",
                "    frames_1.append(TestFramesItem(x=x_3, pose=pose_6))",
            ]
            .join("\n")
        );
    }
}
