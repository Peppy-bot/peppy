use super::code_builder::PythonCodeBuilder;
use super::identifiers::sanitize_python_identifier;
use super::type_mapping::type_name_to_python;
use crate::error::Result;
use crate::generator::naming::to_camel_case;
use crate::generator::rust::generate_parameters_struct as validate_parameters;
use config::AnyType;

/// Generates a Python `parameters.py` file from node parameters configuration.
///
/// Validates field names using the shared validator, then generates `@dataclass`
/// definitions for all parameter groups.
pub fn generate_python_parameters(parameters: &config::NodeArguments) -> Result<String> {
    // Validate field names using the shared Rust validator
    validate_parameters(parameters)?;

    let mut builder = PythonCodeBuilder::new();
    builder.line("from dataclasses import dataclass");
    builder.blank_line();

    // Collect and emit nested classes first (dependency order)
    let mut nested_builders = Vec::new();
    let mut main_fields: Vec<(String, String)> = Vec::new();

    for (field_name, type_spec) in parameters {
        match type_spec {
            AnyType::Object(_) => {
                let struct_name = to_camel_case(field_name);
                let field_ident = sanitize_python_identifier(field_name);
                main_fields.push((field_ident, struct_name.clone()));
                generate_nested_parameter_class(type_spec, &struct_name, &mut nested_builders);
            }
            AnyType::String(type_name) => {
                let field_ident = sanitize_python_identifier(field_name);
                let python_type = type_name_to_python(type_name);
                main_fields.push((field_ident, python_type.to_string()));
            }
            _ => {}
        }
    }

    // Emit nested classes
    for nested in &nested_builders {
        for line in nested.lines() {
            builder.line(line);
        }
        builder.blank_line();
    }

    // Emit main Parameters class
    let field_refs: Vec<(&str, &str)> = main_fields
        .iter()
        .map(|(name, ty)| (name.as_str(), ty.as_str()))
        .collect();
    builder.dataclass("Parameters", &field_refs);

    Ok(builder.build())
}

fn generate_nested_parameter_class(
    type_spec: &AnyType,
    class_name: &str,
    output: &mut Vec<String>,
) {
    if let AnyType::Object(fields) = type_spec {
        // Recurse into nested objects first (so they're defined before referenced)
        for (field_name, field_spec) in fields {
            if let AnyType::Object(_) = field_spec {
                let nested_name = to_camel_case(field_name);
                generate_nested_parameter_class(field_spec, &nested_name, output);
            }
        }

        let mut builder = PythonCodeBuilder::new();
        builder.line("@dataclass");
        builder.line(&format!("class {class_name}:"));
        builder.indent();
        for (field_name, field_spec) in fields {
            let field_ident = sanitize_python_identifier(field_name);
            match field_spec {
                AnyType::String(type_name) => {
                    let py_type = type_name_to_python(type_name);
                    builder.line(&format!("{field_ident}: {py_type}"));
                }
                AnyType::Object(_) => {
                    let nested_name = to_camel_case(field_name);
                    builder.line(&format!("{field_ident}: {nested_name}"));
                }
                _ => {}
            }
        }
        builder.dedent();
        output.push(builder.build());
    }
}
