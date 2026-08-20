use super::code_builder::{PythonCodeBuilder, container_name, emit_fixed_length_check};
use super::identifiers::{is_python_keyword, sanitize_python_identifier};
use crate::generator::naming::sanitize_capnp_field_name;
use config::node::{MessageFormat, SchemaType, TypeToken};
use indexmap::IndexMap;

/// Emits pycapnp field assignment statements for all fields in a `MessageFormat`.
///
/// `builder_var` is the pycapnp message variable (e.g., `"capnp_msg"` or `"header_builder"`).
/// `param_prefix` is the Python expression prefix for accessing parameter values
/// (empty string for top-level fields, e.g., `"header"` for nested fields under `header`).
/// `counter` provides unique suffixes for temporary variable names.
pub fn emit_capnp_assignments(
    builder: &mut PythonCodeBuilder,
    builder_var: &str,
    format: &MessageFormat,
    param_prefix: &str,
    counter: &mut u32,
) {
    for (field_name, schema) in &format.0 {
        let python_name = sanitize_python_identifier(field_name);
        let value_expr = if param_prefix.is_empty() {
            python_name
        } else {
            format!("{param_prefix}.{python_name}")
        };

        emit_field_assignment(
            builder,
            builder_var,
            field_name,
            schema,
            &value_expr,
            counter,
        );
    }
}

fn capnp_assignment_stmt(builder_var: &str, capnp_name: &str, value_expr: &str) -> String {
    if is_python_keyword(capnp_name) {
        format!("setattr({builder_var}, \"{capnp_name}\", {value_expr})")
    } else {
        format!("{builder_var}.{capnp_name} = {value_expr}")
    }
}

/// Emits a single pycapnp field assignment, dispatching on the schema type.
fn emit_field_assignment(
    builder: &mut PythonCodeBuilder,
    builder_var: &str,
    field_name: &str,
    schema: &SchemaType,
    value_expr: &str,
    counter: &mut u32,
) {
    let capnp_name = sanitize_capnp_field_name(field_name);
    let optional = schema.is_optional();

    if optional {
        builder.line(&format!("if {value_expr} is not None:"));
        builder.indent();
    }

    match schema {
        SchemaType::Type(TypeToken::Time) => {
            emit_time_assignment(builder, builder_var, &capnp_name, value_expr, counter);
        }
        SchemaType::Primitive(prim) if prim.kind == TypeToken::Time => {
            emit_time_assignment(builder, builder_var, &capnp_name, value_expr, counter);
        }
        SchemaType::Type(_) | SchemaType::Primitive(_) => {
            builder.line(&capnp_assignment_stmt(builder_var, &capnp_name, value_expr));
        }
        SchemaType::Array(array) => {
            let items = array.items.as_ref();
            // A fixed-length array is checked before anything is written, so
            // a caller passing any other count fails here rather than putting
            // a wrong-sized container on the wire. One check for both item
            // shapes, and the same message the reader raises.
            if let Some(len) = array.length {
                emit_fixed_length_check(
                    builder,
                    value_expr,
                    field_name,
                    container_name(items),
                    len,
                );
            }
            match items {
                SchemaType::Object(object) => emit_object_array_assignment(
                    builder,
                    builder_var,
                    &capnp_name,
                    value_expr,
                    &object.fields,
                    array.length,
                    counter,
                ),
                _ => builder.line(&capnp_assignment_stmt(builder_var, &capnp_name, value_expr)),
            }
        }
        SchemaType::Object(object) => {
            let idx = *counter;
            *counter += 1;
            let sub_builder = format!("builder_{idx}");
            builder.line(&format!(
                "{sub_builder} = {builder_var}.init(\"{capnp_name}\")"
            ));
            // Recurse into nested object fields
            for (nested_name, nested_schema) in &object.fields {
                let nested_python = sanitize_python_identifier(nested_name);
                let nested_value = format!("{value_expr}.{nested_python}");
                emit_field_assignment(
                    builder,
                    &sub_builder,
                    nested_name,
                    nested_schema,
                    &nested_value,
                    counter,
                );
            }
        }
    }

    if optional {
        builder.dedent();
    }
}

/// Emits timestamp conversion and assignment for a `Time` field.
fn emit_time_assignment(
    builder: &mut PythonCodeBuilder,
    builder_var: &str,
    capnp_name: &str,
    value_expr: &str,
    counter: &mut u32,
) {
    let idx = *counter;
    *counter += 1;
    let ts_var = format!("timestamp_{idx}");
    let idx2 = *counter;
    *counter += 1;
    let ts_builder = format!("builder_{idx2}");
    builder.line(&format!(
        "{ts_var} = peppylib.encoding.convert_time({value_expr})"
    ));
    builder.line(&format!(
        "{ts_builder} = {builder_var}.init(\"{capnp_name}\")"
    ));
    builder.line(&format!("{ts_builder}.sec = {ts_var}.sec"));
    builder.line(&format!("{ts_builder}.nsec = {ts_var}.nsec"));
}

