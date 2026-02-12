use super::code_builder::PythonCodeBuilder;
use super::identifiers::{is_python_keyword, sanitize_python_identifier};
use config::node::{MessageFormat, SchemaType, TypeToken};

/// Converts a field name to camelCase for Cap'n Proto field access in pycapnp.
///
/// This mirrors the private `config::encoding::sanitize_field_name` function.
/// E.g., `frame_id` → `frameId`, `sample_rate` → `sampleRate`.
pub fn capnp_field_name(input: &str) -> String {
    // Build PascalCase by splitting on non-alphanumeric characters
    let mut pascal = String::new();
    for segment in input
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
    {
        let mut chars = segment.chars();
        if let Some(first) = chars.next() {
            pascal.push(first.to_ascii_uppercase());
            for ch in chars {
                pascal.push(ch.to_ascii_lowercase());
            }
        }
    }

    if pascal.is_empty() {
        return "_field".to_string();
    }

    // Lowercase the first character for camelCase
    let mut camel = String::with_capacity(pascal.len());
    let mut chars = pascal.chars();
    if let Some(first) = chars.next() {
        camel.push(first.to_ascii_lowercase());
        camel.extend(chars);
    }

    if camel.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
        camel.insert(0, '_');
    }

    camel
}

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
            python_name.clone()
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
    let capnp_name = capnp_field_name(field_name);
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
        SchemaType::Array(_) => {
            builder.line(&capnp_assignment_stmt(builder_var, &capnp_name, value_expr));
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
    let ts_builder = format!("builder_{}", {
        let i = *counter;
        *counter += 1;
        i
    });
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

    #[test]
    fn camel_case_conversion() {
        assert_eq!(capnp_field_name("frame_id"), "frameId");
        assert_eq!(capnp_field_name("sample_rate"), "sampleRate");
        assert_eq!(capnp_field_name("encoding"), "encoding");
        assert_eq!(capnp_field_name("x"), "x");
        assert_eq!(capnp_field_name("return_type"), "returnType");
    }
}
