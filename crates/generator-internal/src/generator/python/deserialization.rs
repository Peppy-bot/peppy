use super::PythonSchemaInfo;
use super::code_builder::PythonCodeBuilder;
use super::identifiers::{is_python_keyword, sanitize_python_identifier};
use super::serialization::capnp_field_name;
use crate::generator::naming::to_camel_case;
use config::node::{MessageFormat, SchemaType, TypeToken};
use indexmap::IndexMap;

/// Returns `true` when the Cap'n Proto encoding stores this type as a pointer
/// (i.e. it is possible to detect "not set" via `_has()`).
fn is_capnp_pointer_type(schema: &SchemaType) -> bool {
    match schema {
        SchemaType::Type(TypeToken::String | TypeToken::Time) => true,
        SchemaType::Primitive(prim) => {
            matches!(prim.kind, TypeToken::String | TypeToken::Time)
        }
        SchemaType::Array(_) | SchemaType::Object(_) => true,
        _ => false,
    }
}

fn capnp_read_expr(reader_var: &str, capnp_name: &str) -> String {
    if is_python_keyword(capnp_name) {
        format!("getattr({reader_var}, \"{capnp_name}\")")
    } else {
        format!("{reader_var}.{capnp_name}")
    }
}

/// Generates a single field reader statement, dispatching on the schema type.
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
    let var = match schema {
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
            let is_u8 = matches!(array.items.as_ref().as_type_token(), Some(TypeToken::U8));
            generate_array_reader(builder, reader_var, field_name, is_u8, counter)
        }
        SchemaType::Object(object) => generate_object_reader(
            builder,
            reader_var,
            field_name,
            &object.fields,
            struct_prefix,
            counter,
        ),
    };

    // For optional pointer-typed fields, override the value with None when the
    // Cap'n Proto field was never set (null pointer).
    if schema.is_optional() && is_capnp_pointer_type(schema) {
        let capnp_name = capnp_field_name(field_name);
        builder.line(&format!("if not {reader_var}._has(\"{capnp_name}\"):"));
        builder.indent();
        builder.line(&format!("{var} = None"));
        builder.dedent();
    }

    var
}

/// Generates a complete `deserialize_payload` helper function that reads a Cap'n Proto
/// payload and constructs a `Message` dataclass.
///
/// Emits:
/// ```python
/// def deserialize_payload(payload):
///     with capnp_module_expr.StructName.from_bytes(payload) as capnp_msg:
///         ...field reads...
///         return Message(field1=var1, ...)
/// ```
pub fn build_deserialize_fn(
    builder: &mut PythonCodeBuilder,
    schema_info: &PythonSchemaInfo,
    format: &MessageFormat,
    struct_prefix: &str,
    capnp_module_expr: &str,
    fn_name: &str,
) {
    builder.blank_line();
    builder.line(&format!(
        "def {fn_name}(payload: bytes) -> {struct_prefix}:"
    ));
    builder.indent();
    builder.line(&format!(
        "with {capnp_module_expr}.{}.from_bytes(payload) as capnp_msg:",
        schema_info.struct_name
    ));
    builder.indent();

    let field_bindings = deserialize_format_fields(builder, "capnp_msg", format, struct_prefix);

    let kwargs: Vec<String> = field_bindings
        .iter()
        .map(|(name, var)| format!("{name}={var}"))
        .collect();
    let kwargs_str = kwargs.join(", ");
    builder.line(&format!("return {struct_prefix}({kwargs_str})"));

    builder.dedent();
    builder.dedent();
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
    let mut field_bindings = Vec::new();

    for (field_name, schema) in &format.0 {
        let python_name = sanitize_python_identifier(field_name);
        let var_name = generate_field_reader_statements(
            builder,
            reader_var,
            field_name,
            schema,
            struct_prefix,
            &mut counter,
        );
        field_bindings.push((python_name, var_name));
    }

    field_bindings
}

fn generate_primitive_reader(
    builder: &mut PythonCodeBuilder,
    reader_var: &str,
    field_name: &str,
    counter: &mut u32,
) -> String {
    let capnp_name = capnp_field_name(field_name);
    let python_name = sanitize_python_identifier(field_name);
    let idx = *counter;
    *counter += 1;
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
    let capnp_name = capnp_field_name(field_name);
    let python_name = sanitize_python_identifier(field_name);
    let ts_idx = *counter;
    *counter += 1;
    let ts_var = format!("timestamp_{ts_idx}");
    let result_idx = *counter;
    *counter += 1;
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

fn generate_array_reader(
    builder: &mut PythonCodeBuilder,
    reader_var: &str,
    field_name: &str,
    is_u8: bool,
    counter: &mut u32,
) -> String {
    let capnp_name = capnp_field_name(field_name);
    let python_name = sanitize_python_identifier(field_name);
    let idx = *counter;
    *counter += 1;
    let var = format!("{python_name}_{idx}");
    if is_u8 {
        builder.line(&format!(
            "{var} = bytes({})",
            capnp_read_expr(reader_var, &capnp_name)
        ));
    } else {
        builder.line(&format!(
            "{var} = list({})",
            capnp_read_expr(reader_var, &capnp_name)
        ));
    }
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
    let capnp_name = capnp_field_name(field_name);
    let python_name = sanitize_python_identifier(field_name);
    let nested_prefix = format!("{struct_prefix}{}", to_camel_case(field_name));

    let reader_idx = *counter;
    *counter += 1;
    let sub_reader = format!("reader_{reader_idx}");
    builder.line(&format!(
        "{sub_reader} = {}",
        capnp_read_expr(reader_var, &capnp_name)
    ));

    let mut nested_bindings = Vec::new();
    for (nested_name, nested_schema) in fields {
        let nested_var = generate_field_reader_statements(
            builder,
            &sub_reader,
            nested_name,
            nested_schema,
            &nested_prefix,
            counter,
        );
        nested_bindings.push((sanitize_python_identifier(nested_name), nested_var));
    }

    let result_idx = *counter;
    *counter += 1;
    let result_var = format!("{python_name}_{result_idx}");
    let kwargs: Vec<String> = nested_bindings
        .iter()
        .map(|(name, var)| format!("{name}={var}"))
        .collect();
    let kwargs_str = kwargs.join(", ");
    builder.line(&format!("{result_var} = {nested_prefix}({kwargs_str})"));
    result_var
}
