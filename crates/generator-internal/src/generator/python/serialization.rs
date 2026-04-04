use super::code_builder::PythonCodeBuilder;
use super::identifiers::{is_python_keyword, sanitize_python_identifier};
use crate::generator::naming::sanitize_capnp_field_name;
use config::node::{MessageFormat, SchemaType, TypeToken};

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
        SchemaType::Array(array) => match array.items.as_ref() {
            SchemaType::Object(object) => {
                let idx = *counter;
                *counter += 1;
                let list_builder = format!("list_{idx}");
                let length_expr = if let Some(len) = array.length {
                    builder.line(&format!("if len({value_expr}) != {len}:"));
                    builder.indent();
                    builder.line(&format!(
                        "raise ValueError(\"invalid fixed list length for field '{field_name}': expected {len}, got \" + str(len({value_expr})))"
                    ));
                    builder.dedent();
                    format!("{len}")
                } else {
                    format!("len({value_expr})")
                };
                builder.line(&format!(
                    "{list_builder} = {builder_var}.init(\"{capnp_name}\", {length_expr})"
                ));
                let loop_idx = format!("i_{idx}");
                let loop_elem = format!("elem_{idx}");
                builder.line(&format!(
                    "for {loop_idx}, {loop_elem} in enumerate({value_expr}):"
                ));
                builder.indent();
                let elem_builder = format!("eb_{idx}");
                builder.line(&format!("{elem_builder} = {list_builder}[{loop_idx}]"));
                for (nested_name, nested_schema) in &object.fields {
                    let nested_python = sanitize_python_identifier(nested_name);
                    let nested_value = format!("{loop_elem}.{nested_python}");
                    emit_field_assignment(
                        builder,
                        &elem_builder,
                        nested_name,
                        nested_schema,
                        &nested_value,
                        counter,
                    );
                }
                builder.dedent();
            }
            _ => {
                builder.line(&capnp_assignment_stmt(builder_var, &capnp_name, value_expr));
            }
        },
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

#[cfg(test)]
mod tests {
    use super::*;
    use config::node::{ArrayKind, ArraySchema, ObjectKind, ObjectSchema};
    use indexmap::IndexMap;

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
