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
                builder.line(&format!(
                    "{list_builder} = {builder_var}.init(\"{capnp_name}\", len({value_expr}))"
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