/// Writes an array of objects: sizes the Cap'n Proto list, then assigns each
/// item's fields into the element builder. The reader's counterpart is
/// `generate_object_array_reader`.
fn emit_object_array_assignment(
    builder: &mut PythonCodeBuilder,
    builder_var: &str,
    capnp_name: &str,
    value_expr: &str,
    fields: &IndexMap<String, SchemaType>,
    length: Option<usize>,
    counter: &mut u32,
) {
    let idx = *counter;
    *counter += 1;
    let list_builder = format!("list_{idx}");
    let length_expr = match length {
        Some(len) => format!("{len}"),
        None => format!("len({value_expr})"),
    };
    builder.line(&format!(
        "{list_builder} = {builder_var}.init(\"{capnp_name}\", {length_expr})"
    ));
    let loop_idx = format!("i_{idx}");
    let loop_elem = format!("elem_{idx}");
    builder.block(
        &format!("for {loop_idx}, {loop_elem} in enumerate({value_expr}):"),
        |b| {
            let elem_builder = format!("eb_{idx}");
            b.line(&format!("{elem_builder} = {list_builder}[{loop_idx}]"));
            for (nested_name, nested_schema) in fields {
                let nested_python = sanitize_python_identifier(nested_name);
                let nested_value = format!("{loop_elem}.{nested_python}");
                emit_field_assignment(
                    b,
                    &elem_builder,
                    nested_name,
                    nested_schema,
                    &nested_value,
                    counter,
                );
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::node::{ArrayKind, ArraySchema, ObjectKind, ObjectSchema};

    fn emit_primitive_array(is_u8: bool, length: Option<usize>) -> String {
        let item = if is_u8 { TypeToken::U8 } else { TypeToken::F64 };
        let schema = SchemaType::Array(ArraySchema {
            kind: ArrayKind::Array,
            items: Box::new(SchemaType::Type(item)),
            length,
            optional: false,
        });
        let mut builder = PythonCodeBuilder::new();
        let mut counter = 0u32;
        emit_field_assignment(
            &mut builder,
            "msg",
            "joint_positions",
            &schema,
            "value.joint_positions",
            &mut counter,
        );
        builder.build()
    }

    /// A fixed-length list of primitives is checked before the assignment,
    /// with the same message the reader raises, so a wrong-sized list never
    /// reaches the wire.
    #[test]
    fn primitive_array_serialization_checks_fixed_length() {
        let code = emit_primitive_array(false, Some(7));
        assert!(
            code.contains("if len(value.joint_positions) != 7:")
                && code.contains(
                    "invalid fixed list length for field 'joint_positions': expected 7, got "
                )
                && code.contains("msg.jointPositions = value.joint_positions"),
            "fixed-length path must check then assign, got:\n{code}"
        );
        let code = emit_primitive_array(true, Some(4));
        assert!(
            code.contains(
                "invalid fixed bytes length for field 'joint_positions': expected 4, got "
            ),
            "fixed u8 arrays are bytes, got:\n{code}"
        );
    }

    #[test]
    fn primitive_array_serialization_skips_check_when_dynamic() {
        for is_u8 in [false, true] {
            let code = emit_primitive_array(is_u8, None);
            assert!(
                !code.contains("raise ValueError")
                    && code.contains("msg.jointPositions = value.joint_positions"),
                "dynamic-length path assigns without a check, got:\n{code}"
            );
        }
    }

    fn emit_object_array(length: Option<usize>) -> String {
        let mut fields = IndexMap::new();
        fields.insert("x".to_string(), SchemaType::Type(TypeToken::I32));

        let schema = SchemaType::Array(ArraySchema {
            kind: ArrayKind::Array,
            items: Box::new(SchemaType::Object(ObjectSchema {
                kind: ObjectKind::Object,
                fields,
                optional: false,
            })),
            length,
            optional: false,
        });

        let mut builder = PythonCodeBuilder::new();
        let mut counter = 0u32;
        emit_field_assignment(
            &mut builder,
            "msg",
            "frames",
            &schema,
            "self.frames",
            &mut counter,
        );
        builder.build()
    }

    #[test]
    fn object_array_serialization_uses_fixed_length_and_validates() {
        let code = emit_object_array(Some(4));
        assert!(
            code.contains("raise ValueError"),
            "fixed-length path must validate with ValueError, got:\n{code}"
        );
        assert!(
            code.contains("!= 4"),
            "must check len against declared length 4, got:\n{code}"
        );
        assert!(
            code.contains("init(\"frames\", 4)"),
            "must pass fixed literal 4 to init, got:\n{code}"
        );
        assert!(
            !code.contains("init(\"frames\", len("),
            "fixed-length path must not use len() in init, got:\n{code}"
        );
    }

    #[test]
    fn object_array_serialization_uses_runtime_len_when_dynamic() {
        let code = emit_object_array(None);
        assert!(
            !code.contains("raise ValueError"),
            "dynamic-length path must not raise ValueError, got:\n{code}"
        );
        assert!(
            code.contains("init(\"frames\", len(self.frames))"),
            "dynamic-length path must use len(value_expr) in init, got:\n{code}"
        );
    }
}
